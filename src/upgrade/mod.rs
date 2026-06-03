///
/// # Neolink Upgrade
///
/// This module handles the `upgrade` subcommand which (once the wire
/// format is confirmed) will upload a firmware image to the camera and
/// trigger a flash.
///
/// # WARNING - SCAFFOLDING ONLY
///
/// The underlying `BcCamera::upgrade_firmware` call is currently
/// unimplemented (returns `NotImplemented`) because the Baichuan cmd_ids
/// for firmware upgrade have not been captured. See
/// `docs/baichuan-lifecycle.md`.
///
/// # Usage
///
/// ```bash
/// # Dry-run: validate the file, compute its hash, do not talk to the camera.
/// neolink upgrade --config=config.toml --yes-i-am-sure --dry-run CameraName firmware.pak
///
/// # Live (will currently fail until wire format is known):
/// neolink upgrade --config=config.toml --yes-i-am-sure CameraName firmware.pak
/// ```
use anyhow::{anyhow, Context, Result};
use log::*;

mod cmdline;

use crate::common::NeoReactor;
pub(crate) use cmdline::Opt;

/// Entry point for the upgrade subcommand.
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    if !opt.yes_i_am_sure {
        return Err(anyhow!(
            "Refusing to proceed without --yes-i-am-sure. Firmware upgrade is \
             a destructive operation that can brick the camera if it goes \
             wrong, and the wire format used here is NOT YET CONFIRMED. \
             Re-run with --yes-i-am-sure if you understand the risk. See \
             docs/baichuan-lifecycle.md."
        ));
    }

    warn!("================================================================");
    warn!("  neolink upgrade: SCAFFOLDING ONLY");
    warn!("  The exact Baichuan cmd_ids for firmware upgrade have not");
    warn!("  been confirmed by a Wireshark capture. This command is NOT");
    warn!("  yet known to work and will refuse to transmit until the");
    warn!("  wire format is filled in. See docs/baichuan-lifecycle.md");
    warn!("================================================================");

    let firmware_path = opt.firmware.clone();
    let dry_run = opt.dry_run;

    let camera = reactor.get(&opt.camera).await?;

    camera
        .run_task(move |camera| {
            let firmware_path = firmware_path.clone();
            Box::pin(async move {
                let preflight = camera
                    .upgrade_firmware_preflight(&firmware_path)
                    .await
                    .context("Firmware pre-flight check failed")?;

                info!(
                    "upgrade pre-flight: file={:?} size={} bytes md5={}",
                    preflight.file_name, preflight.size, preflight.md5_hex
                );

                if dry_run {
                    info!("--dry-run: pre-flight passed, not transmitting to camera.");
                    return Ok(());
                }

                camera
                    .upgrade_firmware(&firmware_path)
                    .await
                    .context("Firmware upgrade failed")
            })
        })
        .await?;

    Ok(())
}
