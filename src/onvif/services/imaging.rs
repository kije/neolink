//! ONVIF Imaging service.
//!
//! The Baichuan protocol exposes almost nothing of a camera's ISP — there is no
//! brightness, contrast or saturation message — so this service deliberately
//! carries only the two controls that do exist behind it:
//!
//! * the **IR cut filter**, via the `LedState` message that drives the infrared
//!   illuminator, and
//! * the **focus motor**, via `StartZoomFocus`.
//!
//! A camera with neither gets no Imaging service address at all (see
//! `CameraCapabilities::imaging`), so a client never sees a control that cannot
//! work.

use anyhow::Result;
use neolink_core::bc_protocol::{BcCamera, LightState};

use crate::onvif::capabilities::{capabilities, CameraCapabilities};
use crate::onvif::services::device::FaultBody;
use crate::onvif::services::media::read_first_text_element;
use crate::onvif::soap::{wrap_envelope, FaultCode, NS_ALL};
use crate::onvif::state::CameraEntry;

/// How the ONVIF `IrCutFilter` mode maps onto Reolink's IR illuminator state.
///
/// **The mapping is inverted, and deliberately so.** ONVIF describes the
/// *filter*: `ON` means the IR cut filter is in place, blocking infrared — that
/// is daylight/colour mode with the illuminator irrelevant. Reolink's
/// `ledState.state` describes the *illuminator*: `open` means the IR LEDs are
/// lit, which is night mode, i.e. the filter is out of the way.
///
/// So ONVIF `ON` is Reolink `close`, and ONVIF `OFF` is Reolink `open`. Wiring
/// them together naively by name would give every client a day/night switch
/// that does the opposite of what it says.
fn ir_cut_filter_to_led(mode: &str) -> Option<LightState> {
    match mode.trim().to_ascii_uppercase().as_str() {
        // Filter engaged → daylight → illuminator off.
        "ON" => Some(LightState::Off),
        // Filter retracted → night → illuminator on.
        "OFF" => Some(LightState::On),
        "AUTO" => Some(LightState::Auto),
        _ => None,
    }
}

/// The inverse of [`ir_cut_filter_to_led`], for reporting current state.
fn led_to_ir_cut_filter(state: &str) -> &'static str {
    match state.trim().to_ascii_lowercase().as_str() {
        "open" => "OFF",
        "close" => "ON",
        // "auto", and anything a firmware invents that we don't recognise: the
        // camera is deciding for itself, which is exactly what AUTO means.
        _ => "AUTO",
    }
}

/// Accept either the video *source* token or the video source *configuration*
/// token. Both name the same sensor here, and clients are inconsistent about
/// which one they send to Imaging.
fn token_matches(cam: &CameraEntry, token: &str) -> bool {
    token == format!("vsrc_{}", cam.name) || token == format!("vs_{}", cam.name)
}

fn required_token(cam: &CameraEntry, body_xml: &str) -> Result<(), FaultBody> {
    let token = read_first_text_element(body_xml, "VideoSourceToken").ok_or_else(|| FaultBody {
        code: FaultCode::InvalidArgs,
        reason: "Missing VideoSourceToken".to_string(),
    })?;
    if !token_matches(cam, &token) {
        return Err(FaultBody {
            code: FaultCode::InvalidArgs,
            reason: format!("Unknown VideoSourceToken '{token}'"),
        });
    }
    Ok(())
}

