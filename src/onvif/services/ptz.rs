//! ONVIF PTZ service — the actual ONVIF→Reolink translator.
//!
//! For every PTZ command we receive from a VMS client (Home Assistant,
//! Frigate, BlueIris, ...) we call into the same `BcCamera` methods the CLI
//! and MQTT surfaces already use:
//!
//! - `send_ptz(Direction, speed)`           : continuous & timed PT moves
//! - `zoom_to(pos)` + `get_zoom()`           : absolute / relative zoom
//! - `get_ptz_preset()` / `moveto_ptz_preset` / `set_ptz_preset` : presets
//!
//! The Reolink protocol does not expose absolute pan/tilt coordinates, so
//! `AbsoluteMove{PanTilt}` returns the proper `NoAbsolutePTZSpace` fault. The
//! ONVIF spec explicitly permits this on continuous-only devices.
//!
//! Everything this service describes — the node's supported spaces, the
//! preset count, the configuration options — is filtered through the probed
//! [`CameraCapabilities`], so a fixed-lens camera does not advertise a zoom
//! space and a mount with no motor does not advertise a pan/tilt one.

use std::sync::Arc;

use anyhow::Result;
use neolink_core::bc_protocol::{BcCamera, Direction};
use quick_xml::events::Event;
use quick_xml::Reader;
use tokio::time::{sleep, Duration};

use crate::onvif::capabilities::{capabilities, CameraCapabilities};
use crate::onvif::services::device::FaultBody;
use crate::onvif::services::media::read_first_text_element;
use crate::onvif::soap::{wrap_envelope, xml_escape, FaultCode, NS_ALL};
use crate::onvif::state::{CameraEntry, OnvifState};

/// URIs of the PTZ coordinate spaces this bridge can implement.
const CONTINUOUS_PT_SPACE: &str =
    "http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace";
const CONTINUOUS_ZOOM_SPACE: &str =
    "http://www.onvif.org/ver10/tptz/ZoomSpaces/VelocityGenericSpace";
const ABSOLUTE_ZOOM_SPACE: &str = "http://www.onvif.org/ver10/tptz/ZoomSpaces/PositionGenericSpace";
const PT_SPEED_SPACE: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaces/GenericSpeedSpace";
const ZOOM_SPEED_SPACE: &str = "http://www.onvif.org/ver10/tptz/ZoomSpaces/ZoomGenericSpeedSpace";

/// Map a normalized [-1.0, 1.0] velocity magnitude to the Reolink `speed`
/// parameter (an f32, conventionally 1..=64; the CLI/MQTT default is 32).
fn onvif_to_reolink_speed(v: f32) -> f32 {
    let mag = v.abs().clamp(0.0, 1.0);
    (1.0 + mag * 63.0).round()
}

/// Choose a Reolink direction string from a velocity vector. Returns None when
/// both components are essentially zero.
///
/// When both axes carry meaningful velocity we emit the matching diagonal
/// (`leftUp`/`rightUp`/`leftDown`/`rightDown`). Otherwise we emit a pure
/// cardinal direction. Threshold of 0.05 avoids jitter triggering moves and
/// also acts as the diagonal-vs-cardinal decision boundary: if the smaller
/// axis is under threshold we treat it as zero.
fn pick_direction(x: f32, y: f32) -> Option<Direction> {
    const THRESHOLD: f32 = 0.05;
    let ax = x.abs();
    let ay = y.abs();
    if ax < THRESHOLD && ay < THRESHOLD {
        return None;
    }
    let has_x = ax >= THRESHOLD;
    let has_y = ay >= THRESHOLD;
    Some(match (has_x, has_y, x > 0.0, y > 0.0) {
        // Diagonals
        (true, true, true, true) => Direction::RightUp,
        (true, true, true, false) => Direction::RightDown,
        (true, true, false, true) => Direction::LeftUp,
        (true, true, false, false) => Direction::LeftDown,
        // Pure X
        (true, false, true, _) => Direction::Right,
        (true, false, false, _) => Direction::Left,
        // Pure Y
        (false, true, _, true) => Direction::Up,
        (false, true, _, false) => Direction::Down,
        // Unreachable — both flags can't be false here.
        _ => return None,
    })
}

#[derive(Default, Debug)]
struct Velocity {
    pan: f32,
    tilt: f32,
    zoom: f32,
}

/// Parse a `Velocity` (or `Translation` or `Position`) sub-element. ONVIF
/// expresses pan/tilt with attributes `x` and `y`, zoom with attribute `x`.
fn parse_velocity(xml: &str, wrapper: &str) -> Velocity {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_wrap = false;
    let mut v = Velocity::default();
    loop {
        let evt = reader.read_event();
        match evt {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = std::str::from_utf8(e.name().into_inner()).unwrap_or("");
                let local = name.rsplit(':').next().unwrap_or(name);
                if local == wrapper {
                    in_wrap = true;
                }
                if in_wrap && local == "PanTilt" {
                    for a in e.attributes().flatten() {
                        let k = std::str::from_utf8(a.key.into_inner()).unwrap_or("");
                        let val: f32 = a
                            .unescape_value()
                            .unwrap_or_default()
                            .parse()
                            .unwrap_or(0.0);
                        match k {
                            "x" => v.pan = val,
                            "y" => v.tilt = val,
                            _ => {}
                        }
                    }
                }
                if in_wrap && local == "Zoom" {
                    for a in e.attributes().flatten() {
                        let k = std::str::from_utf8(a.key.into_inner()).unwrap_or("");
                        if k == "x" {
                            v.zoom = a
                                .unescape_value()
                                .unwrap_or_default()
                                .parse()
                                .unwrap_or(0.0);
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = std::str::from_utf8(e.name().into_inner()).unwrap_or("");
                let local = name.rsplit(':').next().unwrap_or(name);
                if local == wrapper {
                    in_wrap = false;
                }
            }
            _ => {}
        }
    }
    v
}

/// Is there a `<Zoom .../>` element nested inside the named wrapper element?
/// Used to disambiguate "Zoom omitted" from "Zoom = 0.0" without false-matching
/// on a sibling like `<Speed><Zoom .../></Speed>`.
fn zoom_present_in(xml: &str, wrapper: &str) -> bool {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_wrap = false;
    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => return false,
            Ok(Event::Start(e)) => {
                let name = std::str::from_utf8(e.name().into_inner()).unwrap_or("");
                let local = name.rsplit(':').next().unwrap_or(name);
                if local == wrapper {
                    // Enter the wrapper. We don't enter on Event::Empty for
                    // the wrapper itself because a self-closing `<Position/>`
                    // has no children — nothing inside to match.
                    in_wrap = true;
                } else if in_wrap && local == "Zoom" {
                    return true;
                }
            }
            Ok(Event::Empty(e)) => {
                let name = std::str::from_utf8(e.name().into_inner()).unwrap_or("");
                let local = name.rsplit(':').next().unwrap_or(name);
                if in_wrap && local == "Zoom" {
                    return true;
                }
            }
            Ok(Event::End(e)) => {
                let name = std::str::from_utf8(e.name().into_inner()).unwrap_or("");
                let local = name.rsplit(':').next().unwrap_or(name);
                if local == wrapper {
                    in_wrap = false;
                }
            }
            _ => {}
        }
    }
}

