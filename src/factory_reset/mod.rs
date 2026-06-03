///
/// # Neolink Factory Reset
///
/// This module handles the `factory-reset` subcommand which (once the
/// wire format is confirmed) will restore the camera to factory
/// defaults.
///
/// # WARNING - SCAFFOLDING ONLY
///
/// The underlying `BcCamera::factory_reset` call is currently
/// unimplemented (returns `NotImplemented`) because the Baichuan cmd_id
/// for factory reset has not been captured. See
/// `docs/baichuan-lifecycle.md`.
///
/// # Usage
///
/// ```bash
/// neolink factory-reset --config=config.toml --yes-i-am-sure CameraName
/// ```
use anyhow::{anyhow, Context, Result};
use log::*;

mod cmdline;

use crate::common::NeoReactor;
pub(crate) use cmdline::Opt;

/// Entry point for the factory-reset subcommand.
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    if !opt.yes_i_am_sure {
        return Err(anyhow!(
            "Refusing to proceed without --yes-i-am-sure. Factory reset wipes \
             credentials, schedules and paired notifications, and the wire \
             format used here is NOT YET CONFIRMED. Re-run with \
             --yes-i-am-sure if you understand the risk. See \
             docs/baichuan-lifecycle.md."
        ));
    }

    warn!("================================================================");
    warn!("  neolink factory-reset: SCAFFOLDING ONLY");
    warn!("  The exact Baichuan cmd_id for factory reset has not been");
    warn!("  confirmed by a Wireshark capture. This command is NOT yet");
    warn!("  known to work and will refuse to transmit until the wire");
    warn!("  format is filled in. See docs/baichuan-lifecycle.md");
    warn!("================================================================");

    let keep_network = opt.keep_network;
    let camera = reactor.get(&opt.camera).await?;

    camera
        .run_task(move |camera| {
            Box::pin(async move {
                camera
                    .factory_reset(keep_network)
                    .await
                    .context("Could not send factory-reset command to the camera")
            })
        })
        .await?;

    Ok(())
}
