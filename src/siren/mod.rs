///
/// # Neolink Siren
///
/// Trigger the siren, configure scheduled siren windows, or watch the
/// siren-status push stream.
///
/// # Usage
///
/// ```bash
/// # Fire the siren for 5 seconds:
/// neolink siren --config=config.toml CameraName on --duration 5
///
/// # Stop the siren:
/// neolink siren --config=config.toml CameraName off
///
/// # Configure a scheduled siren (10 second siren, fires once):
/// neolink siren --config=config.toml CameraName schedule --times 1 --duration 10
///
/// # Watch siren-status push events:
/// neolink siren --config=config.toml CameraName watch
/// ```
///
use anyhow::{Context, Result};

mod cmdline;

use crate::common::NeoReactor;

pub(crate) use cmdline::{Action, Opt};

/// Entry point for the siren subcommand
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;

    match opt.action {
        Action::On { duration } => {
            camera
                .run_task(move |cam| {
                    Box::pin(async move {
                        cam.fire_siren(duration)
                            .await
                            .context("Unable to fire siren")
                    })
                })
                .await?;
        }
        Action::Off => {
            camera
                .run_task(|cam| {
                    Box::pin(async move { cam.stop_siren().await.context("Unable to stop siren") })
                })
                .await?;
        }
        Action::Schedule {
            times,
            duration,
            disable,
        } => {
            let enable = !disable;
            camera
                .run_task(move |cam| {
                    Box::pin(async move {
                        cam.schedule_siren(times, duration, enable)
                            .await
                            .context("Unable to schedule siren")
                    })
                })
                .await?;
        }
        Action::Watch => {
            camera
                .run_task(|cam| {
                    Box::pin(async move {
                        let mut rx = cam
                            .siren_status_stream()
                            .await
                            .context("Unable to subscribe to siren status")?;
                        while let Some(state) = rx.recv().await {
                            println!("siren {}", if state { "on" } else { "off" });
                        }
                        Ok(())
                    })
                })
                .await?;
        }
    }

    Ok(())
}
