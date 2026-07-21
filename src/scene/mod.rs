//!
//! # Neolink Scene
//!
//! Drives the Baichuan host-level "scenario" / arming-mode surface
//! (cmd 603 / 604 / 605). With no argument it lists the available scene
//! ids; with an id it activates that scene; with `off` it disables scene
//! mode.
//!
//! # Usage
//!
//! ```bash
//! # List available scene ids
//! neolink scene --config=config.toml CameraName
//! # Activate scene id 1 (conventionally "away")
//! neolink scene --config=config.toml CameraName 1
//! # Disable scene mode
//! neolink scene --config=config.toml CameraName off
//! ```
//!
use anyhow::{Context, Result};

mod cmdline;

use crate::common::NeoReactor;
pub(crate) use cmdline::Opt;

/// Entry point for the scene subcommand
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;

    match opt.scene {
        Some(maybe_id) => match maybe_id {
            None | Some(0) => {
                camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.disable_scene()
                                .await
                                .context("Unable to disable camera scene mode")
                        })
                    })
                    .await?;
                println!("scene mode disabled");
            }
            Some(id) => {
                camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.set_scene(id)
                                .await
                                .context("Unable to set camera scene")
                        })
                    })
                    .await?;
                println!("scene {} activated", id);
            }
        },
        None => {
            // List available scene ids. Names are looked up per id via
            // get_scene_info; cameras that don't surface names will print
            // just the id.
            let ids = camera
                .run_task(|cam| {
                    Box::pin(async move { cam.get_scenes().await.context("Unable to list scenes") })
                })
                .await?;
            if ids.is_empty() {
                println!("(no scenes configured)");
            } else {
                for id in ids {
                    // Best-effort: failed name lookup is non-fatal — print
                    // the id alone.
                    let name = camera
                        .run_task(|cam| {
                            Box::pin(async move {
                                Ok(cam.get_scene_info(id).await.ok().and_then(|c| c.name))
                            })
                        })
                        .await
                        .unwrap_or(None);
                    match name {
                        Some(n) => println!("{}\t{}", id, n),
                        None => println!("{}", id),
                    }
                }
            }
        }
    }

    Ok(())
}
