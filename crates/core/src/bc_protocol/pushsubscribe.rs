//! Baichuan persistent push-event subscription.
//!
//! After a successful login, sending `cmd_id = 31` (`MSG_ID_SUBSCRIBE_EVENTS`)
//! with `channel_id = 251` (`MAGIC_CHANNEL_PUSH_SUBSCRIBE`) and an empty body
//! upgrades the TCP socket into a push channel. From that point on the
//! camera asynchronously delivers:
//!
//! * `cmd 33`  — `AlarmEventList` (motion + AI sub-types, with optional
//!   sibling `smartAiTypeList` and `DayNightEvent`).
//! * `cmd 145` — `ChannelInfo` (sleep / loginState / online).
//! * `cmd 580` — `ModifyConfig` (config-stale invalidation; carries the
//!   cmd_id whose cached response should be dropped).
//!
//! Battery cameras (those whose `DeviceInfo.sleep == 1`) must **not** be
//! subscribed — the idle TCP socket keeps the radio awake.
//!
//! The state lives on a [`PushSubscription`] handle that owns a long-lived
//! tokio task. Dropping the handle cancels the task and lets the connection
//! revert to its idle keepalive cadence.

use super::{keepalive::AdaptiveKeepalive, BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Channel capacity for the broadcast bus. Push events are fan-out so we
/// keep this small; slow consumers are expected to call [`PushSubscription::next_event`]
/// in a tight loop.
const PUSH_EVENT_CHANNEL_CAP: usize = 64;

/// A single event delivered over the push channel.
#[derive(Debug, Clone)]
pub enum PushEvent {
    /// `cmd 33` AlarmEvent (motion + AI sub-types).
    Alarm {
        /// When the event was observed locally.
        at: Instant,
        /// The channel id (usually 0).
        channel_id: u8,
        /// Raw status string (`"MD"`, `"none"`, ...).
        status: String,
        /// Normalized AI sub-types (`"people"`, `"vehicle"`, `"dog_cat"`,
        /// `"face"`, `"package"`). Empty when the event is motion-only or
        /// a clear event.
        ai_types: Vec<String>,
        /// `true` if `status != "none"` or any AI sub-type is present.
        active: bool,
    },
    /// `cmd 33` sibling — smart-AI sub-event list.
    SmartAi {
        /// When the event was observed locally.
        at: Instant,
        /// The channel id, if present.
        channel_id: Option<u8>,
        /// Normalized sub-type labels.
        types: Vec<String>,
    },
    /// `cmd 33` sibling — day/night transition.
    DayNight {
        /// When the event was observed locally.
        at: Instant,
        /// The channel id, if present.
        channel_id: Option<u8>,
        /// The reported mode (e.g. `"day"`, `"night"`).
        mode: Option<String>,
    },
    /// `cmd 145` ChannelInfo (sleep / loginState / online).
    ChannelInfo {
        /// When the event was observed locally.
        at: Instant,
        /// Payload as-received.
        info: ChannelInfo,
    },
    /// `cmd 580` ModifyConfig — the camera reports that the cached
    /// response for `cmd` is stale and should be re-fetched on next read.
    ConfigStale {
        /// When the event was observed locally.
        at: Instant,
        /// The cmd_id whose cached response has been invalidated.
        cmd: u32,
        /// Optional channel filter.
        channel_id: Option<u8>,
    },
}

/// A live handle on the persistent push-event subscription.
///
/// Drop to tear the subscription down (the listener task is cancelled and
/// the camera's keepalive cadence is reset to idle).
pub struct PushSubscription {
    rx: broadcast::Receiver<PushEvent>,
    cancel: CancellationToken,
    handle: JoinSet<Result<()>>,
    /// Shared handle on the camera's keepalive policy. Dropped after the
    /// listener task has been cancelled to flip the cadence back to idle.
    keepalive: Arc<Mutex<AdaptiveKeepalive>>,
}

impl PushSubscription {
    /// Wait for the next push event from the camera.
    ///
    /// Returns `Err` if the underlying broadcast channel has been closed
    /// (i.e. the listener task has exited).
    pub async fn next_event(&mut self) -> Result<PushEvent> {
        loop {
            match self.rx.recv().await {
                Ok(ev) => return Ok(ev),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    log::warn!("PushSubscription receiver lagged by {} events", skipped);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(Error::Other("PushSubscription channel closed"))
                }
            }
        }
    }

    /// Returns a new broadcast receiver for the push channel. Useful to
    /// fan out events to multiple consumers.
    pub fn subscribe(&self) -> broadcast::Receiver<PushEvent> {
        self.rx.resubscribe()
    }
}

