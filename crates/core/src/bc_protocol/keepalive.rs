use super::{BcCamera, Result};
use crate::bc::model::*;
use std::sync::Mutex;
use std::time::Duration;

/// Default keepalive cadence in seconds when a camera is actively subscribed
/// (e.g. streaming or holding open a long-poll subscription).
///
/// Mirrors `KEEP_ALLIVE_INTERVAL` from `reolink_aio/const.py`.
pub const DEFAULT_KEEPALIVE_SUBSCRIBED_SECS: u64 = 30;

/// Default keepalive cadence in seconds when a camera is otherwise idle.
///
/// `reolink_aio` uses a longer fall-back interval when nothing is subscribed
/// because the camera itself will heartbeat us; this just keeps NAT
/// translations / TCP keepalives warm.
pub const DEFAULT_KEEPALIVE_IDLE_SECS: u64 = 60;

/// Hard floor for the adapted interval after repeated disconnects.
///
/// Matches `MIN_KEEP_ALLIVE_INTERVAL` in `reolink_aio`.
pub const MIN_KEEPALIVE_SECS: u64 = 9;

/// Once the connection has been stable for this many seconds we permit
/// the adapted interval to step back up towards the default.
pub const STABILITY_RECOVERY_SECS: u64 = 60 * 60; // 1h

/// One adaptive step (in seconds) that we walk back towards the default
/// after a sustained period of stability.
pub const RECOVERY_STEP_SECS: u64 = 1;

/// State for the adaptive keepalive scheduler.
///
/// The scheduler is intentionally pure — it has no I/O. Callers feed it
/// observations (`notice_disconnect`, `notice_stable_period`) and it
/// returns the interval to use for the next outgoing ping. This makes the
/// state machine trivially unit-testable.
///
/// Two cadences are tracked: a "subscribed" cadence (used when at least one
/// long-lived subscription is open) and an "idle" cadence. The adaptation
/// from disconnects always targets the currently-active cadence.
#[derive(Debug, Clone)]
pub struct AdaptiveKeepalive {
    /// Currently adapted interval in seconds.
    current_secs: u64,
    /// Default cadence when subscribed.
    default_subscribed_secs: u64,
    /// Default cadence when not subscribed.
    default_idle_secs: u64,
    /// Floor for adaptation.
    min_secs: u64,
    /// Stability window after which we step the cadence back up.
    stability_recovery_secs: u64,
    /// True while at least one long-lived subscription is open.
    subscribed: bool,
}

impl Default for AdaptiveKeepalive {
    fn default() -> Self {
        Self::new(
            DEFAULT_KEEPALIVE_SUBSCRIBED_SECS,
            DEFAULT_KEEPALIVE_IDLE_SECS,
            MIN_KEEPALIVE_SECS,
            STABILITY_RECOVERY_SECS,
        )
    }
}

impl AdaptiveKeepalive {
    /// Construct a new scheduler with the given parameters.
    pub fn new(
        default_subscribed_secs: u64,
        default_idle_secs: u64,
        min_secs: u64,
        stability_recovery_secs: u64,
    ) -> Self {
        Self {
            current_secs: default_idle_secs,
            default_subscribed_secs,
            default_idle_secs,
            min_secs,
            stability_recovery_secs,
            subscribed: false,
        }
    }

    /// The target (un-adapted) cadence for the current subscription state.
    fn target_secs(&self) -> u64 {
        if self.subscribed {
            self.default_subscribed_secs
        } else {
            self.default_idle_secs
        }
    }

    /// Update subscription state.
    ///
    /// Newly entering the subscribed state may immediately shorten the
    /// cadence if the existing adapted value is larger than the new target.
    pub fn set_subscribed(&mut self, subscribed: bool) {
        self.subscribed = subscribed;
        let target = self.target_secs();
        if self.current_secs > target {
            self.current_secs = target;
        }
    }

    /// Returns the interval that should be used for the next ping.
    pub fn next_interval(&self) -> Duration {
        Duration::from_secs(self.current_secs)
    }

    /// Returns the current adapted cadence in seconds.
    pub fn current_secs(&self) -> u64 {
        self.current_secs
    }

    /// Inform the scheduler that the camera disconnected after `silence_secs`
    /// of silence on the wire. The next interval will be
    /// `max(min, silence_secs - 2)`.
    pub fn notice_disconnect(&mut self, silence_secs: u64) {
        let proposed = silence_secs.saturating_sub(2).max(self.min_secs);
        // Only ratchet downwards.
        if proposed < self.current_secs {
            self.current_secs = proposed;
        }
    }

