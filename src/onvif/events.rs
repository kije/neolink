//! Per-camera ONVIF events manager.
//!
//! Implements a single motion topic (`tns1:VideoSource/MotionAlarm`) translated
//! from the existing `NeoInstance::motion()` watch. Notifications are delivered
//! through ONVIF PullPoint subscriptions: clients call
//! `CreatePullPointSubscription`, then poll the returned subscription URI with
//! `PullMessages`. The subscription endpoint also serves `Renew` and
//! `Unsubscribe`.
//!
//! Why PullPoint and not a push (NotificationConsumer) subscription? Pull is
//! universally supported by the VMS clients we care about (Home Assistant,
//! Frigate, BlueIris, Synology, Agent DVR) and it avoids opening an outbound
//! HTTP connection from the bridge to a client. Push is a Profile T option,
//! not a requirement.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::common::{MdState, NeoInstance};

/// How long a freshly created subscription remains valid before it must be
/// renewed. The ONVIF default for cameras is 1 minute; we use 5 to cut down on
/// chatter when a VMS forgets to renew on time.
const DEFAULT_SUBSCRIPTION_TTL: Duration = Duration::from_secs(5 * 60);

/// Hard ceiling on a subscription's lifetime regardless of what the client
/// asked for in `InitialTerminationTime` / `Renew`. Prevents a misbehaving
/// client from holding a slot forever.
const MAX_SUBSCRIPTION_TTL: Duration = Duration::from_secs(60 * 60);

/// Hard cap on pending notifications per subscription. Beyond this we drop
/// the oldest. A `PullMessages` call that drains the queue clears it.
const MAX_PENDING_MESSAGES: usize = 256;

/// Hard cap on simultaneous subscriptions per camera.
const MAX_SUBSCRIPTIONS_PER_CAMERA: usize = 32;

/// Hard ceiling on the `Timeout` a client can request in `PullMessages`.
/// We hold the request open for up to this long if nothing has happened.
pub(crate) const MAX_PULL_TIMEOUT: Duration = Duration::from_secs(60);

/// A single notification message kept in a subscription's queue.
#[derive(Clone, Debug)]
pub(crate) struct Notification {
    pub(crate) utc_time: DateTime<Utc>,
    pub(crate) topic: &'static str,
    pub(crate) source_name: &'static str,
    pub(crate) source_value: String,
    pub(crate) data_name: &'static str,
    pub(crate) data_value: String,
    pub(crate) property_op: &'static str,
}

pub(crate) struct Subscription {
    pub(crate) id: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) terminates_at: Mutex<DateTime<Utc>>,
    pending: Mutex<VecDeque<Notification>>,
    /// Pinged when a new notification is enqueued so a long-polling
    /// `PullMessages` can return early.
    notify: Notify,
}

impl Subscription {
    fn new(id: String, ttl: Duration) -> Self {
        let now = Utc::now();
        let term = now
            + chrono::Duration::from_std(ttl.min(MAX_SUBSCRIPTION_TTL))
                .unwrap_or_else(|_| chrono::Duration::seconds(60));
        Self {
            id,
            created_at: now,
            terminates_at: Mutex::new(term),
            pending: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        }
    }

    async fn enqueue(&self, msg: Notification) {
        let mut q = self.pending.lock().await;
        if q.len() >= MAX_PENDING_MESSAGES {
            q.pop_front();
        }
        q.push_back(msg);
        drop(q);
        self.notify.notify_waiters();
    }

