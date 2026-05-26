//! Battery-camera idle-close lifecycle.
//!
//! Mirrors `_battery_close_task` in `reolink_aio/baichuan/baichuan.py`.
//! The driver behavior is:
//!
//! 1. After login, if `DeviceInfo.sleep == true` (i.e. the camera is a
//!    battery model), mark the connection as "battery-managed".
//! 2. While battery-managed, track in-flight commands. When the in-flight
//!    count returns to zero, start a 5-second timer. If no new command
//!    arrives before it fires, close the TCP socket.
//! 3. The next command transparently reopens it.
//!
//! In addition, mirror `NONE_WAKING_COMMANDS` from `reolink_aio/const.py`:
//! a small allowlist of `cmd_id`s that the camera will service without
//! waking from deep sleep. Issuing one of these *should not* re-arm the
//! socket nor force a wake.
//!
//! This module is intentionally I/O-free. It exposes:
//!
//! * `BatteryLifecycle` — interior-mutable state holder that callers can
//!   poke from inside a `&BcCamera` method (no &mut required).
//! * `NONE_WAKING_COMMANDS` — the cmd_id allowlist.
//!
//! The actual "close the TCP socket" / "reopen on demand" wiring lives
//! in `BcConnection`; this module is the policy layer.
use crate::bc::model::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// Default idle window before we close a battery camera's TCP socket.
///
/// Mirrors `BATTERY_KEEP_ALLIVE_INTERVAL` semantics in `reolink_aio`
/// (i.e. "5 seconds after the last in-flight command").
pub const BATTERY_IDLE_CLOSE_SECS: u64 = 5;

/// Command IDs that the camera will respond to without coming out of
/// deep sleep. Mirror of `NONE_WAKING_COMMANDS` in `reolink_aio/const.py`.
///
/// Sending one of these to a sleeping battery camera should not force
/// a wake; the camera's low-power firmware path handles them. The exact
/// list matches the upstream library:
///
/// * `MSG_ID_UID`            (114)
/// * `MSG_ID_GET_LED_STATUS` (208)
/// * `MSG_ID_BATTERY_INFO`   (253)
/// * `594` PreRecord query
///
/// `WifiSignal` is referenced in the issue as cmd 115 but is not present
/// in `model.rs`; we include it here so that connection-layer code can
/// gate on it once it is added without needing a downstream constant.
pub const NONE_WAKING_COMMANDS: &[u32] = &[
    MSG_ID_UID,
    115, /* WifiSignal */
    MSG_ID_GET_LED_STATUS,
    MSG_ID_BATTERY_INFO,
    594, /* PreRecord */
];

/// Returns `true` if `cmd_id` may be issued to a sleeping battery camera
/// without forcing a wake.
pub fn is_non_waking(cmd_id: u32) -> bool {
    NONE_WAKING_COMMANDS.contains(&cmd_id)
}

/// Battery-camera idle-close state.
///
/// All state transitions are lock-free and safe to call from `&self` (no
/// `&mut`). The state machine is:
///
/// ```text
///   not_battery ─ enable() ─▶ battery
///   battery + in_flight==0 + idle_for≥5s ─▶ should_close
///   should_close + arm_command() ─▶ battery
/// ```
#[derive(Debug)]
pub struct BatteryLifecycle {
    /// True once we've observed that this is a battery camera.
    enabled: AtomicBool,
    /// In-flight command count.
    in_flight: AtomicUsize,
    /// Idle window before close.
    idle_close: Duration,
    /// Notify drivers when the in-flight count transitions to zero so a
    /// background timer can race the next command to close the socket.
    idle_notify: Notify,
}

impl BatteryLifecycle {
    /// Construct a new lifecycle in the disabled state.
    pub fn new() -> Self {
        Self::with_idle_window(Duration::from_secs(BATTERY_IDLE_CLOSE_SECS))
    }

