///
/// # Neolink WiFi
///
/// This module prints the WiFi info of the camera (current SSID, signal,
/// link type, and optional scan list) as XML.
///
/// # Usage
///
/// ```bash
/// # Just the current SSID + RSSI:
/// neolink wifi --config=config.toml CameraName
///
/// # Full WiFi scan list:
/// neolink wifi --config=config.toml CameraName --scan
/// ```
///
use anyhow::{Context, Result};

mod cmdline;

use crate::common::NeoReactor;

pub(crate) use cmdline::Opt;

/// Entry point for the wifi subcommand
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;
    let scan = opt.scan;

    let wifi = camera
        .run_task(move |cam| {
            Box::pin(async move {
                if scan {
                    cam.get_wifi()
                        .await
                        .context("Unable to get camera WiFi scan")
                } else {
                    cam.get_wifi_signal()
                        .await
                        .context("Unable to get camera WiFi signal")
                }
            })
        })
        .await?;

    let ser = String::from_utf8({
        let mut buf = bytes::BytesMut::new();
        quick_xml::se::to_writer(&mut buf, &wifi).expect("Should Ser the struct");
        buf.to_vec()
    })
    .expect("Should be UTF8");
    println!("{}", ser);

    Ok(())
}
