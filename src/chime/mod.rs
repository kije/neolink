///
/// # Neolink Chime / Doorbell
///
/// Subcommand that exposes Baichuan doorbell + wireless chime control:
///
/// ```bash
/// # List paired chimes
/// neolink chime --config=config.toml CameraName list
///
/// # Ring a specific chime with a named tone
/// neolink chime --config=config.toml CameraName ring 1 citybird
///
/// # Show or set silent windows
/// neolink chime --config=config.toml CameraName silent 1
/// neolink chime --config=config.toml CameraName silent 1 63 22:00 07:00
///
/// # Play a quick-reply audio clip from the doorbell
/// neolink chime --config=config.toml CameraName quick-reply 1 --duration 5 --times 1
/// ```
///
use anyhow::{anyhow, Context, Result};

mod cmdline;

use crate::chime::cmdline::ChimeCommand;
use crate::common::NeoReactor;
pub(crate) use cmdline::Opt;
use neolink_core::bc_protocol::{SilentWindow, ToneId};

/// Entry point for the chime subcommand
///
/// Opt is the command-line options.
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;

    match opt.cmd {
        ChimeCommand::List => {
            let chimes = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        let list = cam
                            .list_chimes()
                            .await
                            .context("Unable to list paired chimes")?;
                        Ok(list)
                    })
                })
                .await?;
            if chimes.is_empty() {
                println!("No wireless chimes are paired with this doorbell.");
            } else {
                println!("Paired chimes:\nID   Name                 Vol Led MusicId");
                for chime in chimes {
                    println!(
                        "{:<4} {:<20} {:<3} {:<3} {}",
                        chime.device_id,
                        chime.name.as_deref().unwrap_or(""),
                        chime
                            .vol_level
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        chime
                            .led_state
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        chime
                            .music_id
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    );
                }
            }
        }
        ChimeCommand::Ring { device_id, tone } => {
            let tone_id = parse_tone(&tone).with_context(|| format!("Unknown tone: {tone:?}"))?;
            camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.ring_chime(device_id, tone_id)
                            .await
                            .context("Unable to ring chime")?;
                        Ok(())
                    })
                })
                .await?;
        }
        ChimeCommand::Silent {
            device_id,
            weekday_mask,
            start_time,
            end_time,
        } => match (weekday_mask, start_time, end_time) {
            (None, None, None) => {
                let window = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            let w = cam
                                .get_chime_silent(device_id)
                                .await
                                .context("Unable to read silent window")?;
                            Ok(w)
                        })
                    })
                    .await?;
                println!(
                    "Silent window for chime {device_id}: weekday_mask={} start={} end={}",
                    window.weekday_mask, window.start_time, window.end_time,
                );
            }
            (Some(mask), Some(start), Some(end)) => {
                let window = SilentWindow {
                    weekday_mask: mask,
                    start_time: start,
                    end_time: end,
                };
                camera
                    .run_task(|cam| {
                        let window = window.clone();
                        Box::pin(async move {
                            cam.set_chime_silent(device_id, window)
                                .await
                                .context("Unable to set silent window")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
            _ => {
                return Err(anyhow!(
                    "silent command requires either zero (get) or three (set) extra arguments"
                ));
            }
        },
        ChimeCommand::QuickReply {
            file_id,
            duration,
            times,
        } => {
            camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.play_quick_reply(file_id, duration, times)
                            .await
                            .context("Unable to play quick-reply clip")?;
                        Ok(())
                    })
                })
                .await?;
        }
    };

    Ok(())
}

fn parse_tone(s: &str) -> Result<u32> {
    if let Ok(n) = s.parse::<u32>() {
        return Ok(n);
    }
    ToneId::from_name(s)
        .map(|t| t.id())
        .ok_or_else(|| anyhow!("unknown tone name {s:?}"))
}