/// Read boolean flags `<PanTilt>true</PanTilt>` / `<Zoom>true</Zoom>` from a
/// `Stop` request body. Default — when neither tag is present — is to stop
/// both axes.
fn parse_stop_flags(xml: &str) -> (bool, bool) {
    let mut pan_tilt = read_first_text_element(xml, "PanTilt");
    let mut zoom = read_first_text_element(xml, "Zoom");
    if pan_tilt.is_none() && zoom.is_none() {
        pan_tilt = Some("true".into());
        zoom = Some("true".into());
    }
    let parse = |s: Option<String>| {
        s.map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    };
    (parse(pan_tilt), parse(zoom))
}

async fn send_direction(cam: &CameraEntry, dir: Direction, speed: f32) -> Result<()> {
    cam.run(move |c: &BcCamera| {
        Box::pin(async move {
            c.send_ptz(dir, speed).await?;
            Ok(())
        })
    })
    .await
}

async fn stop_pt(cam: &CameraEntry) -> Result<()> {
    send_direction(cam, Direction::Stop, 0.0).await
}

async fn abort_zoom_task(cam: &CameraEntry) {
    let mut g = cam.zoom_task.lock().await;
    if let Some(h) = g.take() {
        h.abort();
    }
}

/// Spawn a background task that approximates a continuous-zoom move. Reolink
/// has no native "zoom velocity" so we simulate it by stepping `zoom_to`.
///
/// The mutex is held across abort+spawn+store so two concurrent
/// `ContinuousMove` requests can't leak the previous task: without atomicity
/// it's possible for both calls to abort the slot, both spawn a fresh task,
/// then race to store — the loser's handle is dropped without being aborted
/// and keeps stepping zoom in the background.
///
/// The spawned task holds a *Weak* reference to the CameraEntry, not Arc.
/// Storing a JoinHandle for self on the entry would otherwise be a reference
/// cycle (entry → JoinHandle for task → task closure → Arc<entry>) that
/// keeps the entry alive forever after the bridge drops it from the map.
async fn spawn_zoom_task(cam: &Arc<CameraEntry>, dir: f32) {
    let mut g = cam.zoom_task.lock().await;
    if let Some(h) = g.take() {
        h.abort();
    }
    let cam_weak = Arc::downgrade(cam);
    let h = tokio::spawn(async move {
        let _ = run_zoom_loop(cam_weak, dir).await;
    });
    *g = Some(h);
}

