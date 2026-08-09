//! Per-camera ONVIF events manager.
//!
//! Translates the camera's alarm stream into ONVIF notifications:
//!
//! * `tns1:VideoSource/MotionAlarm` from the `NeoInstance::motion()` watch
//! * `tns1:RuleEngine/MyRuleDetector/*` (people, vehicle, dog/cat, face,
//!   visitor) and `tns1:AudioAnalytics/Audio/DetectedSound` (baby cry) from
//!   the `NeoInstance::ai()` watch
//! * `tns1:RuleEngine/FieldDetector/ObjectsInside` for the smart-AI zone
//!   detectors (crossline, intrusion, loitering, legacy, loss), distinguished
//!   by the `Rule` source item
//!
//! These are the topics Reolink cameras emit natively, so a bridged camera
//! presents the same event surface to a VMS as a directly attached one.
//!
//! Notifications are delivered through ONVIF PullPoint subscriptions: clients
//! call `CreatePullPointSubscription`, then poll the returned subscription URI
//! with `PullMessages`. The subscription endpoint also serves `Renew` and
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
///
/// `source` and `data` are lists because some ONVIF topics carry more than one
/// `SimpleItem`; `tns1:AudioAnalytics/Audio/DetectedSound` for instance is
/// identified by an audio source token, an analytics token and a rule name.
#[derive(Clone, Debug)]
pub(crate) struct Notification {
    pub(crate) utc_time: DateTime<Utc>,
    pub(crate) topic: &'static str,
    pub(crate) source: Vec<(&'static str, String)>,
    pub(crate) data: Vec<(&'static str, String)>,
    pub(crate) property_op: &'static str,
}

pub(crate) struct Subscription {
    pub(crate) id: String,
    pub(crate) created_at: DateTime<Utc>,
    /// Termination timestamp is a tiny `Copy` value updated by
    /// `Subscription::renew` and read by `termination_time` /
    /// `EventsManager::reap_expired`. A `std::sync::Mutex` is correct here:
    /// the critical section is just a load/store with no `.await` points,
    /// and using a sync mutex lets `reap_expired` iterate without holding
    /// the global `subs` lock across `.await`.
    terminates_at: std::sync::Mutex<DateTime<Utc>>,
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
            terminates_at: std::sync::Mutex::new(term),
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

    pub(crate) fn renew(&self, ttl: Duration) -> DateTime<Utc> {
        let new_term = Utc::now()
            + chrono::Duration::from_std(ttl.min(MAX_SUBSCRIPTION_TTL))
                .unwrap_or_else(|_| chrono::Duration::seconds(60));
        *self
            .terminates_at
            .lock()
            .expect("terminates_at mutex poisoned") = new_term;
        new_term
    }

    pub(crate) fn termination_time(&self) -> DateTime<Utc> {
        *self
            .terminates_at
            .lock()
            .expect("terminates_at mutex poisoned")
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
    /// Last published state per AI type / smart-AI zone detector, used both to
    /// suppress duplicates and to seed new subscriptions.
    last_ai: Mutex<HashMap<String, bool>>,
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
            last_ai: Mutex::new(HashMap::new()),
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
        // Seed the subscription with the current known state so a client that
        // attaches mid-motion still gets a usable first PullMessages response.
        //
        // This happens *before* the subscription is published to `subs`: once
        // it is visible, a listener can enqueue a "Changed" message, and a
        // seed appended after that would leave the client latched to the older
        // value.
        self.resync(&sub).await;
        self.subs.write().await.insert(id, sub.clone());

        // Make sure the motion listener is running; first subscription on this
        // camera kicks it off.
        self.ensure_listener_running().await;

        Ok(sub)
    }

    /// Re-send the current state of every property to one subscription.
    ///
    /// This is both the initial seeding and the answer to the client calling
    /// `SetSynchronizationPoint`.
    pub(crate) async fn resync(&self, sub: &Arc<Subscription>) {
        if let Some(state) = *self.last_motion.lock().await {
            for n in build_motion_notifications(&self.cam_name, state, "Initialized") {
                sub.enqueue(n).await;
            }
        }
        for (key, state) in self.last_ai.lock().await.iter() {
            if let Some(n) = self.build_ai_key_notification(key, *state, "Initialized") {
                sub.enqueue(n).await;
            }
        }
    }

    /// Build the notification for one entry of `last_ai`.
    ///
    /// Zone detectors are stored with a `zone:` prefix so they cannot collide
    /// with an AI type of the same name.
    fn build_ai_key_notification(
        &self,
        key: &str,
        state: bool,
        op: &'static str,
    ) -> Option<Notification> {
        match key.strip_prefix("zone:") {
            Some(kind) => Some(build_zone_notification(&self.cam_name, kind, state, op)),
            None => build_ai_notification(&self.cam_name, key, state, op),
        }
    }

    pub(crate) async fn get(&self, id: &str) -> Option<Arc<Subscription>> {
        self.subs.read().await.get(id).cloned()
    }

    pub(crate) async fn remove(&self, id: &str) -> bool {
        self.subs.write().await.remove(id).is_some()
    }

    async fn reap_expired(&self) {
        let now = Utc::now();
        // `termination_time` is now sync, so we can do the whole sweep
        // without releasing the write-lock — but more importantly, without
        // awaiting while holding it. That keeps subscription operations
        // unblocked even when many entries are present.
        self.subs
            .write()
            .await
            .retain(|_, s| s.termination_time() >= now);
    }

    async fn ensure_listener_running(self: &Arc<Self>) {
        let mut started = self.listener_started.lock().await;
        if *started {
            return;
        }
        *started = true;
        let this = self.clone();
        tokio::spawn(async move { this.run_motion_listener().await });
        let this = self.clone();
        tokio::spawn(async move { this.run_ai_listener().await });
    }

    /// Mirror the camera's AI detections onto the ONVIF topics a VMS expects.
    async fn run_ai_listener(self: Arc<Self>) {
        let cancel = self.cancel.clone();
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let result = async {
                let mut ai = self.instance.ai().await?;
                loop {
                    let state = ai.borrow_and_update().clone();
                    let mut current: HashMap<String, bool> = state
                        .detections()
                        .iter()
                        .map(|(k, v)| (k.clone(), *v))
                        .collect();
                    for (kind, locations) in state.zones().iter() {
                        current.insert(format!("zone:{kind}"), !locations.is_empty());
                    }
                    // A detector that has stopped reporting is no longer
                    // detecting anything.
                    for key in self.last_ai.lock().await.keys() {
                        current.entry(key.clone()).or_insert(false);
                    }
                    self.publish_ai(current).await;
                    ai.changed().await?;
                }
                #[allow(unreachable_code)]
                Result::<()>::Ok(())
            }
            .await;
            log::debug!(
                "ONVIF events: AI listener for {} restarting: {:?}",
                self.cam_name,
                result
            );
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    async fn publish_ai(&self, current: HashMap<String, bool>) {
        let mut changed = vec![];
        {
            let mut last = self.last_ai.lock().await;
            for (key, state) in current.into_iter() {
                if last.get(&key) == Some(&state) {
                    continue;
                }
                last.insert(key.clone(), state);
                changed.push((key, state));
            }
        }
        if changed.is_empty() {
            return;
        }
        let subs = self.subs.read().await.clone();
        for (key, state) in changed.into_iter() {
            let Some(n) = self.build_ai_key_notification(&key, state, "Changed") else {
                // AI types without a standard ONVIF topic (package, ...) are
                // MQTT only.
                continue;
            };
            for s in subs.values() {
                s.enqueue(n.clone()).await;
            }
        }
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
        let subs = self.subs.read().await.clone();
        for n in build_motion_notifications(&self.cam_name, state, op) {
            for s in subs.values() {
                s.enqueue(n.clone()).await;
            }
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

/// Motion is published on two topics, not one.
///
/// `tns1:VideoSource/MotionAlarm` is what a Reolink camera emits natively and
/// what `reolink_aio` looks for. But a large part of the VMS world — Frigate's
/// ONVIF path, Blue Iris, Synology Surveillance Station, Milestone, Agent DVR —
/// only ever subscribes to `tns1:RuleEngine/CellMotionDetector/Motion`, because
/// that is the topic ONVIF Profile S standardised for motion and what most
/// non-Reolink cameras send. A bridge that publishes only the native topic is
/// invisible to all of them.
///
/// Emitting both costs one extra queued message per state change and is what a
/// camera supporting both profiles does. Clients that understand both see a
/// consistent pair rather than a contradiction, since they are always published
/// together from the same state.
const MOTION_TOPICS: &[(&str, &str)] = &[
    ("tns1:VideoSource/MotionAlarm", "State"),
    ("tns1:RuleEngine/CellMotionDetector/Motion", "IsMotion"),
];

fn build_motion_notifications(cam_name: &str, state: bool, op: &'static str) -> Vec<Notification> {
    MOTION_TOPICS
        .iter()
        .map(|&(topic, data_name)| Notification {
            utc_time: Utc::now(),
            topic,
            source: vec![("Source", format!("vsrc_{cam_name}"))],
            // The data item name differs between the two: `MotionAlarm`
            // carries `State`, the cell-motion rule carries `IsMotion`.
            // Clients look it up by name, so a shared name would make one of
            // the two silently unreadable.
            data: vec![(data_name, bool_value(state))],
            property_op: op,
        })
        .collect()
}

#[cfg(test)]
fn build_motion_notification(cam_name: &str, state: bool, op: &'static str) -> Notification {
    build_motion_notifications(cam_name, state, op)
        .into_iter()
        .next()
        .expect("there is always at least one motion topic")
}

fn bool_value(state: bool) -> String {
    if state {
        "true".to_string()
    } else {
        "false".to_string()
    }
}

/// How an AI type reported by the camera maps onto an ONVIF topic.
///
/// These are the topics a real Reolink camera emits. `reolink_aio` — what
/// Home Assistant's Reolink integration uses to consume ONVIF events —
/// accepts exactly the rules `Motion`, `MotionAlarm`, `FaceDetect`,
/// `PeopleDetect`, `VehicleDetect`, `DogCatDetect`, `Package` and `Visitor`
/// (`reolink_aio/api.py::ONVIF_event_callback`), and Home Assistant's generic
/// ONVIF integration registers parsers for the same set. Note the package leaf
/// is `Package`, not `PackageDetect`.
///
/// `non-motor vehicle` has no standard topic and stays MQTT only.
const AI_TOPICS: &[(&str, &str)] = &[
    ("people", "tns1:RuleEngine/MyRuleDetector/PeopleDetect"),
    ("vehicle", "tns1:RuleEngine/MyRuleDetector/VehicleDetect"),
    ("dog_cat", "tns1:RuleEngine/MyRuleDetector/DogCatDetect"),
    ("face", "tns1:RuleEngine/MyRuleDetector/FaceDetect"),
    ("visitor", "tns1:RuleEngine/MyRuleDetector/Visitor"),
    ("package", "tns1:RuleEngine/MyRuleDetector/Package"),
];

/// The name of the data item on the rule-detector topics.
///
/// `reolink_aio` looks this up **by name**
/// (`SimpleItem[@Name='State']`), so an invented name such as `IsPeople`
/// would make it discard the event entirely. It also matches the data name
/// the existing `MotionAlarm` notification already uses.
const AI_DATA_NAME: &str = "State";

/// The ONVIF topic for an audio detection (baby cry).
const AUDIO_TOPIC: &str = "tns1:AudioAnalytics/Audio/DetectedSound";

/// The ONVIF topic used for the smart-AI zone detectors.
///
/// All five (crossline / intrusion / loitering / legacy / loss) are reported
/// as field detections distinguished by the `Rule` source item, which is what
/// a VMS keys its entities on.
const FIELD_TOPIC: &str = "tns1:RuleEngine/FieldDetector/ObjectsInside";

/// Look up the ONVIF topic for a camera AI type.
fn ai_topic(ai_type: &str) -> Option<&'static str> {
    AI_TOPICS
        .iter()
        .find(|(name, _)| *name == ai_type)
        .map(|(_, topic)| *topic)
}

fn build_ai_notification(
    cam_name: &str,
    ai_type: &str,
    state: bool,
    op: &'static str,
) -> Option<Notification> {
    if ai_type == "cry" {
        return Some(Notification {
            utc_time: Utc::now(),
            topic: AUDIO_TOPIC,
            source: vec![
                ("AudioSourceConfigurationToken", format!("asrc_{cam_name}")),
                (
                    "AudioAnalyticsConfigurationToken",
                    format!("aacfg_{cam_name}"),
                ),
                ("Rule", "CryDetect".to_string()),
            ],
            data: vec![("IsSoundDetected", bool_value(state))],
            property_op: op,
        });
    }
    let topic = ai_topic(ai_type)?;
    Some(Notification {
        utc_time: Utc::now(),
        topic,
        source: vec![("Source", format!("vsrc_{cam_name}"))],
        data: vec![(AI_DATA_NAME, bool_value(state))],
        property_op: op,
    })
}

fn build_zone_notification(
    cam_name: &str,
    kind: &str,
    state: bool,
    op: &'static str,
) -> Notification {
    Notification {
        utc_time: Utc::now(),
        topic: FIELD_TOPIC,
        source: vec![
            ("VideoSourceConfigurationToken", format!("vsrc_{cam_name}")),
            (
                "VideoAnalyticsConfigurationToken",
                format!("vacfg_{cam_name}"),
            ),
            ("Rule", kind.to_string()),
        ],
        data: vec![("IsInside", bool_value(state))],
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

    /// Motion has to reach both worlds: Reolink's native topic for
    /// `reolink_aio`, and the ONVIF-standard cell-motion topic that Frigate,
    /// Blue Iris, Synology and Milestone subscribe to instead.
    #[test]
    fn motion_is_published_on_both_topics() {
        let ns = build_motion_notifications("cam", true, "Changed");
        let topics: Vec<_> = ns.iter().map(|n| n.topic).collect();
        assert_eq!(
            topics,
            vec![
                "tns1:VideoSource/MotionAlarm",
                "tns1:RuleEngine/CellMotionDetector/Motion"
            ]
        );
        // Both describe the same state, so a client reading both can never see
        // them disagree.
        for n in &ns {
            assert_eq!(n.data[0].1, "true");
            assert_eq!(n.source[0], ("Source", "vsrc_cam".to_string()));
            assert_eq!(n.property_op, "Changed");
        }
    }

    /// Clients look the data item up by name, and the two topics use different
    /// ones — sharing a name would make one of them silently unreadable.
    #[test]
    fn each_motion_topic_uses_its_own_data_item_name() {
        let ns = build_motion_notifications("cam", false, "Initialized");
        assert_eq!(ns[0].data[0].0, "State");
        assert_eq!(ns[1].data[0].0, "IsMotion");
        for n in &ns {
            assert_eq!(n.data[0].1, "false");
        }
    }

    #[test]
    fn ai_types_map_to_the_topics_clients_expect() {
        // These exact strings are the rules `reolink_aio` accepts and that
        // Home Assistant's ONVIF integration registers parsers for; getting
        // one wrong silently drops the event. Note `Package`, not
        // `PackageDetect`.
        for (ai_type, topic) in [
            ("people", "tns1:RuleEngine/MyRuleDetector/PeopleDetect"),
            ("vehicle", "tns1:RuleEngine/MyRuleDetector/VehicleDetect"),
            ("dog_cat", "tns1:RuleEngine/MyRuleDetector/DogCatDetect"),
            ("face", "tns1:RuleEngine/MyRuleDetector/FaceDetect"),
            ("visitor", "tns1:RuleEngine/MyRuleDetector/Visitor"),
            ("package", "tns1:RuleEngine/MyRuleDetector/Package"),
        ] {
            let n = build_ai_notification("cam", ai_type, true, "Changed")
                .unwrap_or_else(|| panic!("{} should map to a topic", ai_type));
            assert_eq!(n.topic, topic, "{}", ai_type);
            // `reolink_aio` looks the data item up by name, so this must be
            // `State` and not an invented `IsPeople`-style name.
            assert_eq!(n.data[0].0, "State", "{}", ai_type);
            assert_eq!(n.data[0].1, "true", "{}", ai_type);
            // The parsers look the video source up by an item literally named
            // "Source".
            assert_eq!(n.source[0].0, "Source", "{}", ai_type);
        }
    }

    #[test]
    fn cry_maps_to_the_audio_topic() {
        let n = build_ai_notification("cam", "cry", true, "Changed").expect("cry should map");
        assert_eq!(n.topic, "tns1:AudioAnalytics/Audio/DetectedSound");
        let names: Vec<_> = n.source.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            vec![
                "AudioSourceConfigurationToken",
                "AudioAnalyticsConfigurationToken",
                "Rule"
            ]
        );
    }

    #[test]
    fn ai_types_without_a_standard_topic_are_skipped() {
        // Published over MQTT, but there is no ONVIF topic a VMS would parse.
        assert!(build_ai_notification("cam", "non-motor vehicle", true, "Changed").is_none());
    }

    #[test]
    fn smart_ai_zones_map_to_field_detection_with_a_rule() {
        let n = build_zone_notification("cam", "crossline", true, "Changed");
        assert_eq!(n.topic, "tns1:RuleEngine/FieldDetector/ObjectsInside");
        assert_eq!(n.source[2], ("Rule", "crossline".to_string()));
        assert_eq!(n.data[0].1, "true");
    }

    #[tokio::test]
    async fn subscription_enqueue_and_pull() {
        let sub = Subscription::new("x".into(), Duration::from_secs(60));
        sub.enqueue(build_motion_notification("cam", true, "Changed"))
            .await;
        let msgs = sub.pull(10, Duration::from_millis(50)).await;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].data[0].1, "true");
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
        let first = sub.termination_time();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = sub.renew(Duration::from_secs(10));
        assert!(second > first);
    }

    #[tokio::test]
    async fn queue_overflow_drops_oldest() {
        let sub = Subscription::new("x".into(), Duration::from_secs(60));
        for i in 0..(MAX_PENDING_MESSAGES + 5) {
            let mut n = build_motion_notification("cam", i % 2 == 0, "Changed");
            n.data[0].1 = i.to_string();
            sub.enqueue(n).await;
        }
        let msgs = sub
            .pull(MAX_PENDING_MESSAGES + 10, Duration::from_millis(50))
            .await;
        assert_eq!(msgs.len(), MAX_PENDING_MESSAGES);
        // The oldest 5 should have been dropped.
        assert_eq!(msgs[0].data[0].1, "5");
    }
}
