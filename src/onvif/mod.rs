//! # Neolink ONVIF
//!
//! Implements an ONVIF Profile S bridge in front of the existing Reolink
//! camera connections. Each enabled camera becomes an independent virtual
//! ONVIF device under `/onvif/<camera-name>/...`, with its own WS-Discovery
//! announcement and stable UUID. PTZ commands are translated to the Reolink
//! BC protocol via the same `BcCamera` methods the CLI and MQTT surfaces use.
//!
//! This module is intentionally NOT gated on the `gstreamer` feature: ONVIF
//! itself only hands out the existing RTSP URL (it doesn't stream media or
//! transcode anything), so it builds and runs without GStreamer. The combined
//! `mqtt-rtsp` launcher remains `gstreamer`-gated because RTSP is.
//!
//! See `sample_config.toml` for the user-facing configuration.

use anyhow::Result;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

mod cmdline;
mod discovery;
mod server;
mod services;
mod snapshot;
mod soap;
mod state;

pub(crate) use cmdline::Opt;

use crate::common::NeoReactor;
use crate::AnyResult;
use state::OnvifState;

/// Entry point for `neolink onvif`. Boots the HTTP/SOAP server and the
/// WS-Discovery responder, then watches the config and reconciles the
/// per-camera state map whenever the config changes.
pub(crate) async fn main(_opt: Opt, reactor: NeoReactor) -> Result<()> {
    let mut cfg_rx = reactor.config().await?;
    let initial = cfg_rx.borrow_and_update().clone();

    if !initial.onvif.enabled {
        log::info!("ONVIF is disabled in config; the onvif task is idle");
        // Wait forever (until the outer cancel hits) so the joiners stay
        // balanced.
        std::future::pending::<()>().await;
        unreachable!();
    }

    let state = OnvifState::new(initial.onvif.clone(), initial.bind_port);
    state.sync_with_config(&initial, &reactor).await?;

    let cancel = CancellationToken::new();
    let mut set: JoinSet<AnyResult<()>> = JoinSet::new();

    let s_state = state.clone();
    let s_cancel = cancel.clone();
    set.spawn(async move { server::run(s_state, s_cancel).await });

    if initial.onvif.discovery {
        let d_state = state.clone();
        let d_cancel = cancel.clone();
        set.spawn(async move { discovery::run(d_state, d_cancel).await });
    }

    let r_state = state.clone();
    let r_reactor = reactor.clone();
    let r_cancel = cancel.clone();
    set.spawn(async move {
        let mut rx = cfg_rx;
        loop {
            tokio::select! {
                _ = r_cancel.cancelled() => break,
                changed = rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let new_cfg = rx.borrow().clone();
                    if let Err(e) = r_state.sync_with_config(&new_cfg, &r_reactor).await {
                        log::warn!("ONVIF: config sync failed: {e:?}");
                    }
                }
            }
        }
        Ok(())
    });

    log::info!("ONVIF bridge started");
    let r = wait_first(&mut set).await;
    cancel.cancel();
    while let Some(j) = set.join_next().await {
        if let Err(e) = j {
            log::debug!("ONVIF task join error: {e:?}");
        }
    }
    r
}

async fn wait_first(set: &mut JoinSet<AnyResult<()>>) -> Result<()> {
    while let Some(joined) = set.join_next().await {
        match joined {
            Err(e) => {
                log::error!("ONVIF task panicked: {e:?}");
                return Err(anyhow::anyhow!(e));
            }
            Ok(Err(e)) => {
                log::error!("ONVIF task failed: {e:?}");
                return Err(e);
            }
            Ok(Ok(())) => continue,
        }
    }
    Ok(())
}