    /// Construct with a custom idle window. Useful in tests.
    pub fn with_idle_window(idle_close: Duration) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            idle_close,
            idle_notify: Notify::new(),
        }
    }

    /// Mark this connection as a battery camera (e.g. after observing
    /// `DeviceInfo.sleep == true`).
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    /// Mark this connection as not battery managed.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    /// Returns whether this connection is currently battery managed.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// The configured idle window.
    pub fn idle_window(&self) -> Duration {
        self.idle_close
    }

    /// Record that a command is in-flight. Returns a guard that decrements
    /// the count when dropped.
    ///
    /// `cmd_id == 0` and non-waking commands are not counted, so they
    /// neither prevent nor reset the idle close timer.
    pub fn track(&self, cmd_id: u32) -> InFlightGuard<'_> {
        let counted = !is_non_waking(cmd_id);
        if counted {
            self.in_flight.fetch_add(1, Ordering::AcqRel);
        }
        InFlightGuard { lc: self, counted }
    }

    /// Number of in-flight commands currently being tracked.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Wait until the in-flight count next transitions to zero.
    ///
    /// Drivers can race this against the next outgoing command to decide
    /// whether the socket should be closed.
    pub async fn wait_idle(&self) {
        if self.in_flight() == 0 {
            return;
        }
        // Tight loop on the notification — `decrement` posts to `idle_notify`
        // whenever it observes a transition to zero.
        loop {
            let notified = self.idle_notify.notified();
            if self.in_flight() == 0 {
                return;
            }
            notified.await;
            if self.in_flight() == 0 {
                return;
            }
        }
    }
}

impl Default for BatteryLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard that decrements the in-flight count when dropped.
#[must_use = "InFlightGuard must be held for the lifetime of the command"]
pub struct InFlightGuard<'a> {
    lc: &'a BatteryLifecycle,
    counted: bool,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if !self.counted {
            return;
        }
        let prev = self.lc.in_flight.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Transition to zero — wake any idle waiters.
            self.lc.idle_notify.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_starts_disabled() {
        let lc = BatteryLifecycle::new();
        assert!(!lc.is_enabled());
    }

    #[test]
    fn enable_disable_toggles_state() {
        let lc = BatteryLifecycle::new();
        lc.enable();
        assert!(lc.is_enabled());
        lc.disable();
        assert!(!lc.is_enabled());
    }

    #[test]
    fn non_waking_commands_are_not_tracked() {
        let lc = BatteryLifecycle::new();
        let _g = lc.track(MSG_ID_GET_LED_STATUS);
        assert_eq!(lc.in_flight(), 0);
    }

    #[test]
    fn regular_commands_are_tracked() {
        let lc = BatteryLifecycle::new();
        let g = lc.track(MSG_ID_VIDEO);
        assert_eq!(lc.in_flight(), 1);
        drop(g);
        assert_eq!(lc.in_flight(), 0);
    }

    #[test]
    fn nested_commands_count_correctly() {
        let lc = BatteryLifecycle::new();
        let g1 = lc.track(MSG_ID_VIDEO);
        let g2 = lc.track(MSG_ID_PTZ_CONTROL);
        assert_eq!(lc.in_flight(), 2);
        drop(g1);
        assert_eq!(lc.in_flight(), 1);
        drop(g2);
        assert_eq!(lc.in_flight(), 0);
    }

    #[test]
    fn known_non_waking_ids() {
        assert!(is_non_waking(MSG_ID_GET_LED_STATUS));
        assert!(is_non_waking(MSG_ID_BATTERY_INFO));
        assert!(is_non_waking(594));
        assert!(is_non_waking(115));
        assert!(!is_non_waking(MSG_ID_VIDEO));
        assert!(!is_non_waking(MSG_ID_PTZ_CONTROL));
    }

    #[tokio::test]
    async fn wait_idle_returns_immediately_when_zero() {
        let lc = BatteryLifecycle::new();
        // Already idle — should return without blocking.
        lc.wait_idle().await;
    }

    #[tokio::test]
    async fn wait_idle_wakes_on_drop() {
        use std::sync::Arc;
        let lc = Arc::new(BatteryLifecycle::new());
        let g = lc.track(MSG_ID_VIDEO);
        let lc2 = lc.clone();
        let waiter = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(2), lc2.wait_idle()).await
        });
        // Yield so the waiter installs the notification.
        tokio::task::yield_now().await;
        drop(g);
        let res = waiter.await.unwrap();
        assert!(
            res.is_ok(),
            "wait_idle should have completed once in_flight hit 0"
        );
    }
}
