///
/// # Neolink PTZ Control
///
/// This module handles the controls of the PTZ commands
///
/// # Usage
///
/// ```bash
/// # Rotate left by 32
/// neolink ptz --config=config.toml CameraName control 32 left
/// # Rotate left by 32 at speed 10 (speed not supported on most camera)
/// neolink ptz --config=config.toml CameraName control 32 left 10
/// # Print the list of preset positions
/// neolink ptz --config=config.toml CameraName preset
/// # Move the camera to preset ID 0
/// neolink ptz --config=config.toml CameraName preset 0
/// # Save the current position as preset ID 0 with name PresetName
/// neolink ptz --config=config.toml CameraName assign 0 PresetName
/// ```
///
use anyhow::{Context, Result};
use tokio::time::{sleep, Duration};

mod cmdline;

use crate::common::NeoReactor;
use crate::ptz::cmdline::{CmdDirection, GuardCommand, OnOff, PatrolCommand, PtzCommand};
pub(crate) use cmdline::Opt;
use neolink_core::bc_protocol::Direction;

/// Entry point for the ptz subcommand
///
/// Opt is the command line options
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;

    match opt.cmd {
        PtzCommand::Preset { preset_id } => {
            if let Some(preset_id) = preset_id {
                camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.moveto_ptz_preset(preset_id)
                                .await
                                .context("Unable to move to PTZ preset")?;
                            Ok(())
                        })
                    })
                    .await?;
            } else {
                let preset_list = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            let preset_list = cam
                                .get_ptz_preset()
                                .await
                                .context("Unable to get PTZ presets")?;
                            Ok(preset_list)
                        })
                    })
                    .await?;

                println!("Available presets:\nID Name");
                for preset in preset_list.preset_list.preset {
                    println!("{:<2} {:?}", preset.id, preset.name);
                }
            }
        }
        PtzCommand::Assign { preset_id, name } => {
            camera
                .run_task(|cam| {
                    let name = name.clone();
                    Box::pin(async move {
                        cam.set_ptz_preset(preset_id, name)
                            .await
                            .context("Unable to set PTZ preset")?;
                        Ok(())
                    })
                })
                .await?;
        }
        PtzCommand::Control {
            amount,
            command,
            speed,
        } => {
            let direction = match command {
                CmdDirection::Left => Direction::Left,
                CmdDirection::Right => Direction::Right,
                CmdDirection::Up => Direction::Up,
                CmdDirection::Down => Direction::Down,
                CmdDirection::Stop => Direction::Stop,
            };
            let speed = speed.unwrap_or(32) as f32;
            let seconds = amount as f32 / speed;
            let duration = Duration::from_secs_f32(seconds);
            camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.send_ptz(direction, speed)
                            .await
                            .context("Unable to execute PTZ move command")?;
                        Ok(())
                    })
                })
                .await?;

            sleep(duration).await;
            camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.send_ptz(Direction::Stop, 0_f32)
                            .await
                            .context("Unable to execute PTZ move command")?;
                        Ok(())
                    })
                })
                .await?;
        }
        PtzCommand::Zoom { amount } => {
            camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.zoom_to((amount * 1000.0) as u32)
                            .await
                            .context("Unable to execute PTZ move command")?;
                        Ok(())
                    })
                })
                .await?;
            sleep(Duration::from_secs(1)).await;
        }
        PtzCommand::Patrol { action } => match action {
            PatrolCommand::List => {
                let patrols = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            let list = cam
                                .list_patrols()
                                .await
                                .context("Unable to list PTZ patrols")?;
                            Ok(list)
                        })
                    })
                    .await?;
                println!("Available patrols:\nID Name      Stops");
                for patrol in &patrols.patrol {
                    let stops = patrol
                        .preset_list
                        .as_ref()
                        .map(|p| p.preset.len())
                        .unwrap_or(0);
                    println!(
                        "{:<2} {:<9} {}",
                        patrol.id,
                        patrol.name.as_deref().unwrap_or("-"),
                        stops
                    );
                }
            }
            PatrolCommand::Start { id } => {
                camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.start_patrol(id)
                                .await
                                .context("Unable to start PTZ patrol")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
            PatrolCommand::Stop { id } => {
                camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.stop_patrol(id)
                                .await
                                .context("Unable to stop PTZ patrol")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
        },
        PtzCommand::Guard { action } => match action {
            GuardCommand::Get => {
                let guard = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.get_guard().await.context("Unable to read PTZ guard")
                        })
                    })
                    .await?;
                println!(
                    "Guard: enabled={} timeout={}s needSetPos={:?}",
                    guard.benable != 0,
                    guard.timeout,
                    guard.need_set_pos
                );
            }
            GuardCommand::Set { timeout } => {
                camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.set_guard(true, Some("setPos"), timeout, Some(1))
                                .await
                                .context("Unable to set PTZ guard")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
            GuardCommand::Goto => {
                camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.goto_guard()
                                .await
                                .context("Unable to move to PTZ guard position")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
            GuardCommand::Delete => {
                camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.set_guard(false, Some("delPos"), 0, None)
                                .await
                                .context("Unable to delete PTZ guard position")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
        },
        PtzCommand::Click {
            x,
            y,
            screen_width,
            screen_height,
            width,
            height,
            speed,
        } => {
            camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.ptz_3d_click(x, y, width, height, screen_width, screen_height, speed)
                            .await
                            .context("Unable to send PTZ 3D click")?;
                        Ok(())
                    })
                })
                .await?;
        }
        PtzCommand::Autofocus { state } => {
            let enable = matches!(state, OnOff::On);
            camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.set_auto_focus(enable)
                            .await
                            .context("Unable to toggle auto-focus")?;
                        Ok(())
                    })
                })
                .await?;
        }
        PtzCommand::Position => {
            let (pan, tilt) = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.get_ptz_position()
                            .await
                            .context("Unable to read PTZ position")
                    })
                })
                .await?;
            println!("Pan: {pan} Tilt: {tilt}");
        }
    };

    Ok(())
}
