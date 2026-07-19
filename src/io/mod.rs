///
/// # Neolink I/O
///
/// Watches the I/O input contacts (cmd 677 push) on NVRs / hubs and
/// prints each state change. Useful for door sensors / gate contacts
/// wired into the alarm-in terminals.
///
/// # Usage
///
/// ```bash
/// neolink io --config=config.toml HubName
/// ```
///
use anyhow::{Context, Result};

mod cmdline;

use crate::common::NeoReactor;

pub(crate) use cmdline::Opt;

/// Entry point for the io subcommand
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;

    camera
        .run_task(|cam| {
            Box::pin(async move {
                let mut rx = cam
                    .io_input_state_stream()
                    .await
                    .context("Unable to subscribe to I/O input")?;
                while let Some((index, on)) = rx.recv().await {
                    println!("io {} {}", index, if on { "on" } else { "off" });
                }
                Ok(())
            })
        })
        .await?;

    Ok(())
}