async fn run_zoom_loop(cam_weak: std::sync::Weak<CameraEntry>, dir: f32) -> Result<()> {
    // The loop body upgrades the Weak to Arc only for the duration of a single
    // step; if the entry has been dropped (config reload removed the camera,
    // bridge shutdown, ...) we exit cleanly without holding it alive.
    let Some(cam) = cam_weak.upgrade() else {
        return Ok(());
    };
    let zf = cam
        .run(|c: &BcCamera| Box::pin(async move { Ok(c.get_zoom().await?) }))
        .await?;
    let min = zf.zoom.min_pos;
    let max = zf.zoom.max_pos;
    drop(cam);
    if max <= min {
        return Ok(());
    }
    let mut cur = zf.zoom.cur_pos;
    let step = ((max - min) / 20).max(1);
    loop {
        let next = if dir > 0.0 {
            cur.saturating_add(step).min(max)
        } else {
            cur.saturating_sub(step).max(min)
        };
        if next == cur {
            break;
        }
        let target = next;
        let Some(cam) = cam_weak.upgrade() else {
            return Ok(());
        };
        cam.run(move |c: &BcCamera| {
            Box::pin(async move {
                c.zoom_to(target).await?;
                Ok(())
            })
        })
        .await?;
        drop(cam);
        cur = next;
        // Roughly 4 steps/sec — fast enough for HA's typical 250-500ms
        // press-and-hold pulses, slow enough to not saturate the camera.
        sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

pub(crate) async fn dispatch(
    _state: &OnvifState,
    cam: &Arc<CameraEntry>,
    action: &str,
    body_xml: &str,
) -> Result<String, FaultBody> {
    let caps = capabilities(cam).await;

    // A camera with no motor at all still gets asked: `GetCapabilities` no
    // longer lists a PTZ XAddr, but plenty of clients probe the well-known
    // path anyway. Answer the enumerations with an empty set — that is the
    // spec's way of saying "nothing here" — and fault the rest.
    if !caps.ptz() {
        return match action {
            "GetNodes" => Ok(wrap_envelope("<tptz:GetNodesResponse/>", NS_ALL)),
            "GetConfigurations" => Ok(wrap_envelope("<tptz:GetConfigurationsResponse/>", NS_ALL)),
            "GetServiceCapabilities" => {
                Ok(wrap_envelope(&render_service_capabilities(&caps), NS_ALL))
            }
            _ => Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!("Camera '{}' has no PTZ support", cam.name),
            }),
        };
    }

    let body = match action {
        "GetConfigurations" => format!(
            "<tptz:GetConfigurationsResponse>{cfg}</tptz:GetConfigurationsResponse>",
            cfg = render_ptz_configuration_xml(&cam.name, &caps, "tptz:PTZConfiguration"),
        ),
        "GetConfiguration" => format!(
            "<tptz:GetConfigurationResponse>{cfg}</tptz:GetConfigurationResponse>",
            cfg = render_ptz_configuration_xml(&cam.name, &caps, "tptz:PTZConfiguration"),
        ),
        "GetConfigurationOptions" => render_configuration_options(&caps),
        "GetServiceCapabilities" => render_service_capabilities(&caps),
        "GetNodes" => format!(
            "<tptz:GetNodesResponse>{n}</tptz:GetNodesResponse>",
            n = render_ptz_node(&cam.name, &caps)
        ),
        "GetNode" => format!(
            "<tptz:GetNodeResponse>{n}</tptz:GetNodeResponse>",
            n = render_ptz_node(&cam.name, &caps)
        ),
        "ContinuousMove" => {
            let v = parse_velocity(body_xml, "Velocity");
            if !caps.pan_tilt && pick_direction(v.pan, v.tilt).is_some() {
                return Err(FaultBody {
                    code: FaultCode::NoContinuousPanTiltSpace,
                    reason: format!("Camera '{}' has no pan/tilt", cam.name),
                });
            }
            if !caps.zoom && v.zoom.abs() >= 0.05 {
                return Err(FaultBody {
                    code: FaultCode::NoContinuousZoomSpace,
                    reason: format!("Camera '{}' has no optical zoom", cam.name),
                });
            }
            abort_zoom_task(cam).await;
            if let Some(dir) = pick_direction(v.pan, v.tilt) {
                let speed = onvif_to_reolink_speed(v.pan.abs().max(v.tilt.abs()));
                send_direction(cam, dir, speed).await.map_err(other_fault)?;
            } else if caps.pan_tilt && v.pan == 0.0 && v.tilt == 0.0 {
                // Pure zoom move — make sure no PT is in progress.
                let _ = stop_pt(cam).await;
            }
            if v.zoom.abs() >= 0.05 {
                spawn_zoom_task(cam, v.zoom).await;
            }
            "<tptz:ContinuousMoveResponse/>".to_string()
        }
        "RelativeMove" => {
            let translation = parse_velocity(body_xml, "Translation");
            let speed_v = parse_velocity(body_xml, "Speed");
            if !caps.pan_tilt && pick_direction(translation.pan, translation.tilt).is_some() {
                return Err(FaultBody {
                    code: FaultCode::NoRelativePanTiltSpace,
                    reason: format!("Camera '{}' has no pan/tilt", cam.name),
                });
            }
            if !caps.zoom && translation.zoom.abs() >= 0.005 {
                return Err(FaultBody {
                    code: FaultCode::NoRelativeZoomSpace,
                    reason: format!("Camera '{}' has no optical zoom", cam.name),
                });
            }
            abort_zoom_task(cam).await;
            // PT relative: do a timed continuous move. Magnitude is treated
            // as seconds (clamped to 10s) like the existing CLI does.
            if let Some(dir) = pick_direction(translation.pan, translation.tilt) {
                let speed_mag = if speed_v.pan != 0.0 || speed_v.tilt != 0.0 {
                    speed_v.pan.abs().max(speed_v.tilt.abs())
                } else {
                    1.0
                };
                let reolink_speed = onvif_to_reolink_speed(speed_mag);
                let dur = translation
                    .pan
                    .abs()
                    .max(translation.tilt.abs())
                    .clamp(0.05, 10.0);
                send_direction(cam, dir, reolink_speed)
                    .await
                    .map_err(other_fault)?;
                sleep(Duration::from_secs_f32(dur)).await;
                let _ = stop_pt(cam).await;
            }
            if translation.zoom.abs() >= 0.005 {
                relative_zoom(cam, translation.zoom)
                    .await
                    .map_err(other_fault)?;
            }
            "<tptz:RelativeMoveResponse/>".to_string()
        }
        "AbsoluteMove" => {
            let position = parse_velocity(body_xml, "Position");
            if position.pan != 0.0 || position.tilt != 0.0 {
                return Err(FaultBody {
                    code: FaultCode::NoAbsolutePtzSpace,
                    reason: "Reolink cameras do not support absolute pan/tilt".to_string(),
                });
            }
            // We need to distinguish "client sent Zoom=0.0" (move to fully
            // wide) from "client omitted Zoom entirely" (don't touch zoom).
            // Scan only for a Zoom element nested in Position so a sibling
            // <Speed><Zoom .../></Speed> doesn't trigger an unintended move.
            if zoom_present_in(body_xml, "Position") {
                if !caps.zoom {
                    return Err(FaultBody {
                        code: FaultCode::NoAbsoluteZoomSpace,
                        reason: format!("Camera '{}' has no optical zoom", cam.name),
                    });
                }
                absolute_zoom(cam, position.zoom)
                    .await
                    .map_err(other_fault)?;
            }
            "<tptz:AbsoluteMoveResponse/>".to_string()
        }
        "Stop" => {
            let (pt, zoom) = parse_stop_flags(body_xml);
            // Stop is a no-op for an axis the camera doesn't have; faulting
            // would break the common client pattern of stopping both axes
            // after every move.
            if pt && caps.pan_tilt {
                stop_pt(cam).await.map_err(other_fault)?;
            }
            if zoom {
                abort_zoom_task(cam).await;
            }
            "<tptz:StopResponse/>".to_string()
        }
        "GetStatus" => {
            let zoom_x = if caps.zoom {
                cam.run(|c| Box::pin(async move { Ok(c.get_zoom().await?) }))
                    .await
                    .ok()
                    .filter(|z| z.zoom.max_pos > z.zoom.min_pos)
                    .map(|z| {
                        (z.zoom.cur_pos.saturating_sub(z.zoom.min_pos)) as f32
                            / (z.zoom.max_pos - z.zoom.min_pos) as f32
                    })
            } else {
                None
            };
            // Only report a position for an axis that exists. A fixed mount
            // reporting `PanTilt x="0" y="0"` reads to a client as "centred",
            // not as "absent".
            let mut position = String::new();
            let mut move_status = String::new();
            if caps.pan_tilt {
                position.push_str(
                    "<tt:PanTilt x=\"0\" y=\"0\" space=\"http://www.onvif.org/ver10/tptz/PanTiltSpaces/PositionGenericSpace\"/>",
                );
                move_status.push_str("<tt:PanTilt>IDLE</tt:PanTilt>");
            }
            if let Some(zx) = zoom_x {
                position.push_str(&format!(
                    "<tt:Zoom x=\"{zx:.4}\" space=\"{ABSOLUTE_ZOOM_SPACE}\"/>"
                ));
            }
            if caps.zoom {
                move_status.push_str("<tt:Zoom>IDLE</tt:Zoom>");
            }
            format!(
                "<tptz:GetStatusResponse><tptz:PTZStatus>\
{position}\
<tt:MoveStatus>{move_status}</tt:MoveStatus>\
<tt:UtcTime>{ts}</tt:UtcTime>\
</tptz:PTZStatus></tptz:GetStatusResponse>",
                position = if position.is_empty() {
                    String::new()
                } else {
                    format!("<tt:Position>{position}</tt:Position>")
                },
                ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            )
        }
        "GetPresets" if !caps.presets => "<tptz:GetPresetsResponse/>".to_string(),
        "GotoPreset" | "SetPreset" | "GotoHomePosition" | "SetHomePosition" if !caps.presets => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!("Camera '{}' does not support PTZ presets", cam.name),
            });
        }
        // Recalling a preset and redefining one are separate permissions on a
        // Reolink camera. An account holding only `preset_ro` can drive the
        // camera and jump to stored positions, and every attempt to store one
        // is rejected by the camera — so say so here rather than letting the
        // client discover it as an opaque protocol error.
        "SetPreset" if !caps.preset_write => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!(
                    "Camera '{}' does not allow this user to store PTZ presets",
                    cam.name
                ),
            });
        }
        "SetHomePosition" if !caps.preset_write => {
            return Err(FaultBody {
                code: FaultCode::CannotOverwriteHome,
                reason: format!(
                    "Camera '{}' does not allow this user to store PTZ presets, \
so the home position cannot be overwritten",
                    cam.name
                ),
            });
        }
        "GetPresets" => {
            let presets = cam
                .run(|c| Box::pin(async move { Ok(c.get_ptz_preset().await?) }))
                .await
                .map_err(other_fault)?;
            let items: String = presets
                .preset_list
                .preset
                .iter()
                .map(|p| {
                    format!(
                        "<tptz:Preset token=\"preset_{id}\"><tt:Name>{name}</tt:Name></tptz:Preset>",
                        id = p.id,
                        name = preset_display_name(p.name.as_deref(), p.id),
                    )
                })
                .collect();
            format!("<tptz:GetPresetsResponse>{items}</tptz:GetPresetsResponse>")
        }
        "GotoPreset" => {
            let token =
                read_first_text_element(body_xml, "PresetToken").ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "Missing PresetToken".to_string(),
                })?;
            let id = parse_preset_id(&token).ok_or_else(|| FaultBody {
                code: FaultCode::InvalidArgs,
                reason: format!("Unknown preset token '{token}'"),
            })?;
            cam.run(move |c| {
                Box::pin(async move {
                    c.moveto_ptz_preset(id).await?;
                    Ok(())
                })
            })
            .await
            .map_err(other_fault)?;
            "<tptz:GotoPresetResponse/>".to_string()
        }
        "SetPreset" => {
            let name = read_first_text_element(body_xml, "PresetName")
                .unwrap_or_else(|| "preset".to_string());
            let token_opt = read_first_text_element(body_xml, "PresetToken");
            let id = match token_opt.as_deref().and_then(parse_preset_id) {
                Some(id) => id,
                None => allocate_preset_id(cam).await?,
            };
            let name_for_task = name.clone();
            cam.run(move |c| {
                let name = name_for_task.clone();
                Box::pin(async move {
                    c.set_ptz_preset(id, name).await?;
                    Ok(())
                })
            })
            .await
            .map_err(other_fault)?;
            format!(
                "<tptz:SetPresetResponse><tptz:PresetToken>preset_{id}</tptz:PresetToken></tptz:SetPresetResponse>"
            )
        }
        "RemovePreset" => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: "Reolink protocol has no preset deletion".to_string(),
            });
        }
        "GotoHomePosition" => {
            // Preset 0 is only a home position once something has been stored
            // in it. Asking the camera to move to an empty slot earns a bare
            // protocol rejection; the spec has a fault that says exactly what
            // is wrong, and clients can act on it (Home Assistant hides the
            // home button, ODM reports it).
            if home_preset(cam).await.map_err(other_fault)?.is_none() {
                return Err(FaultBody {
                    code: FaultCode::NoHomePosition,
                    reason: format!(
                        "Camera '{}' has no home position stored — save one first \
(ONVIF SetHomePosition, or preset {HOME_PRESET_ID} in the Reolink app)",
                        cam.name
                    ),
                });
            }
            cam.run(|c| {
                Box::pin(async move {
                    c.moveto_ptz_preset(HOME_PRESET_ID).await?;
                    Ok(())
                })
            })
            .await
            .map_err(other_fault)?;
            "<tptz:GotoHomePositionResponse/>".to_string()
        }
        "SetHomePosition" => {
            // Home is preset 0 (see HOME_PRESET_ID), which is an ordinary
            // writable slot, so this is a plain preset write rather than the
            // fault a truly fixed home would justify.
            //
            // Writing a preset also writes its name, and slot 0 may already
            // carry one the user chose in the Reolink app. Keep it: the client
            // asked to move the home position, not to relabel it.
            let name = home_preset(cam)
                .await
                .ok()
                .flatten()
                .and_then(|p| p.name)
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| "home".to_string());
            cam.run(move |c| {
                let name = name.clone();
                Box::pin(async move {
                    c.set_ptz_preset(HOME_PRESET_ID, name).await?;
                    Ok(())
                })
            })
            .await
            .map_err(other_fault)?;
            "<tptz:SetHomePositionResponse/>".to_string()
        }
        other => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!("PTZ action '{other}' not supported"),
            });
        }
    };
    Ok(wrap_envelope(&body, NS_ALL))
}