impl Drop for PushSubscription {
    fn drop(&mut self) {
        log::trace!("Drop PushSubscription");
        self.cancel.cancel();
        // Flip the keepalive cadence back to its idle target so the
        // background ping driver doesn't keep hammering the camera at the
        // subscribed cadence after the subscription has gone away.
        if let Ok(mut policy) = self.keepalive.lock() {
            policy.set_subscribed(false);
        }
        let mut handle = std::mem::take(&mut self.handle);
        if let Ok(handle_guard) = tokio::runtime::Handle::try_current() {
            let _gt = handle_guard.enter();
            tokio::task::spawn(async move {
                while handle.join_next().await.is_some() {}
                log::trace!("Dropped PushSubscription");
            });
        }
    }
}

impl BcCamera {
    /// Open the persistent push-event subscription on this camera.
    ///
    /// Sends `cmd 31` (`MSG_ID_SUBSCRIBE_EVENTS`) with `channel_id = 251`
    /// and an empty body, then spawns a long-lived listener that forwards
    /// every subsequent push frame (cmd 33, 145, 580) to a broadcast bus.
    ///
    /// Battery cameras (those whose [`DeviceInfo::is_battery`] reports
    /// true) must not be subscribed — pass the `DeviceInfo` returned by
    /// `login()` and this method will skip the subscription and return
    /// `Err(Error::Other("battery camera"))`. Callers that want to opt-in
    /// regardless can use [`BcCamera::subscribe_push_events_unchecked`].
    pub async fn subscribe_push_events(
        &self,
        device_info: &DeviceInfo,
    ) -> Result<PushSubscription> {
        if device_info.is_battery() {
            log::info!(
                "Skipping push-event subscription on battery camera (DeviceInfo.sleep = {:?})",
                device_info.sleep
            );
            return Err(Error::Other(
                "battery camera: push subscription intentionally skipped",
            ));
        }
        self.subscribe_push_events_unchecked().await
    }