    /// Inform the scheduler that the connection has been stable for
    /// `stable_secs` seconds. For every `stability_recovery_secs` of
    /// uninterrupted stability we step the cadence one step back towards
    /// the (subscription-dependent) target.
    pub fn notice_stable_period(&mut self, stable_secs: u64) {
        if self.stability_recovery_secs == 0 {
            return;
        }
        let target = self.target_secs();
        if self.current_secs >= target {
            return;
        }
        let steps = stable_secs / self.stability_recovery_secs;
        if steps == 0 {
            return;
        }
        let step = RECOVERY_STEP_SECS.max(1);
        let delta = steps.saturating_mul(step);
        self.current_secs = self.current_secs.saturating_add(delta).min(target);
    }

    /// Hard reset to default state. Useful at reconnect time.
    pub fn reset(&mut self) {
        self.current_secs = self.target_secs();
    }
}

impl BcCamera {
    /// Create a handler to respond to keep alive messages
    /// These messages are sent by the camera so we listen to
    /// a message ID rather than setting a message number and
    /// responding to it
    pub async fn keepalive(&self) -> Result<()> {
        let connection = self.get_connection();
        connection
            .handle_msg(MSG_ID_UDP_KEEP_ALIVE, |bc| {
                Box::pin(async move {
                    Some(Bc {
                        meta: BcMeta {
                            msg_id: MSG_ID_UDP_KEEP_ALIVE,
                            channel_id: bc.meta.channel_id,
                            msg_num: bc.meta.msg_num,
                            stream_type: bc.meta.stream_type,
                            response_code: 200,
                            class: 0x6414,
                        },
                        body: BcBody::ModernMsg(ModernMsg {
                            ..Default::default()
                        }),
                    })
                })
            })
            .await?;
        Ok(())
    }

    /// Returns a handle to the adaptive keepalive scheduler for this camera.
    ///
    /// The scheduler is interior-mutable behind a `Mutex` so callers can
    /// adjust it (e.g. report disconnects) without taking a mutable borrow
    /// on the camera itself.
    pub fn keepalive_policy(&self) -> &Mutex<AdaptiveKeepalive> {
        &self.keepalive_policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_starts_at_idle_cadence() {
        let ka = AdaptiveKeepalive::default();
        assert_eq!(ka.next_interval(), Duration::from_secs(60));
    }

    #[test]
    fn subscribing_shortens_to_30s() {
        let mut ka = AdaptiveKeepalive::default();
        ka.set_subscribed(true);
        assert_eq!(ka.next_interval(), Duration::from_secs(30));
    }

    #[test]
    fn disconnect_after_silence_ratchets_down_by_two() {
        let mut ka = AdaptiveKeepalive::default();
        ka.set_subscribed(true);
        // Disconnect happened after 25s of silence; next interval is 23s.
        ka.notice_disconnect(25);
        assert_eq!(ka.current_secs(), 23);
    }

    #[test]
    fn disconnect_floored_at_min() {
        let mut ka = AdaptiveKeepalive::default();
        ka.notice_disconnect(5); // would propose 3
        assert_eq!(ka.current_secs(), MIN_KEEPALIVE_SECS);
    }

    #[test]
    fn disconnect_is_monotonic_downwards() {
        let mut ka = AdaptiveKeepalive::default();
        ka.set_subscribed(true);
        ka.notice_disconnect(20); // -> 18
        ka.notice_disconnect(28); // would propose 26, but we already have 18
        assert_eq!(ka.current_secs(), 18);
    }

    #[test]
    fn stability_walks_back_towards_default() {
        let mut ka = AdaptiveKeepalive::default();
        ka.set_subscribed(true);
        ka.notice_disconnect(15); // -> 13
                                  // One hour of stability -> +1 step
        ka.notice_stable_period(STABILITY_RECOVERY_SECS);
        assert_eq!(ka.current_secs(), 14);
        // Five more hours -> +5 (clamped to default 30)
        ka.notice_stable_period(5 * STABILITY_RECOVERY_SECS);
        assert_eq!(ka.current_secs(), 19);
    }

    #[test]
    fn stability_does_not_exceed_target() {
        let mut ka = AdaptiveKeepalive::default();
        ka.set_subscribed(true);
        ka.notice_disconnect(28); // -> 26
        ka.notice_stable_period(1_000 * STABILITY_RECOVERY_SECS);
        assert_eq!(ka.current_secs(), 30);
    }

    #[test]
    fn reset_returns_to_target_cadence() {
        let mut ka = AdaptiveKeepalive::default();
        ka.notice_disconnect(11); // -> 9
        assert_eq!(ka.current_secs(), 9);
        ka.reset();
        assert_eq!(ka.current_secs(), DEFAULT_KEEPALIVE_IDLE_SECS);
    }
}