/// The `tt:Spaces` / `tt:SupportedPTZSpaces` body, holding only the spaces the
/// camera can actually be driven through. Shared by the PTZ node and the
/// configuration options so the two can never disagree.
fn render_supported_spaces(caps: &CameraCapabilities) -> String {
    let mut out = String::new();
    if caps.pan_tilt {
        out.push_str(&format!(
            "<tt:ContinuousPanTiltVelocitySpace>\
<tt:URI>{CONTINUOUS_PT_SPACE}</tt:URI>\
<tt:XRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:XRange>\
<tt:YRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:YRange>\
</tt:ContinuousPanTiltVelocitySpace>"
        ));
    }
    if caps.zoom {
        out.push_str(&format!(
            "<tt:ContinuousZoomVelocitySpace>\
<tt:URI>{CONTINUOUS_ZOOM_SPACE}</tt:URI>\
<tt:XRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:XRange>\
</tt:ContinuousZoomVelocitySpace>\
<tt:AbsoluteZoomPositionSpace>\
<tt:URI>{ABSOLUTE_ZOOM_SPACE}</tt:URI>\
<tt:XRange><tt:Min>0.0</tt:Min><tt:Max>1.0</tt:Max></tt:XRange>\
</tt:AbsoluteZoomPositionSpace>"
        ));
    }
    out
}