    /// Pull up to `limit` notifications, blocking until at least one arrives
    /// or `timeout` elapses. The current termination time is refreshed if it
    /// was past — the caller can then check expiry separately.
    pub(crate) async fn pull(&self, limit: usize, timeout: Duration) -> Vec<Notification> {
        let deadline = Instant::now() + timeout.min(MAX_PULL_TIMEOUT);
        loop {
            {
                let mut q = self.pending.lock().await;
                if !q.is_empty() {
                    let take = q.len().min(limit.max(1));
                    return q.drain(..take).collect();
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Vec::new();
            }
            let wait = deadline - now;
            // Either a new message arrives, or the deadline fires.
            tokio::select! {
                _ = self.notify.notified() => {},
                _ = tokio::time::sleep(wait) => return Vec::new(),
            }
        }
    }

    pub(crate) async fn renew(&self, ttl: Duration) -> DateTime<Utc> {
        let new_term = Utc::now()
            + chrono::Duration::from_std(ttl.min(MAX_SUBSCRIPTION_TTL))
                .unwrap_or_else(|_| chrono::Duration::seconds(60));
        *self.terminates_at.lock().await = new_term;
        new_term
    }

    pub(crate) async fn termination_time(&self) -> DateTime<Utc> {
        *self.terminates_at.lock().await
    }
}

/// The per-camera events manager. Lazily started: the motion listener task
/// is only spawned once the first PullPoint subscription is created and is
/// torn down with the rest of the bridge through the cancel token.
pub(crate) struct EventsManager {
    cam_name: String,
    instance: NeoInstance,
    cancel: CancellationToken,
    subs: RwLock<HashMap<String, Arc<Subscription>>>,
    /// Set once the background motion-listener task is running.
    listener_started: Mutex<bool>,
    /// Last known motion state, used so that brand-new subscriptions get an
    /// initial event reflecting the current state on the first PullMessages.
    last_motion: Mutex<Option<bool>>,
}

impl EventsManager {
    pub(crate) fn new(cam_name: String, instance: NeoInstance, cancel: CancellationToken) -> Self {
        Self {
            cam_name,
            instance,
            cancel,
            subs: RwLock::new(HashMap::new()),
            listener_started: Mutex::new(false),
            last_motion: Mutex::new(None),
        }
    }

    pub(crate) async fn create_subscription(
        self: &Arc<Self>,
        ttl: Option<Duration>,
    ) -> Result<Arc<Subscription>> {
        // Reap expired subs before checking the cap.
        self.reap_expired().await;

        {
            let subs = self.subs.read().await;
            if subs.len() >= MAX_SUBSCRIPTIONS_PER_CAMERA {
                anyhow::bail!(
                    "ONVIF events: too many active subscriptions for camera {}",
                    self.cam_name
                );
            }
        }
        let id = Uuid::new_v4().simple().to_string();
        let sub = Arc::new(Subscription::new(
            id.clone(),
            ttl.unwrap_or(DEFAULT_SUBSCRIPTION_TTL),
        ));
        self.subs.write().await.insert(id, sub.clone());

        // Make sure the motion listener is running; first subscription on this
        // camera kicks it off.
        self.ensure_listener_running().await;

        // Seed the subscription with the current known state so a client that
        // attaches mid-motion still gets a usable first PullMessages response.
        if let Some(state) = *self.last_motion.lock().await {
            let n = build_motion_notification(&self.cam_name, state, "Initialized");
            sub.enqueue(n).await;
        }
        Ok(sub)
    }

    pub(crate) async fn get(&self, id: &str) -> Option<Arc<Subscription>> {
        self.subs.read().await.get(id).cloned()
    }

    pub(crate) async fn remove(&self, id: &str) -> bool {
        self.subs.write().await.remove(id).is_some()
    }

    async fn reap_expired(&self) {
        let now = Utc::now();
        let mut subs = self.subs.write().await;
        let mut to_drop = Vec::new();
        for (id, s) in subs.iter() {
            if s.termination_time().await < now {
                to_drop.push(id.clone());
            }
        }
        for id in to_drop {
            subs.remove(&id);
        }
    }

    async fn ensure_listener_running(self: &Arc<Self>) {
        let mut started = self.listener_started.lock().await;
        if *started {
            return;
        }
        *started = true;
        let this = self.clone();
        tokio::spawn(async move { this.run_motion_listener().await });
    }

