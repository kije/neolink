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

use std::future::Future;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::task::JoinSet;
use tokio::time::{sleep, Instant};
use tokio_util::sync::CancellationToken;

mod cmdline;
mod discovery;
mod events;
mod server;
mod services;
mod snapshot;
mod soap;
mod state;

pub(crate) use cmdline::Opt;

use crate::common::NeoReactor;
use crate::AnyResult;
use state::OnvifState;

/// How long to wait before restarting a task that came back. Doubles on every
/// consecutive restart so a permanently broken bind (wrong address, port taken)
/// settles into a slow retry instead of spinning.
const RESTART_BACKOFF_MIN: Duration = Duration::from_secs(1);
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A task that stayed up at least this long counts as healthy: its next failure
/// starts over from `RESTART_BACKOFF_MIN`.
const RESTART_BACKOFF_RESET: Duration = Duration::from_secs(60);

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
    let cancel = CancellationToken::new();
    state.set_cancel(cancel.clone()).await;
    state.sync_with_config(&initial, &reactor).await?;
    let mut set: JoinSet<(&'static str, AnyResult<()>)> = JoinSet::new();

    // The HTTP server is the whole point of the bridge: a VMS that loses it
    // just sees connection refused (or a 502 from whatever proxies it), with no
    // hint that neolink is otherwise healthy. Supervise it so a failed bind or
    // an accept loop that falls over comes back on its own.
    let s_state = state.clone();
    let s_cancel = cancel.clone();
    set.spawn(async move {
        (
            "HTTP server",
            supervise("HTTP server", s_cancel.clone(), move || {
                server::run(s_state.clone(), s_cancel.clone())
            })
            .await,
        )
    });

    if initial.onvif.discovery {
        let d_state = state.clone();
        let d_cancel = cancel.clone();
        set.spawn(async move {
            (
                "WS-Discovery",
                supervise("WS-Discovery", d_cancel.clone(), move || {
                    discovery::run(d_state.clone(), d_cancel.clone())
                })
                .await,
            )
        });
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
                        // The config publisher is gone. Keep serving the config
                        // we already have rather than taking the bridge down;
                        // only the outer cancel ends this task.
                        log::warn!(
                            "ONVIF: config watch closed; the bridge keeps serving its current config"
                        );
                        r_cancel.cancelled().await;
                        break;
                    }
                    let new_cfg = rx.borrow().clone();
                    if let Err(e) = r_state.sync_with_config(&new_cfg, &r_reactor).await {
                        log::warn!("ONVIF: config sync failed: {e:?}");
                    }
                }
            }
        }
        ("config watcher", Ok(()))
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

/// Run `task` until `cancel` fires, restarting it whenever it comes back.
///
/// Neither the HTTP server nor the discovery responder is supposed to return
/// before cancellation, so a clean `Ok(())` is just as much a failure as an
/// `Err`: the service it was providing is gone either way. Both get logged and
/// restarted.
async fn supervise<F, Fut>(name: &'static str, cancel: CancellationToken, mut task: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AnyResult<()>>,
{
    let mut backoff = RESTART_BACKOFF_MIN;
    loop {
        let started = Instant::now();
        let outcome = task().await;
        if cancel.is_cancelled() {
            return Ok(());
        }
        if started.elapsed() >= RESTART_BACKOFF_RESET {
            backoff = RESTART_BACKOFF_MIN;
        }
        match outcome {
            Ok(()) => log::error!("ONVIF {name} exited unexpectedly; restarting in {backoff:?}"),
            Err(e) => log::error!("ONVIF {name} failed: {e:?}; restarting in {backoff:?}"),
        }
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(RESTART_BACKOFF_MAX);
    }
}

/// Wait for the first ONVIF task to come back.
///
/// None of them should: the supervised tasks restart themselves and the config
/// watcher runs until cancelled, and `cancel` is only fired once this function
/// has returned. So anything arriving here — a panic, an error, or a clean
/// `Ok(())` — means part of the bridge has stopped serving. Swallowing the
/// clean case would leave the process up and apparently healthy while its ONVIF
/// endpoints answer nothing at all.
async fn wait_first(set: &mut JoinSet<(&'static str, AnyResult<()>)>) -> Result<()> {
    match set.join_next().await {
        None => Err(anyhow!("ONVIF has no tasks to run")),
        Some(Err(e)) => {
            log::error!("ONVIF task panicked: {e:?}");
            Err(anyhow!(e))
        }
        Some(Ok((name, Err(e)))) => {
            log::error!("ONVIF {name} task failed: {e:?}");
            Err(e.context(format!("ONVIF {name} task failed")))
        }
        Some(Ok((name, Ok(())))) => {
            log::error!("ONVIF {name} task exited unexpectedly; stopping the ONVIF bridge");
            Err(anyhow!("ONVIF {name} task exited unexpectedly"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A supervised task returning `Ok(())` is not a task that finished its
    /// job: the HTTP server only stops serving when something went wrong, so
    /// the supervisor has to bring it back rather than let the endpoint go
    /// quiet while the rest of neolink carries on.
    #[tokio::test]
    async fn a_task_that_returns_cleanly_is_restarted() {
        let cancel = CancellationToken::new();
        let runs = Arc::new(AtomicUsize::new(0));

        let task_runs = runs.clone();
        let task_cancel = cancel.clone();
        supervise("test", cancel, move || {
            let runs = task_runs.clone();
            let cancel = task_cancel.clone();
            async move {
                if runs.fetch_add(1, Ordering::SeqCst) >= 1 {
                    cancel.cancel();
                }
                Ok(())
            }
        })
        .await
        .expect("supervise ends cleanly once cancelled");

        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_cancelled_supervisor_does_not_restart_its_task() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let runs = Arc::new(AtomicUsize::new(0));

        let task_runs = runs.clone();
        supervise("test", cancel, move || {
            let runs = task_runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .await
        .expect("supervise ends cleanly once cancelled");

        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_clean_exit_is_reported_as_a_failure() {
        let mut set: JoinSet<(&'static str, AnyResult<()>)> = JoinSet::new();
        set.spawn(async { ("HTTP server", Ok(())) });

        let err = wait_first(&mut set)
            .await
            .expect_err("a task that stopped serving is a failure");
        assert!(err.to_string().contains("HTTP server"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_task_is_named_in_the_error() {
        let mut set: JoinSet<(&'static str, AnyResult<()>)> = JoinSet::new();
        set.spawn(async { ("WS-Discovery", Err(anyhow!("socket is gone"))) });

        let err = wait_first(&mut set).await.expect_err("the task failed");
        assert!(err.to_string().contains("WS-Discovery"), "{err}");
        assert!(format!("{err:?}").contains("socket is gone"), "{err:?}");
    }
}