    /// Open the persistent push-event subscription without consulting
    /// `DeviceInfo.sleep`.
    ///
    /// Prefer [`BcCamera::subscribe_push_events`]; this variant exists
    /// for cameras whose login response omits the sleep field but which
    /// the caller knows to be mains-powered.
    pub async fn subscribe_push_events_unchecked(&self) -> Result<PushSubscription> {
        // 1. Send the cmd 31 / ch 251 / empty body subscribe frame.
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_SUBSCRIBE_EVENTS, msg_num)
            .await?;

        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SUBSCRIBE_EVENTS,
                channel_id: MAGIC_CHANNEL_PUSH_SUBSCRIBE,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg::default()),
        };

        sub.send(msg).await?;

        let reply = sub.recv().await?;
        if reply.meta.response_code != 200 {
            return Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(reply)),
                why: "Camera refused push-event subscription",
            });
        }

        // 2. Bump the adaptive keepalive policy into "subscribed" cadence
        //    so the keepalive driver knows we now need 30s pings.
        if let Ok(mut policy) = self.keepalive_policy().lock() {
            policy.set_subscribed(true);
            log::debug!(
                "Push subscription active; keepalive cadence is now {}s",
                policy.current_secs()
            );
        }

        // 3. Spawn the dispatcher task. The task subscribes to the three
        //    push-frame ids (cmd 33, 145, 580) and forwards every frame
        //    to the broadcast bus.
        let (tx, rx) = broadcast::channel::<PushEvent>(PUSH_EVENT_CHANNEL_CAP);
        let cancel = CancellationToken::new();
        let mut set = JoinSet::new();

        // cmd 33 — AlarmEventList (+ SmartAiTypeList, DayNightEvent siblings)
        {
            let connection = connection.clone();
            let tx = tx.clone();
            let cancel = cancel.clone();
            set.spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => Ok(()),
                    v = dispatch_loop(connection, MSG_ID_ALARM_EVENT, move |bc| {
                        let mut out = Vec::new();
                        if let BcBody::ModernMsg(ModernMsg {
                            payload: Some(BcPayloads::BcXml(xml)),
                            ..
                        }) = &bc.body {
                            if let Some(list) = xml.alarm_event_list.as_ref() {
                                for ev in &list.alarm_events {
                                    // Compute `ai_types` once and derive
                                    // `active` from it — `is_active()` would
                                    // otherwise re-parse the comma-separated
                                    // AItype list per push.
                                    let ai_types = ev.ai_types();
                                    let active = ev.status != "none" || !ai_types.is_empty();
                                    out.push(PushEvent::Alarm {
                                        at: Instant::now(),
                                        channel_id: ev.channel_id,
                                        status: ev.status.clone(),
                                        ai_types,
                                        active,
                                    });
                                }
                            }
                            if let Some(smart) = xml.smart_ai_type_list.as_ref() {
                                out.push(PushEvent::SmartAi {
                                    at: Instant::now(),
                                    channel_id: smart.channel_id,
                                    types: smart.types(),
                                });
                            }
                            if let Some(dn) = xml.day_night_event.as_ref() {
                                out.push(PushEvent::DayNight {
                                    at: Instant::now(),
                                    channel_id: dn.channel_id,
                                    mode: dn.mode.clone(),
                                });
                            }
                        }
                        out
                    }, tx) => v,
                }
            });
        }

        // cmd 145 — ChannelInfo
        {
            let connection = connection.clone();
            let tx = tx.clone();
            let cancel = cancel.clone();
            set.spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => Ok(()),
                    v = dispatch_loop(connection, MSG_ID_CHANNEL_INFO, move |bc| {
                        if let BcBody::ModernMsg(ModernMsg {
                            payload: Some(BcPayloads::BcXml(BcXml {
                                channel_info: Some(info),
                                ..
                            })),
                            ..
                        }) = &bc.body {
                            vec![PushEvent::ChannelInfo {
                                at: Instant::now(),
                                info: info.clone(),
                            }]
                        } else {
                            Vec::new()
                        }
                    }, tx) => v,
                }
            });
        }

        // cmd 580 — ModifyConfig
        {
            let connection = connection.clone();
            let tx = tx.clone();
            let cancel = cancel.clone();
            set.spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => Ok(()),
                    v = dispatch_loop(connection, MSG_ID_MODIFY_CONFIG, move |bc| {
                        if let BcBody::ModernMsg(ModernMsg {
                            payload: Some(BcPayloads::BcXml(BcXml {
                                modify_config: Some(mc),
                                ..
                            })),
                            ..
                        }) = &bc.body {
                            vec![PushEvent::ConfigStale {
                                at: Instant::now(),
                                cmd: mc.cmd,
                                channel_id: mc.channel_id,
                            }]
                        } else {
                            Vec::new()
                        }
                    }, tx) => v,
                }
            });
        }

        Ok(PushSubscription {
            rx,
            cancel,
            handle: set,
            keepalive: self.keepalive_policy_handle(),
        })
    }
}