    async fn run_motion_listener(self: Arc<Self>) {
        let cancel = self.cancel.clone();
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let result = async {
                let mut md = self.instance.motion().await?;
                // Capture the initial state if it's already known.
                let initial = mdstate_to_bool(md.borrow_and_update().clone());
                if let Some(state) = initial {
                    self.publish_motion(state, "Initialized").await;
                }
                loop {
                    md.changed().await?;
                    let snapshot = mdstate_to_bool(md.borrow_and_update().clone());
                    if let Some(state) = snapshot {
                        self.publish_motion(state, "Changed").await;
                    }
                }
                #[allow(unreachable_code)]
                Result::<()>::Ok(())
            }
            .await;
            log::debug!(
                "ONVIF events: motion listener for {} restarting: {:?}",
                self.cam_name,
                result
            );
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    async fn publish_motion(&self, state: bool, op: &'static str) {
        // Suppress duplicate publishes: the camera sometimes fires `Start`
        // when state was already Start.
        {
            let mut last = self.last_motion.lock().await;
            if *last == Some(state) {
                return;
            }
            *last = Some(state);
        }
        let n = build_motion_notification(&self.cam_name, state, op);
        let subs = self.subs.read().await.clone();
        for s in subs.values() {
            s.enqueue(n.clone()).await;
        }
    }
}

fn mdstate_to_bool(s: MdState) -> Option<bool> {
    match s {
        MdState::Start(_) => Some(true),
        MdState::Stop(_) => Some(false),
        MdState::Unknown => None,
    }
}

fn build_motion_notification(cam_name: &str, state: bool, op: &'static str) -> Notification {
    Notification {
        utc_time: Utc::now(),
        topic: "tns1:VideoSource/MotionAlarm",
        source_name: "Source",
        source_value: format!("vsrc_{cam_name}"),
        data_name: "State",
        data_value: if state { "true".into() } else { "false".into() },
        property_op: op,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdstate_mapping() {
        assert_eq!(mdstate_to_bool(MdState::Unknown), None);
        assert!(matches!(
            mdstate_to_bool(MdState::Start(Instant::now())),
            Some(true)
        ));
        assert!(matches!(
            mdstate_to_bool(MdState::Stop(Instant::now())),
            Some(false)
        ));
    }

    #[tokio::test]
    async fn subscription_enqueue_and_pull() {
        let sub = Subscription::new("x".into(), Duration::from_secs(60));
        sub.enqueue(build_motion_notification("cam", true, "Changed"))
            .await;
        let msgs = sub.pull(10, Duration::from_millis(50)).await;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].data_value, "true");
    }

    #[tokio::test]
    async fn subscription_pull_times_out() {
        let sub = Subscription::new("x".into(), Duration::from_secs(60));
        let started = Instant::now();
        let msgs = sub.pull(10, Duration::from_millis(50)).await;
        assert!(msgs.is_empty());
        // Allow a generous slack for slow CI.
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[tokio::test]
    async fn renew_extends_termination() {
        let sub = Subscription::new("x".into(), Duration::from_secs(1));
        let first = sub.termination_time().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = sub.renew(Duration::from_secs(10)).await;
        assert!(second > first);
    }

    #[tokio::test]
    async fn queue_overflow_drops_oldest() {
        let sub = Subscription::new("x".into(), Duration::from_secs(60));
        for i in 0..(MAX_PENDING_MESSAGES + 5) {
            let mut n = build_motion_notification("cam", i % 2 == 0, "Changed");
            n.data_value = i.to_string();
            sub.enqueue(n).await;
        }
        let msgs = sub
            .pull(MAX_PENDING_MESSAGES + 10, Duration::from_millis(50))
            .await;
        assert_eq!(msgs.len(), MAX_PENDING_MESSAGES);
        // The oldest 5 should have been dropped.
        assert_eq!(msgs[0].data_value, "5");
    }
}