pub(crate) async fn dispatch(
    cam: &CameraEntry,
    action: &str,
    body_xml: &str,
) -> Result<String, FaultBody> {
    let caps = capabilities(cam).await;

    let body = match action {
        "GetServiceCapabilities" => "<timg:GetServiceCapabilitiesResponse>\
<timg:Capabilities ImageStabilization=\"false\" Presets=\"false\" AdaptablePreset=\"false\"/>\
</timg:GetServiceCapabilitiesResponse>"
            .to_string(),
        "GetImagingSettings" => {
            required_token(cam, body_xml)?;
            let mut settings = String::new();
            if caps.focus {
                // The camera has no autofocus message, so every focus move a
                // client makes is a manual one.
                settings
                    .push_str("<tt:Focus><tt:AutoFocusMode>MANUAL</tt:AutoFocusMode></tt:Focus>");
            }
            if caps.led_ctrl {
                let state = read_led_state(cam)
                    .await
                    .unwrap_or_else(|| "auto".to_string());
                settings.push_str(&format!(
                    "<tt:IrCutFilter>{}</tt:IrCutFilter>",
                    led_to_ir_cut_filter(&state)
                ));
            }
            format!(
                "<timg:GetImagingSettingsResponse><timg:ImagingSettings>{settings}\
</timg:ImagingSettings></timg:GetImagingSettingsResponse>"
            )
        }
        "SetImagingSettings" => {
            required_token(cam, body_xml)?;
            // The only settable field. A request that carries nothing we can
            // act on is accepted rather than faulted: clients routinely send
            // the whole settings block back, and rejecting it because it also
            // mentioned brightness would break the parts that do work.
            if let Some(mode) = read_first_text_element(body_xml, "IrCutFilter") {
                if !caps.led_ctrl {
                    return Err(FaultBody {
                        code: FaultCode::InvalidArgs,
                        reason: "This camera has no IR cut filter control".to_string(),
                    });
                }
                let Some(_) = ir_cut_filter_to_led(&mode) else {
                    return Err(FaultBody {
                        code: FaultCode::InvalidArgs,
                        reason: format!("IrCutFilter must be ON, OFF or AUTO, got '{mode}'"),
                    });
                };
                cam.run(move |c: &BcCamera| {
                    // Rebuilt per attempt: `LightState` is not `Clone` and
                    // `run` may retry.
                    let state = ir_cut_filter_to_led(&mode).expect("validated above");
                    Box::pin(async move { Ok(c.irled_light_set(state).await?) })
                })
                .await
                .map_err(other_fault)?;
            }
            "<timg:SetImagingSettingsResponse/>".to_string()
        }
        "GetOptions" => {
            required_token(cam, body_xml)?;
            render_options(cam, &caps).await
        }
        "GetMoveOptions" => {
            required_token(cam, body_xml)?;
            if !caps.focus {
                return Err(FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "This camera has no focus motor".to_string(),
                });
            }
            let (min, max) = focus_range(cam).await.ok_or_else(|| FaultBody {
                code: FaultCode::Other,
                reason: "Could not read the focus range from the camera".to_string(),
            })?;
            // Continuous is deliberately absent: the camera has no "stop the
            // lens" message, so a continuous move would run to the mechanical
            // limit with no way to halt it. Absolute and Relative both resolve
            // to a single bounded `StartZoomFocus`.
            format!(
                "<timg:GetMoveOptionsResponse><timg:MoveOptions>\
<tt:Absolute><tt:Position><tt:Min>{min}</tt:Min><tt:Max>{max}</tt:Max></tt:Position></tt:Absolute>\
<tt:Relative><tt:Distance><tt:Min>-{span}</tt:Min><tt:Max>{span}</tt:Max></tt:Distance></tt:Relative>\
</timg:MoveOptions></timg:GetMoveOptionsResponse>",
                span = max.saturating_sub(min),
            )
        }
        "Move" => {
            required_token(cam, body_xml)?;
            if !caps.focus {
                return Err(FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "This camera has no focus motor".to_string(),
                });
            }
            let target = focus_target(cam, body_xml).await?;
            cam.run(move |c: &BcCamera| Box::pin(async move { Ok(c.focus_to(target).await?) }))
                .await
                .map_err(other_fault)?;
            "<timg:MoveResponse/>".to_string()
        }
        "Stop" => {
            required_token(cam, body_xml)?;
            // Every move this service issues is a bounded absolute move that
            // the camera completes on its own, so there is never anything in
            // flight to stop. Answering cleanly is honest here — and a fault
            // would make clients that always send Stop after a move look
            // broken.
            "<timg:StopResponse/>".to_string()
        }
        "GetStatus" => {
            required_token(cam, body_xml)?;
            let pos = if caps.focus {
                focus_position(cam).await
            } else {
                None
            };
            let focus_xml = match pos {
                Some(p) => format!(
                    "<tt:FocusStatus20><tt:Position>{p}</tt:Position>\
<tt:MoveStatus>IDLE</tt:MoveStatus></tt:FocusStatus20>"
                ),
                None => String::new(),
            };
            format!(
                "<timg:GetStatusResponse><timg:Status>{focus_xml}</timg:Status>\
</timg:GetStatusResponse>"
            )
        }
        other => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!("Imaging action '{other}' not supported"),
            });
        }
    };
    Ok(wrap_envelope(&body, NS_ALL))
}

async fn render_options(cam: &CameraEntry, caps: &CameraCapabilities) -> String {
    let mut out = String::new();
    // Schema order: Focus precedes IrCutFilterModes.
    if caps.focus {
        if let Some((min, max)) = focus_range(cam).await {
            out.push_str(&format!(
                "<tt:Focus><tt:AutoFocusModes>MANUAL</tt:AutoFocusModes>\
<tt:DefaultSpeed><tt:Min>1</tt:Min><tt:Max>1</tt:Max></tt:DefaultSpeed>\
<tt:NearLimit><tt:Min>{min}</tt:Min><tt:Max>{max}</tt:Max></tt:NearLimit>\
<tt:FarLimit><tt:Min>{min}</tt:Min><tt:Max>{max}</tt:Max></tt:FarLimit>\
</tt:Focus>"
            ));
        }
    }
    if caps.led_ctrl {
        out.push_str(
            "<tt:IrCutFilterModes>ON</tt:IrCutFilterModes>\
<tt:IrCutFilterModes>OFF</tt:IrCutFilterModes>\
<tt:IrCutFilterModes>AUTO</tt:IrCutFilterModes>",
        );
    }
    format!("<timg:GetOptionsResponse><timg:ImagingOptions>{out}</timg:ImagingOptions></timg:GetOptionsResponse>")
}