/// The `PTZConfiguration` element. `tag` differs by service: the Media service
/// nests it in a profile as `tt:PTZConfiguration`, the PTZ service returns it
/// as `tptz:PTZConfiguration`.
pub(crate) fn render_ptz_configuration_xml(
    cam_name: &str,
    caps: &CameraCapabilities,
    tag: &str,
) -> String {
    let mut default_spaces = String::new();
    let mut default_speed = String::new();
    if caps.pan_tilt {
        default_spaces.push_str(&format!(
            "<tt:DefaultContinuousPanTiltVelocitySpace>{CONTINUOUS_PT_SPACE}</tt:DefaultContinuousPanTiltVelocitySpace>"
        ));
        default_speed.push_str(&format!(
            "<tt:PanTilt x=\"0.5\" y=\"0.5\" space=\"{PT_SPEED_SPACE}\"/>"
        ));
    }
    if caps.zoom {
        default_spaces.push_str(&format!(
            "<tt:DefaultContinuousZoomVelocitySpace>{CONTINUOUS_ZOOM_SPACE}</tt:DefaultContinuousZoomVelocitySpace>"
        ));
        default_speed.push_str(&format!(
            "<tt:Zoom x=\"0.5\" space=\"{ZOOM_SPEED_SPACE}\"/>"
        ));
    }
    format!(
        "<{tag} token=\"ptz_{cam_name}\">\
<tt:Name>{cam_name}_ptz</tt:Name>\
<tt:UseCount>1</tt:UseCount>\
<tt:NodeToken>ptz_node_{cam_name}</tt:NodeToken>\
{default_spaces}\
<tt:DefaultPTZSpeed>{default_speed}</tt:DefaultPTZSpeed>\
<tt:DefaultPTZTimeout>PT5S</tt:DefaultPTZTimeout>\
</{tag}>",
        tag = tag,
        cam_name = xml_escape(cam_name),
    )
}

fn render_configuration_options(caps: &CameraCapabilities) -> String {
    format!(
        "<tptz:GetConfigurationOptionsResponse><tptz:PTZConfigurationOptions>\
<tt:Spaces>{spaces}</tt:Spaces>\
<tt:PTZTimeout><tt:Min>PT1S</tt:Min><tt:Max>PT60S</tt:Max></tt:PTZTimeout>\
</tptz:PTZConfigurationOptions></tptz:GetConfigurationOptionsResponse>",
        spaces = render_supported_spaces(caps),
    )
}

fn render_service_capabilities(caps: &CameraCapabilities) -> String {
    format!(
        "<tptz:GetServiceCapabilitiesResponse><tptz:Capabilities EFlip=\"false\" \
Reverse=\"false\" GetCompatibleConfigurations=\"true\" MoveStatus=\"false\" \
StatusPosition=\"{status_position}\"/></tptz:GetServiceCapabilitiesResponse>",
        // The only position we can actually read back off a Reolink camera is
        // the zoom one; without a zoom motor `GetStatus` carries no position
        // at all, and claiming otherwise makes clients poll it forever.
        status_position = caps.zoom,
    )
}