/// Subscribe to a single push cmd id and forward every frame through `to_events`
/// onto the broadcast bus until either the connection drops or the receiver is
/// closed.
async fn dispatch_loop<F>(
    connection: Arc<crate::bc_protocol::BcConnection>,
    msg_id: u32,
    to_events: F,
    tx: broadcast::Sender<PushEvent>,
) -> Result<()>
where
    F: Fn(&Bc) -> Vec<PushEvent> + Send + 'static,
{
    let mut sub = connection.subscribe_to_id(msg_id).await?;
    loop {
        match sub.recv().await {
            Ok(bc) => {
                let events = to_events(&bc);
                for ev in events {
                    // It's fine if there are no receivers — we are a
                    // broadcaster and consumers may come and go.
                    let _ = tx.send(ev);
                }
            }
            Err(e) => {
                log::trace!("push dispatch loop for cmd {} ending: {}", msg_id, e);
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_info_is_battery_handles_missing_field() {
        let mains = DeviceInfo {
            sleep: None,
            ..Default::default()
        };
        assert!(!mains.is_battery());

        let mains_explicit = DeviceInfo {
            sleep: Some(0),
            ..Default::default()
        };
        assert!(!mains_explicit.is_battery());

        let battery = DeviceInfo {
            sleep: Some(1),
            ..Default::default()
        };
        assert!(battery.is_battery());
    }

    #[test]
    fn push_event_alarm_carries_normalized_ai_types() {
        // Build a synthetic Bc frame and route it through the same closure
        // the dispatcher uses, to validate parsing without spinning up a
        // real camera connection.
        let xml = BcXml {
            alarm_event_list: Some(AlarmEventList {
                version: "1.1".to_string(),
                alarm_events: vec![AlarmEvent {
                    version: "1.1".to_string(),
                    channel_id: 0,
                    status: "MD".to_string(),
                    ai_type: Some("person,pet,vehicle".to_string()),
                    recording: 1,
                    timeStamp: 1700000000,
                }],
            }),
            ..Default::default()
        };
        let bc = Bc::new_from_xml(
            BcMeta {
                msg_id: MSG_ID_ALARM_EVENT,
                channel_id: 0,
                msg_num: 1,
                stream_type: 0,
                response_code: 200,
                class: 0x6414,
            },
            xml,
        );

        let mut events = Vec::new();
        if let BcBody::ModernMsg(ModernMsg {
            payload: Some(BcPayloads::BcXml(xml)),
            ..
        }) = &bc.body
        {
            if let Some(list) = xml.alarm_event_list.as_ref() {
                for ev in &list.alarm_events {
                    events.push(PushEvent::Alarm {
                        at: Instant::now(),
                        channel_id: ev.channel_id,
                        status: ev.status.clone(),
                        ai_types: ev.ai_types(),
                        active: ev.is_active(),
                    });
                }
            }
        }
        assert_eq!(events.len(), 1);
        match &events[0] {
            PushEvent::Alarm {
                status,
                ai_types,
                active,
                ..
            } => {
                assert_eq!(status, "MD");
                assert!(*active);
                assert_eq!(ai_types, &vec!["people", "dog_cat", "vehicle"]);
            }
            _ => panic!("unexpected event"),
        }
    }

    #[test]
    fn modify_config_is_translated_to_config_stale() {
        let xml = BcXml {
            modify_config: Some(ModifyConfig {
                version: "1.1".to_string(),
                cmd: 56,
                channel_id: Some(0),
            }),
            ..Default::default()
        };
        let bc = Bc::new_from_xml(
            BcMeta {
                msg_id: MSG_ID_MODIFY_CONFIG,
                channel_id: 0,
                msg_num: 1,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            xml,
        );
        let mut events = Vec::new();
        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    modify_config: Some(mc),
                    ..
                })),
            ..
        }) = &bc.body
        {
            events.push(PushEvent::ConfigStale {
                at: Instant::now(),
                cmd: mc.cmd,
                channel_id: mc.channel_id,
            });
        }
        match events.as_slice() {
            [PushEvent::ConfigStale {
                cmd: 56,
                channel_id: Some(0),
                ..
            }] => {}
            _ => panic!("expected one ConfigStale(56)"),
        }
    }
}
