//!
//! # Neolink Privacy
//!
//! Drives the Baichuan privacy-mode shutter (cmd 622 / 623). With no on|off
//! argument it queries the current state; with `on` / `off` it sets it.
//!
//! # Usage
//!
//! ```bash
//! # Query
//! neolink privacy --config=config.toml CameraName
//! # Set
//! neolink privacy --config=config.toml CameraName on
//! neolink privacy --config=config.toml CameraName off
//! ```
//!
use anyhow::{Context, Result};

mod cmdline;

use crate::common::NeoReactor;
pub(crate) use cmdline::Opt;

/// Entry point for the privacy subcommand
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;

    if let Some(enable) = opt.on {
        camera
            .run_task(|cam| {
                Box::pin(async move {
                    cam.set_privacy_mode(enable)
                        .await
                        .context("Unable to set camera privacy mode")
                })
            })
            .await?;
        println!("privacy mode {}", if enable { "on" } else { "off" });
    } else {
        let state = camera
            .run_task(|cam| {
                Box::pin(async move {
                    cam.get_privacy_mode()
                        .await
                        .context("Unable to get camera privacy mode")
                })
            })
            .await?;
        println!("{}", if state { "on" } else { "off" });
    }

    Ok(())
}