/// Work out the absolute focus position a `Move` request is asking for.
///
/// Absolute moves carry the position directly; relative ones are resolved
/// against the current position here, because the camera only understands
/// "go to this position".
async fn focus_target(cam: &CameraEntry, body_xml: &str) -> Result<u32, FaultBody> {
    let invalid = |reason: String| FaultBody {
        code: FaultCode::InvalidArgs,
        reason,
    };
    if body_xml.contains("Continuous") {
        return Err(invalid(
            "Continuous focus moves are not supported: the camera has no way to \
             stop the lens once it is moving"
                .to_string(),
        ));
    }
    if let Some(pos) = read_first_text_element(body_xml, "Position") {
        let pos: f64 = pos
            .trim()
            .parse()
            .map_err(|_| invalid(format!("Position '{pos}' is not a number")))?;
        return Ok(pos.max(0.0) as u32);
    }
    if let Some(distance) = read_first_text_element(body_xml, "Distance") {
        let distance: f64 = distance
            .trim()
            .parse()
            .map_err(|_| invalid(format!("Distance '{distance}' is not a number")))?;
        let current = focus_position(cam).await.ok_or_else(|| FaultBody {
            code: FaultCode::Other,
            reason: "Could not read the current focus position from the camera".to_string(),
        })?;
        // Saturating rather than wrapping: a client asking to go further near
        // than the lens can reach gets the near limit, and `focus_to` clamps
        // to the real range anyway.
        let target = (current as f64 + distance).max(0.0);
        return Ok(target as u32);
    }
    Err(invalid(
        "Move needs an Absolute Position or a Relative Distance".to_string(),
    ))
}

async fn read_led_state(cam: &CameraEntry) -> Option<String> {
    cam.run(|c: &BcCamera| Box::pin(async move { Ok(c.get_ledstate().await?) }))
        .await
        .ok()
        .map(|s| s.state)
}

async fn focus_range(cam: &CameraEntry) -> Option<(u32, u32)> {
    cam.run(|c: &BcCamera| Box::pin(async move { Ok(c.get_zoom().await?) }))
        .await
        .ok()
        .map(|zf| (zf.focus.min_pos, zf.focus.max_pos))
}

async fn focus_position(cam: &CameraEntry) -> Option<u32> {
    cam.run(|c: &BcCamera| Box::pin(async move { Ok(c.get_zoom().await?) }))
        .await
        .ok()
        .map(|zf| zf.focus.cur_pos)
}

fn other_fault(e: anyhow::Error) -> FaultBody {
    FaultBody {
        code: FaultCode::Other,
        reason: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of this mapping. ONVIF names the filter, Reolink names
    /// the illuminator, and they mean opposite things — a client asking for
    /// `IrCutFilter=ON` wants daylight mode, which is the IR LEDs *off*.
    #[test]
    fn the_ir_cut_filter_mapping_is_inverted() {
        assert!(matches!(ir_cut_filter_to_led("ON"), Some(LightState::Off)));
        assert!(matches!(ir_cut_filter_to_led("OFF"), Some(LightState::On)));
        assert!(matches!(
            ir_cut_filter_to_led("AUTO"),
            Some(LightState::Auto)
        ));
        assert!(ir_cut_filter_to_led("SOMETIMES").is_none());

        assert_eq!(led_to_ir_cut_filter("close"), "ON");
        assert_eq!(led_to_ir_cut_filter("open"), "OFF");
        assert_eq!(led_to_ir_cut_filter("auto"), "AUTO");
    }

    /// Case and whitespace vary between clients; neither should decide whether
    /// a camera flips to night mode.
    #[test]
    fn the_mapping_tolerates_client_formatting() {
        assert!(matches!(
            ir_cut_filter_to_led(" on "),
            Some(LightState::Off)
        ));
        assert!(matches!(
            ir_cut_filter_to_led("Auto"),
            Some(LightState::Auto)
        ));
        assert_eq!(led_to_ir_cut_filter(" OPEN "), "OFF");
    }

    /// A firmware value we have never seen is the camera deciding for itself,
    /// which is what AUTO reports — not a guess at ON or OFF.
    #[test]
    fn an_unknown_led_state_reports_auto() {
        assert_eq!(led_to_ir_cut_filter("halfopen"), "AUTO");
        assert_eq!(led_to_ir_cut_filter(""), "AUTO");
    }
}
