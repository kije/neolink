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

use std::sync::Arc;

use anyhow::Result;
use neolink_core::bc_protocol::{BcCamera, Direction};
use quick_xml::events::Event;
use quick_xml::Reader;
use tokio::time::{sleep, Duration};

use crate::onvif::services::device::FaultBody;
use crate::onvif::services::media::read_first_text_element;
use crate::onvif::soap::{wrap_envelope, xml_escape, FaultCode, NS_ALL};
use crate::onvif::state::{CameraEntry, OnvifState};

/// Map a normalized [-1.0, 1.0] velocity magnitude to the Reolink `speed`
/// parameter (an f32, conventionally 1..=64; the CLI/MQTT default is 32).
fn onvif_to_reolink_speed(v: f32) -> f32 {
    let mag = v.abs().clamp(0.0, 1.0);
    (1.0 + mag * 63.0).round()
}

/// Choose a Reolink direction string from a velocity vector. Returns None when
/// both components are essentially zero.
fn pick_direction(x: f32, y: f32) -> Option<Direction> {
    // The current crates/core::Direction enum has no diagonals; pick the
    // dominant axis. Threshold of 0.05 to avoid jitter triggering moves.
    let ax = x.abs();
    let ay = y.abs();
    if ax < 0.05 && ay < 0.05 {
        return None;
    }
    if ax >= ay {
        Some(if x > 0.0 {
            Direction::Right
        } else {
            Direction::Left
        })
    } else {
        Some(if y > 0.0 {
            Direction::Up
        } else {
            Direction::Down
        })
    }
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
    let body = match action {
        "GetConfigurations" => format!(
            "<tptz:GetConfigurationsResponse>{cfg}</tptz:GetConfigurationsResponse>",
            cfg = render_ptz_configuration_xml(cam, "tptz:PTZConfiguration"),
        ),
        "GetConfiguration" => format!(
            "<tptz:GetConfigurationResponse>{cfg}</tptz:GetConfigurationResponse>",
            cfg = render_ptz_configuration_xml(cam, "tptz:PTZConfiguration"),
        ),
        "GetConfigurationOptions" => render_configuration_options(),
        "GetServiceCapabilities" => {
            "<tptz:GetServiceCapabilitiesResponse><tptz:Capabilities EFlip=\"false\" Reverse=\"false\" GetCompatibleConfigurations=\"true\" MoveStatus=\"false\" StatusPosition=\"true\"/></tptz:GetServiceCapabilitiesResponse>".to_string()
        }
        "GetNodes" => format!(
            "<tptz:GetNodesResponse>{n}</tptz:GetNodesResponse>",
            n = render_ptz_node(cam)
        ),
        "GetNode" => format!(
            "<tptz:GetNodeResponse>{n}</tptz:GetNodeResponse>",
            n = render_ptz_node(cam)
        ),
        "ContinuousMove" => {
            let v = parse_velocity(body_xml, "Velocity");
            abort_zoom_task(cam).await;
            if let Some(dir) = pick_direction(v.pan, v.tilt) {
                let speed = onvif_to_reolink_speed(v.pan.abs().max(v.tilt.abs()));
                send_direction(cam, dir, speed).await.map_err(other_fault)?;
            } else if v.pan == 0.0 && v.tilt == 0.0 {
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
                relative_zoom(cam, translation.zoom).await.map_err(other_fault)?;
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
                absolute_zoom(cam, position.zoom).await.map_err(other_fault)?;
            }
            "<tptz:AbsoluteMoveResponse/>".to_string()
        }
        "Stop" => {
            let (pt, zoom) = parse_stop_flags(body_xml);
            if pt {
                stop_pt(cam).await.map_err(other_fault)?;
            }
            if zoom {
                abort_zoom_task(cam).await;
            }
            "<tptz:StopResponse/>".to_string()
        }
        "GetStatus" => {
            let zf = cam
                .run(|c| Box::pin(async move { Ok(c.get_zoom().await?) }))
                .await
                .ok();
            let zoom_x = zf
                .as_ref()
                .filter(|z| z.zoom.max_pos > z.zoom.min_pos)
                .map(|z| {
                    (z.zoom.cur_pos.saturating_sub(z.zoom.min_pos)) as f32
                        / (z.zoom.max_pos - z.zoom.min_pos) as f32
                })
                .unwrap_or(0.0);
            format!(
                "<tptz:GetStatusResponse><tptz:PTZStatus>\
<tt:Position>\
<tt:PanTilt x=\"0\" y=\"0\" space=\"http://www.onvif.org/ver10/tptz/PanTiltSpaces/PositionGenericSpace\"/>\
<tt:Zoom x=\"{zx:.4}\" space=\"http://www.onvif.org/ver10/tptz/ZoomSpaces/PositionGenericSpace\"/>\
</tt:Position>\
<tt:MoveStatus>\
<tt:PanTilt>IDLE</tt:PanTilt>\
<tt:Zoom>IDLE</tt:Zoom>\
</tt:MoveStatus>\
<tt:UtcTime>{ts}</tt:UtcTime>\
</tptz:PTZStatus></tptz:GetStatusResponse>",
                zx = zoom_x,
                ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            )
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
                        name = xml_escape(p.name.as_deref().unwrap_or("")),
                    )
                })
                .collect();
            format!("<tptz:GetPresetsResponse>{items}</tptz:GetPresetsResponse>")
        }
        "GotoPreset" => {
            let token = read_first_text_element(body_xml, "PresetToken")
                .ok_or_else(|| FaultBody {
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
                None => allocate_preset_id(cam).await.map_err(other_fault)?,
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
            cam.run(|c| {
                    Box::pin(async move {
                        c.moveto_ptz_preset(0).await?;
                        Ok(())
                    })
                })
                .await
                .map_err(other_fault)?;
            "<tptz:GotoHomePositionResponse/>".to_string()
        }
        "SetHomePosition" => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: "Home position cannot be reassigned on Reolink".to_string(),
            });
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