fn render_ptz_node(cam_name: &str, caps: &CameraCapabilities) -> String {
    format!(
        "<tptz:PTZNode token=\"ptz_node_{cam_name}\" FixedHomePosition=\"{fixed_home}\">\
<tt:Name>{cam_name}_node</tt:Name>\
<tt:SupportedPTZSpaces>{spaces}</tt:SupportedPTZSpaces>\
<tt:MaximumNumberOfPresets>{presets}</tt:MaximumNumberOfPresets>\
<tt:HomeSupported>{home}</tt:HomeSupported>\
</tptz:PTZNode>",
        cam_name = xml_escape(cam_name),
        spaces = render_supported_spaces(caps),
        // `GotoHomePosition` is implemented as "go to preset 0", so home is
        // exactly as available as presets are.
        presets = if caps.presets { MAX_PRESETS } else { 0 },
        home = caps.presets,
        // Home lives in an ordinary preset slot, so it is fixed exactly when
        // this user cannot write preset slots. A camera with no presets at all
        // has no home to fix.
        fixed_home = caps.presets && !caps.preset_write,
    )
}

/// The camera's current home preset, or `None` when nothing has ever been
/// stored in [`HOME_PRESET_ID`].
async fn home_preset(cam: &Arc<CameraEntry>) -> Result<Option<neolink_core::bc::xml::Preset>> {
    let presets = cam
        .run(|c| Box::pin(async move { Ok(c.get_ptz_preset().await?) }))
        .await?;
    Ok(presets
        .preset_list
        .preset
        .into_iter()
        .find(|p| p.id == HOME_PRESET_ID))
}

/// What to show a client for a preset. The camera can return a slot with an
/// empty name; an unlabelled row is unusable in a VMS preset list, so fall back
/// to the slot number rather than handing out a blank.
fn preset_display_name(name: Option<&str>, id: u8) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => xml_escape(n),
        None => format!("Preset {id}"),
    }
}

/// Reolink's preset table is 64 slots wide (ids 0..=63).
const MAX_PRESETS: u8 = 64;

/// The preset slot `GotoHomePosition` / `SetHomePosition` map onto.
///
/// The Reolink protocol has no distinct "home" position — it has a flat preset
/// table — so home is a convention: preset 0. That makes home *writable*, which
/// is why the node reports `FixedHomePosition="false"` and `SetHomePosition` is
/// implemented rather than refused: a client can already rewrite this slot with
/// an ordinary `SetPreset` on token `preset_0`, so claiming the home position
/// were fixed would be a claim the bridge cannot keep.
const HOME_PRESET_ID: u8 = 0;

async fn relative_zoom(cam: &Arc<CameraEntry>, delta: f32) -> Result<()> {
    let zf = cam
        .run(|c| Box::pin(async move { Ok(c.get_zoom().await?) }))
        .await?;
    let span = (zf.zoom.max_pos - zf.zoom.min_pos) as f32;
    let off = (delta.clamp(-1.0, 1.0) * span) as i64;
    let target =
        (zf.zoom.cur_pos as i64 + off).clamp(zf.zoom.min_pos as i64, zf.zoom.max_pos as i64) as u32;
    cam.run(move |c| {
        Box::pin(async move {
            c.zoom_to(target).await?;
            Ok(())
        })
    })
    .await?;
    Ok(())
}

async fn absolute_zoom(cam: &Arc<CameraEntry>, position: f32) -> Result<()> {
    let zf = cam
        .run(|c| Box::pin(async move { Ok(c.get_zoom().await?) }))
        .await?;
    let span = (zf.zoom.max_pos - zf.zoom.min_pos) as f32;
    let target = (zf.zoom.min_pos as f32 + position.clamp(0.0, 1.0) * span) as u32;
    cam.run(move |c| {
        Box::pin(async move {
            c.zoom_to(target).await?;
            Ok(())
        })
    })
    .await?;
    Ok(())
}

fn parse_preset_id(token: &str) -> Option<u8> {
    token.strip_prefix("preset_").and_then(|s| s.parse().ok())
}

/// Pick a free preset slot for a `SetPreset` that didn't name one.
///
/// Skips [`HOME_PRESET_ID`] on the first pass: "save the current view as a new
/// preset" should not silently redefine where `GotoHomePosition` goes. A client
/// that actually means to move home can still say so, by passing `preset_0` or
/// by calling `SetHomePosition`. Slot 0 is only handed out once every other
/// slot is taken, so no preset capacity is lost.
async fn allocate_preset_id(cam: &Arc<CameraEntry>) -> Result<u8, FaultBody> {
    let presets = cam
        .run(|c| Box::pin(async move { Ok(c.get_ptz_preset().await?) }))
        .await
        .map_err(other_fault)?;
    let used: std::collections::HashSet<u8> =
        presets.preset_list.preset.iter().map(|p| p.id).collect();
    pick_free_preset_id(&used).ok_or_else(|| FaultBody {
        code: FaultCode::TooManyPresets,
        reason: format!(
            "Camera '{}' has no free preset slots — all {MAX_PRESETS} are in use",
            cam.name
        ),
    })
}