fn render_ptz_configuration_xml(cam: &CameraEntry, tag: &str) -> String {
    format!(
        "<{tag} token=\"ptz_{cam_name}\">\
<tt:Name>{cam_name}_ptz</tt:Name>\
<tt:UseCount>1</tt:UseCount>\
<tt:NodeToken>ptz_node_{cam_name}</tt:NodeToken>\
<tt:DefaultContinuousPanTiltVelocitySpace>http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace</tt:DefaultContinuousPanTiltVelocitySpace>\
<tt:DefaultContinuousZoomVelocitySpace>http://www.onvif.org/ver10/tptz/ZoomSpaces/VelocityGenericSpace</tt:DefaultContinuousZoomVelocitySpace>\
<tt:DefaultPTZSpeed>\
<tt:PanTilt x=\"0.5\" y=\"0.5\" space=\"http://www.onvif.org/ver10/tptz/PanTiltSpaces/GenericSpeedSpace\"/>\
<tt:Zoom x=\"0.5\" space=\"http://www.onvif.org/ver10/tptz/ZoomSpaces/ZoomGenericSpeedSpace\"/>\
</tt:DefaultPTZSpeed>\
<tt:DefaultPTZTimeout>PT5S</tt:DefaultPTZTimeout>\
</{tag}>",
        tag = tag,
        cam_name = xml_escape(&cam.name),
    )
}

fn render_configuration_options() -> String {
    "<tptz:GetConfigurationOptionsResponse><tptz:PTZConfigurationOptions>\
<tt:Spaces>\
<tt:ContinuousPanTiltVelocitySpace>\
<tt:URI>http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace</tt:URI>\
<tt:XRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:XRange>\
<tt:YRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:YRange>\
</tt:ContinuousPanTiltVelocitySpace>\
<tt:ContinuousZoomVelocitySpace>\
<tt:URI>http://www.onvif.org/ver10/tptz/ZoomSpaces/VelocityGenericSpace</tt:URI>\
<tt:XRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:XRange>\
</tt:ContinuousZoomVelocitySpace>\
<tt:AbsoluteZoomPositionSpace>\
<tt:URI>http://www.onvif.org/ver10/tptz/ZoomSpaces/PositionGenericSpace</tt:URI>\
<tt:XRange><tt:Min>0.0</tt:Min><tt:Max>1.0</tt:Max></tt:XRange>\
</tt:AbsoluteZoomPositionSpace>\
</tt:Spaces>\
<tt:PTZTimeout><tt:Min>PT1S</tt:Min><tt:Max>PT60S</tt:Max></tt:PTZTimeout>\
</tptz:PTZConfigurationOptions></tptz:GetConfigurationOptionsResponse>"
        .to_string()
}

fn render_ptz_node(cam: &CameraEntry) -> String {
    format!(
        "<tptz:PTZNode token=\"ptz_node_{cam_name}\" FixedHomePosition=\"true\">\
<tt:Name>{cam_name}_node</tt:Name>\
<tt:SupportedPTZSpaces>\
<tt:ContinuousPanTiltVelocitySpace>\
<tt:URI>http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace</tt:URI>\
<tt:XRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:XRange>\
<tt:YRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:YRange>\
</tt:ContinuousPanTiltVelocitySpace>\
<tt:ContinuousZoomVelocitySpace>\
<tt:URI>http://www.onvif.org/ver10/tptz/ZoomSpaces/VelocityGenericSpace</tt:URI>\
<tt:XRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:XRange>\
</tt:ContinuousZoomVelocitySpace>\
<tt:AbsoluteZoomPositionSpace>\
<tt:URI>http://www.onvif.org/ver10/tptz/ZoomSpaces/PositionGenericSpace</tt:URI>\
<tt:XRange><tt:Min>0.0</tt:Min><tt:Max>1.0</tt:Max></tt:XRange>\
</tt:AbsoluteZoomPositionSpace>\
</tt:SupportedPTZSpaces>\
<tt:MaximumNumberOfPresets>64</tt:MaximumNumberOfPresets>\
<tt:HomeSupported>true</tt:HomeSupported>\
</tptz:PTZNode>",
        cam_name = xml_escape(&cam.name)
    )
}

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

async fn allocate_preset_id(cam: &Arc<CameraEntry>) -> Result<u8> {
    let presets = cam
        .run(|c| Box::pin(async move { Ok(c.get_ptz_preset().await?) }))
        .await?;
    let used: std::collections::HashSet<u8> =
        presets.preset_list.preset.iter().map(|p| p.id).collect();
    for id in 0u8..=63 {
        if !used.contains(&id) {
            return Ok(id);
        }
    }
    anyhow::bail!("No free preset slots")
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

    #[test]
    fn speed_mapping() {
        assert_eq!(onvif_to_reolink_speed(0.0), 1.0);
        assert_eq!(onvif_to_reolink_speed(1.0), 64.0);
        assert_eq!(onvif_to_reolink_speed(-1.0), 64.0);
        let mid = onvif_to_reolink_speed(0.5);
        assert!((32.0..=33.0).contains(&mid));
    }

    #[test]
    fn direction_picks_dominant_axis() {
        assert!(pick_direction(0.0, 0.0).is_none());
        assert!(matches!(pick_direction(0.8, 0.1), Some(Direction::Right)));
        assert!(matches!(pick_direction(-0.8, 0.1), Some(Direction::Left)));
        assert!(matches!(pick_direction(0.1, 0.8), Some(Direction::Up)));
        assert!(matches!(pick_direction(0.1, -0.8), Some(Direction::Down)));
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