fn pick_free_preset_id(used: &std::collections::HashSet<u8>) -> Option<u8> {
    let free = |id: &u8| !used.contains(id);
    (0u8..MAX_PRESETS)
        .filter(|id| *id != HOME_PRESET_ID)
        .find(free)
        .or_else(|| free(&HOME_PRESET_ID).then_some(HOME_PRESET_ID))
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

    const FULL: CameraCapabilities = CameraCapabilities {
        pan_tilt: true,
        zoom: true,
        presets: true,
        preset_write: true,
    };
    const PT_ONLY: CameraCapabilities = CameraCapabilities {
        pan_tilt: true,
        zoom: false,
        presets: true,
        preset_write: true,
    };
    const ZOOM_ONLY: CameraCapabilities = CameraCapabilities {
        pan_tilt: false,
        zoom: true,
        presets: false,
        preset_write: false,
    };
    /// A user who may recall stored positions but not redefine them.
    const READ_ONLY_PRESETS: CameraCapabilities = CameraCapabilities {
        pan_tilt: true,
        zoom: true,
        presets: true,
        preset_write: false,
    };

    /// A fixed-lens pan/tilt camera must not offer a zoom space — clients read
    /// this list to decide which controls to draw.
    #[test]
    fn a_pan_tilt_only_node_advertises_no_zoom_space() {
        let xml = render_ptz_node("cam", &PT_ONLY);
        assert!(xml.contains("ContinuousPanTiltVelocitySpace"));
        assert!(!xml.contains("ZoomVelocitySpace"), "{}", xml);
        assert!(!xml.contains("AbsoluteZoomPositionSpace"), "{}", xml);
    }

    #[test]
    fn a_zoom_only_node_advertises_no_pan_tilt_space() {
        let xml = render_ptz_node("cam", &ZOOM_ONLY);
        assert!(!xml.contains("PanTiltVelocitySpace"), "{}", xml);
        assert!(xml.contains("ContinuousZoomVelocitySpace"));
        assert!(xml.contains("AbsoluteZoomPositionSpace"));
    }

    /// `GotoHomePosition` is preset 0, so a camera without presets has no home
    /// and no preset slots to offer.
    #[test]
    fn presets_and_home_track_each_other() {
        let xml = render_ptz_node("cam", &FULL);
        assert!(xml.contains("<tt:MaximumNumberOfPresets>64</tt:MaximumNumberOfPresets>"));
        assert!(xml.contains("<tt:HomeSupported>true</tt:HomeSupported>"));

        let xml = render_ptz_node("cam", &ZOOM_ONLY);
        assert!(xml.contains("<tt:MaximumNumberOfPresets>0</tt:MaximumNumberOfPresets>"));
        assert!(xml.contains("<tt:HomeSupported>false</tt:HomeSupported>"));
    }

    /// The node and the configuration options describe the same hardware, so
    /// they must list the same spaces.
    #[test]
    fn node_and_configuration_options_agree() {
        for caps in [FULL, PT_ONLY, ZOOM_ONLY] {
            let spaces = render_supported_spaces(&caps);
            assert!(render_ptz_node("cam", &caps).contains(&spaces));
            assert!(render_configuration_options(&caps).contains(&spaces));
        }
    }

    #[test]
    fn a_configuration_only_defaults_the_axes_that_exist() {
        let xml = render_ptz_configuration_xml("cam", &PT_ONLY, "tt:PTZConfiguration");
        assert!(xml.contains("DefaultContinuousPanTiltVelocitySpace"));
        assert!(
            !xml.contains("DefaultContinuousZoomVelocitySpace"),
            "{}",
            xml
        );
        assert!(xml.contains("<tt:PanTilt x=\"0.5\""));
        assert!(!xml.contains("<tt:Zoom x=\"0.5\""), "{}", xml);

        let xml = render_ptz_configuration_xml("cam", &ZOOM_ONLY, "tt:PTZConfiguration");
        assert!(
            !xml.contains("DefaultContinuousPanTiltVelocitySpace"),
            "{}",
            xml
        );
        assert!(xml.contains("DefaultContinuousZoomVelocitySpace"));
    }

    /// The zoom position is the only one we can read back off a Reolink
    /// camera, so it is the only thing that can justify `StatusPosition`.
    #[test]
    fn status_position_follows_zoom() {
        assert!(render_service_capabilities(&FULL).contains("StatusPosition=\"true\""));
        assert!(render_service_capabilities(&PT_ONLY).contains("StatusPosition=\"false\""));
    }

    #[test]
    fn a_camera_with_nothing_gets_an_empty_space_list() {
        let none = CameraCapabilities {
            pan_tilt: false,
            zoom: false,
            presets: false,
            preset_write: false,
        };
        assert_eq!(render_supported_spaces(&none), "");
    }

    #[test]
    fn speed_mapping() {
        assert_eq!(onvif_to_reolink_speed(0.0), 1.0);
        assert_eq!(onvif_to_reolink_speed(1.0), 64.0);
        assert_eq!(onvif_to_reolink_speed(-1.0), 64.0);
        let mid = onvif_to_reolink_speed(0.5);
        assert!((32.0..=33.0).contains(&mid));
    }

    #[test]
    fn direction_picks_cardinal_when_one_axis_dominates() {
        assert!(pick_direction(0.0, 0.0).is_none());
        // Tiny secondary axis falls under the diagonal threshold.
        assert!(matches!(pick_direction(0.8, 0.01), Some(Direction::Right)));
        assert!(matches!(pick_direction(-0.8, 0.01), Some(Direction::Left)));
        assert!(matches!(pick_direction(0.01, 0.8), Some(Direction::Up)));
        assert!(matches!(pick_direction(0.01, -0.8), Some(Direction::Down)));
    }

    #[test]
    fn direction_picks_diagonal_when_both_axes_significant() {
        assert!(matches!(pick_direction(0.8, 0.5), Some(Direction::RightUp)));
        assert!(matches!(pick_direction(-0.8, 0.5), Some(Direction::LeftUp)));
        assert!(matches!(
            pick_direction(0.8, -0.5),
            Some(Direction::RightDown)
        ));
        assert!(matches!(
            pick_direction(-0.8, -0.5),
            Some(Direction::LeftDown)
        ));
    }

    #[test]
    fn velocity_parse() {
        let xml = r#"<tptz:ContinuousMove xmlns:tptz="x" xmlns:tt="y">
<tptz:ProfileToken>foo</tptz:ProfileToken>
<tptz:Velocity>
  <tt:PanTilt x="0.5" y="-0.2"/>
  <tt:Zoom x="0.3"/>
</tptz:Velocity>
</tptz:ContinuousMove>"#;
        let v = parse_velocity(xml, "Velocity");
        assert!((v.pan - 0.5).abs() < 1e-6);
        assert!((v.tilt + 0.2).abs() < 1e-6);
        assert!((v.zoom - 0.3).abs() < 1e-6);
    }

    #[test]
    fn stop_default_both_axes() {
        let xml =
            r#"<tptz:Stop xmlns:tptz="x"><tptz:ProfileToken>foo</tptz:ProfileToken></tptz:Stop>"#;
        assert_eq!(parse_stop_flags(xml), (true, true));
    }

    #[test]
    fn stop_pan_tilt_only() {
        let xml = r#"<tptz:Stop xmlns:tptz="x"><tptz:ProfileToken>foo</tptz:ProfileToken><tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>false</tptz:Zoom></tptz:Stop>"#;
        assert_eq!(parse_stop_flags(xml), (true, false));
    }

    /// The Reolink protocol has no fixed home — home is preset 0, an ordinary
    /// writable slot — so the node must not claim otherwise. Claiming a fixed
    /// home while `SetPreset` on `preset_0` can rewrite it is exactly the kind
    /// of untrue advertisement this service is supposed to stop making.
    #[test]
    fn home_is_not_advertised_as_fixed() {
        let xml = render_ptz_node("cam", &FULL);
        assert!(xml.contains("FixedHomePosition=\"false\""), "{}", xml);
        assert!(xml.contains("<tt:HomeSupported>true</tt:HomeSupported>"));
    }

    /// ...but for a user who cannot write the preset table, home really is
    /// fixed: they can go to it and cannot move it.
    #[test]
    fn home_is_fixed_when_presets_are_read_only() {
        let xml = render_ptz_node("cam", &READ_ONLY_PRESETS);
        assert!(xml.contains("FixedHomePosition=\"true\""), "{}", xml);
        assert!(
            xml.contains("<tt:HomeSupported>true</tt:HomeSupported>"),
            "recall still works, so home is still supported: {}",
            xml
        );
    }

    /// A camera with no preset table has no home at all — there is nothing to
    /// call fixed.
    #[test]
    fn a_camera_without_presets_has_no_fixed_home_to_claim() {
        let xml = render_ptz_node("cam", &ZOOM_ONLY);
        assert!(xml.contains("FixedHomePosition=\"false\""), "{}", xml);
        assert!(xml.contains("<tt:HomeSupported>false</tt:HomeSupported>"));
    }

    /// A blank slot name renders as a usable label rather than an empty row in
    /// the client's preset list.
    #[test]
    fn presets_always_get_a_label() {
        assert_eq!(preset_display_name(Some("Driveway"), 3), "Driveway");
        assert_eq!(preset_display_name(Some("   "), 3), "Preset 3");
        assert_eq!(preset_display_name(None, 7), "Preset 7");
    }

    #[test]
    fn preset_names_are_escaped() {
        assert_eq!(
            preset_display_name(Some("Fred & <Ginger>"), 1),
            "Fred &amp; &lt;Ginger&gt;"
        );
    }

    /// "Save this view as a new preset" must not quietly redefine where the
    /// home button goes.
    #[test]
    fn auto_allocation_leaves_the_home_slot_alone() {
        let empty = std::collections::HashSet::new();
        assert_eq!(pick_free_preset_id(&empty), Some(1));

        let used: std::collections::HashSet<u8> = (1u8..5).collect();
        assert_eq!(pick_free_preset_id(&used), Some(5));
    }

    /// ...but the slot isn't wasted: it is still handed out once nothing else
    /// is left, so the camera keeps all 64 usable presets.
    #[test]
    fn the_home_slot_is_the_last_resort_not_a_reservation() {
        let used: std::collections::HashSet<u8> = (1u8..MAX_PRESETS).collect();
        assert_eq!(pick_free_preset_id(&used), Some(HOME_PRESET_ID));

        let all: std::collections::HashSet<u8> = (0u8..MAX_PRESETS).collect();
        assert_eq!(pick_free_preset_id(&all), None);
    }

    #[test]
    fn preset_id_round_trip() {
        assert_eq!(parse_preset_id("preset_7"), Some(7));
        assert_eq!(parse_preset_id("preset_xyz"), None);
        assert_eq!(parse_preset_id("xyz"), None);
    }

    /// Regression: `<Position/>` self-closing must not put `zoom_present_in`
    /// in the "inside wrapper" state, otherwise a later `<Speed><Zoom/></Speed>`
    /// would falsely match.
    #[test]
    fn zoom_present_self_closing_position_does_not_match_speed_zoom() {
        let xml = r#"<tptz:AbsoluteMove xmlns:tptz="x" xmlns:tt="y">
<tptz:ProfileToken>p</tptz:ProfileToken>
<tptz:Position/>
<tptz:Speed><tt:Zoom x="0.5"/></tptz:Speed>
</tptz:AbsoluteMove>"#;
        assert!(!zoom_present_in(xml, "Position"));
    }

    #[test]
    fn zoom_present_inside_position_matches() {
        let xml = r#"<tptz:AbsoluteMove xmlns:tptz="x" xmlns:tt="y">
<tptz:Position><tt:Zoom x="0.5"/></tptz:Position>
</tptz:AbsoluteMove>"#;
        assert!(zoom_present_in(xml, "Position"));
    }

    #[test]
    fn zoom_present_only_inside_wrapper() {
        let xml = r#"<tptz:AbsoluteMove xmlns:tptz="x" xmlns:tt="y">
<tptz:Speed><tt:Zoom x="0.5"/></tptz:Speed>
</tptz:AbsoluteMove>"#;
        assert!(!zoom_present_in(xml, "Position"));
    }
}
