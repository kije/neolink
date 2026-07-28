//! Second-generation Reolink AI / smart-detect surface over Baichuan.
//!
//! Wraps cmd ids 527/529/531/549/551 (smart-AI zones, read-only — see
//! below; the matching write ids 528/530/532/550/552 are deliberately not
//! issued), 342/343 (per-AI alarm config), 299/300 (`AiCfg`, which carries
//! baby-cry detection and auto-tracking) and 600/696 (YOLO push events).
//!
//! The five smart-AI kinds share a common shape, so they are dispatched via
//! the [`SmartAiKind`] enum. They also share the per-zone item type
//! [`SmartDetectItem`]; only the name of the zone element differs.
//!
//! # Why the zone detectors are read-only
//!
//! `reolink_aio` writes a zone by patching the *raw XML* of the get-reply and
//! sending it back, so every element it does not understand survives the
//! write. Rebuilding the container from a typed struct cannot do that: any
//! element we have not modelled — zone geometry above all — would be dropped,
//! and cmds 528/530/532/550/552 look authoritative for the whole container.
//! Rather than risk erasing a user's detection lines, the zone setters are
//! left out until a camera capture pins the full item shape. Reading the
//! zones, and the 342/343 and 299/300 config surfaces, are unaffected.
//!
//! # Where AI *events* come from
//!
//! Detections are **not** reported through these cmd ids. They arrive on the
//! ordinary alarm channel (cmd 33, `<AlarmEventList>`), in the `AItype` field
//! and the `smartAiTypeList` element of each `<AlarmEvent>`. Because the
//! connection allows only one subscriber per message id, that decoding lives
//! with the motion listener: see [`crate::bc_protocol::MotionData::ai_state`]
//! and [`AiState`].
//!
//! Element names come from `dissector/messages.md` (which documents cmd 299
//! as `<AiCfg>`) and from the `reolink_aio` symbols cited on each struct.

use super::{BcCamera, BcConnection, Error, Result};
use crate::bc::{model::*, xml::*};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::mpsc::{channel, Receiver};

/// Which smart-AI detector cmd-pair to operate on.
///
/// Each variant maps to a `(get, set)` MSG_ID pair, and all five share the
/// [`SmartDetectItem`] zone shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SmartAiKind {
    /// Line-cross zones (cmd 527/528)
    Crossline,
    /// Intrusion zones (cmd 529/530)
    Intrusion,
    /// Loitering zones (cmd 531/532)
    Loitering,
    /// Forgotten-object zones (cmd 549/550)
    Legacy,
    /// Taken-object zones (cmd 551/552)
    Loss,
}

/// Every [`SmartAiKind`], in a fixed order, for callers that want to iterate
/// the whole surface.
pub const SMART_AI_KINDS: &[SmartAiKind] = &[
    SmartAiKind::Crossline,
    SmartAiKind::Intrusion,
    SmartAiKind::Loitering,
    SmartAiKind::Legacy,
    SmartAiKind::Loss,
];

impl SmartAiKind {
    /// MSG_ID for the GET variant of this detector
    pub fn get_id(self) -> u32 {
        match self {
            Self::Crossline => MSG_ID_GET_CROSSLINE_DETECT,
            Self::Intrusion => MSG_ID_GET_INTRUSION_DETECT,
            Self::Loitering => MSG_ID_GET_LOITERING_DETECT,
            Self::Legacy => MSG_ID_GET_LEGACY_DETECT,
            Self::Loss => MSG_ID_GET_LOSS_DETECT,
        }
    }

    /// MSG_ID for the SET variant of this detector
    pub fn set_id(self) -> u32 {
        match self {
            Self::Crossline => MSG_ID_SET_CROSSLINE_DETECT,
            Self::Intrusion => MSG_ID_SET_INTRUSION_DETECT,
            Self::Loitering => MSG_ID_SET_LOITERING_DETECT,
            Self::Legacy => MSG_ID_SET_LEGACY_DETECT,
            Self::Loss => MSG_ID_SET_LOSS_DETECT,
        }
    }

    /// The name Reolink uses for this detector on the wire, e.g. in the
    /// `smartAiTypeList` of an alarm event.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Crossline => "crossline",
            Self::Intrusion => "intrusion",
            Self::Loitering => "loitering",
            Self::Legacy => "legacy",
            Self::Loss => "loss",
        }
    }
}

impl std::fmt::Display for SmartAiKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SmartAiKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "crossline" => Ok(Self::Crossline),
            "intrusion" => Ok(Self::Intrusion),
            "loitering" | "linger" => Ok(Self::Loitering),
            "legacy" | "forgotten" => Ok(Self::Legacy),
            "loss" | "taken" => Ok(Self::Loss),
            _ => Err(Error::Other("Unknown smart AI kind")),
        }
    }
}

/// The contents of a smart-AI reply.
///
/// Returned by [`BcCamera::get_smart_ai`]. The variant indicates which
/// detector replied and carries the channel-id and zone list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SmartAiPayload {
    /// Crossline (cmd 527) reply
    Crossline(CrosslineDetect),
    /// Intrusion (cmd 529) reply
    Intrusion(IntrusionDetect),
    /// Loitering (cmd 531) reply
    Loitering(LoiteringDetect),
    /// Legacy / forgotten-object (cmd 549) reply
    Legacy(LegacyDetect),
    /// Loss / taken-object (cmd 551) reply
    Loss(LossDetect),
}

impl SmartAiPayload {
    /// Which detector this payload belongs to
    pub fn kind(&self) -> SmartAiKind {
        match self {
            Self::Crossline(_) => SmartAiKind::Crossline,
            Self::Intrusion(_) => SmartAiKind::Intrusion,
            Self::Loitering(_) => SmartAiKind::Loitering,
            Self::Legacy(_) => SmartAiKind::Legacy,
            Self::Loss(_) => SmartAiKind::Loss,
        }
    }

    /// Returns the channel id this payload was reported for.
    pub fn channel_id(&self) -> u8 {
        match self {
            Self::Crossline(v) => v.channel_id,
            Self::Intrusion(v) => v.channel_id,
            Self::Loitering(v) => v.channel_id,
            Self::Legacy(v) => v.channel_id,
            Self::Loss(v) => v.channel_id,
        }
    }

    /// The configured detection zones
    pub fn items(&self) -> &[SmartDetectItem] {
        match self {
            Self::Crossline(v) => &v.items,
            Self::Intrusion(v) => &v.items,
            Self::Loitering(v) => &v.items,
            Self::Legacy(v) => &v.items,
            Self::Loss(v) => &v.items,
        }
    }
}

/// The AI detection state of a channel.
///
/// Built from the `AItype` field and the `smartAiTypeList` element of the
/// alarm events the camera pushes on cmd 33, and kept up to date by the
/// motion listener. Obtain one with
/// [`crate::bc_protocol::MotionData::ai_state`].
///
/// The camera reports the *complete* current state on every alarm event, so a
/// type that stops being listed has stopped being detected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AiState {
    /// Canonical AI type -> currently detected.
    ///
    /// Keys accumulate as types are seen: a camera that has never reported
    /// `dog_cat` simply has no entry for it.
    detections: BTreeMap<String, bool>,
    /// Smart-AI detector name -> the zone locations currently triggered
    zones: BTreeMap<String, BTreeSet<u32>>,
}

impl AiState {
    /// Whether the given AI type is currently detected.
    ///
    /// The name is normalized, so `person` and `people` both work.
    pub fn is_detected(&self, ai_type: &str) -> bool {
        self.detections
            .get(canonical_ai_type(ai_type))
            .copied()
            .unwrap_or(false)
    }

    /// Every AI type this channel has ever reported, with its current state.
    pub fn detections(&self) -> &BTreeMap<String, bool> {
        &self.detections
    }

    /// The AI types currently detected
    pub fn detected_types(&self) -> impl Iterator<Item = &str> {
        self.detections
            .iter()
            .filter(|(_, detected)| **detected)
            .map(|(ai_type, _)| ai_type.as_str())
    }

    /// The zone locations currently triggered, per smart-AI detector
    pub fn zones(&self) -> &BTreeMap<String, BTreeSet<u32>> {
        &self.zones
    }

    /// Whether any zone of the given smart-AI detector is triggered
    pub fn is_zone_detected(&self, kind: SmartAiKind) -> bool {
        self.zones
            .get(kind.as_str())
            .map(|locs| !locs.is_empty())
            .unwrap_or(false)
    }

    /// Apply one alarm *message* to this state.
    ///
    /// Takes every [`AlarmEvent`] of the message that belongs to our channel
    /// at once, rather than one at a time: the camera may split its report
    /// over several `<AlarmEvent>` elements, and folding them individually
    /// would let a later event wipe what an earlier one reported.
    ///
    /// Mirrors `reolink_aio`: `AItype` carries the full set of active types,
    /// so every known key is cleared and then the reported ones are set. A
    /// message that carries no `AItype` at all leaves the AI types untouched
    /// (the camera is only reporting motion), but the smart-AI zones are
    /// always recomputed.
    pub(crate) fn apply_events<'a>(&mut self, events: impl Iterator<Item = &'a AlarmEvent>) {
        let mut active: BTreeSet<String> = BTreeSet::new();
        let mut zones: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
        let mut saw_ai_type = false;

        for event in events {
            if event.ai_type.is_some() {
                saw_ai_type = true;
            }
            for ai_type in event.ai_types() {
                // `other` is how the camera signals plain motion; that is
                // already covered by MotionStatus so it is not an AI type.
                if ai_type == "other" {
                    continue;
                }
                active.insert(ai_type.to_string());
            }
            // A doorbell press is reported in `status`, not `AItype`.
            if event.status.split(',').any(|part| part.trim() == "visitor") {
                saw_ai_type = true;
                active.insert("visitor".to_string());
            }
            if let Some(list) = event.smart_ai_type_list.as_ref() {
                for smart in list.types.iter() {
                    if smart.smart_type.is_empty() {
                        continue;
                    }
                    zones
                        .entry(smart.smart_type.clone())
                        .or_default()
                        .extend(smart.locations());
                }
            }
        }

        if saw_ai_type {
            for (ai_type, detected) in self.detections.iter_mut() {
                *detected = active.contains(ai_type);
            }
            for ai_type in active.into_iter() {
                self.detections.insert(ai_type, true);
            }
        }
        self.zones = zones;
    }
}

/// A YOLO push event received from the camera (cmd 600 / 696).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmartAiEvent {
    /// The channel the event was reported on
    pub channel_id: u8,
    /// Whether this came from the detailed (cmd 696) push rather than the
    /// basic (cmd 600) one
    pub detailed: bool,
    /// The detection itself
    pub detection: YoloWorldType,
}

impl SmartAiEvent {
    /// The channel id the event was reported on
    pub fn channel_id(&self) -> u8 {
        self.channel_id
    }

    /// The canonical AI type for the event
    pub fn ai_type(&self) -> &str {
        canonical_ai_type(&self.detection.ai_type)
    }

    /// The reported sub-types, e.g. `dog` under `dog_cat`
    pub fn sub_types(&self) -> Vec<String> {
        self.detection.sub_types()
    }
}

/// Releases the listener slot if `listen_on_smart_ai` bails out part way
/// through, so a failed call does not lock the camera out of ever listening.
struct ListenGuard {
    listening: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl ListenGuard {
    /// Hand the slot over to the [`SmartAiPush`] that will own it from now on
    fn release(mut self) -> Arc<std::sync::atomic::AtomicBool> {
        self.listening.take().expect("guard released twice")
    }
}

impl Drop for ListenGuard {
    fn drop(&mut self) {
        if let Some(listening) = self.listening.take() {
            listening.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Handle on a running YOLO push subscription.
///
/// Push events arrive via [`SmartAiPush::next_event`]; dropping the handle
/// deregisters the message handlers.
pub struct SmartAiPush {
    connection: Arc<BcConnection>,
    /// Cleared once the handlers are actually gone, so the next listener
    /// cannot register while ours are still installed.
    listening: Arc<std::sync::atomic::AtomicBool>,
    rx: Receiver<SmartAiEvent>,
}

impl SmartAiPush {
    /// Await the next push event from the camera.
    pub async fn next_event(&mut self) -> Result<SmartAiEvent> {
        self.rx.recv().await.ok_or(Error::Other("SmartAi dropped"))
    }
}

impl Drop for SmartAiPush {
    fn drop(&mut self) {
        log::trace!("Drop SmartAiPush");
        let connection = self.connection.clone();
        let listening = self.listening.clone();
        let _gt = tokio::runtime::Handle::current().enter();
        tokio::task::spawn(async move {
            let _ = connection.unhandle_msg(MSG_ID_YOLO_DETECT).await;
            let _ = connection.unhandle_msg(MSG_ID_YOLO_DETECT_DETAIL).await;
            // Only now is it safe for another listener to register.
            listening.store(false, std::sync::atomic::Ordering::Release);
            log::trace!("Dropped SmartAiPush");
        });
    }
}

impl BcCamera {
    /// Fetch the current smart-AI configuration for the given detector
    /// kind on this camera's channel.
    pub async fn get_smart_ai(&self, kind: SmartAiKind) -> Result<SmartAiPayload> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(kind.get_id(), msg_num).await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: kind.get_id(),
                channel_id: self.channel_id,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: Some(Extension {
                    channel_id: Some(self.channel_id),
                    ..Default::default()
                }),
                payload: None,
            }),
        };
        sub.send(msg).await?;
        let reply = sub.recv().await?;
        if reply.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: reply.meta.msg_id,
                code: reply.meta.response_code,
            });
        }
        if let BcBody::ModernMsg(ModernMsg {
            payload: Some(BcPayloads::BcXml(mut xml)),
            ..
        }) = reply.body
        {
            let extracted = match kind {
                SmartAiKind::Crossline => {
                    xml.crossline_detect.take().map(SmartAiPayload::Crossline)
                }
                SmartAiKind::Intrusion => {
                    xml.intrusion_detect.take().map(SmartAiPayload::Intrusion)
                }
                SmartAiKind::Loitering => {
                    xml.loitering_detect.take().map(SmartAiPayload::Loitering)
                }
                SmartAiKind::Legacy => xml.legacy_detect.take().map(SmartAiPayload::Legacy),
                SmartAiKind::Loss => xml.loss_detect.take().map(SmartAiPayload::Loss),
            };
            extracted.ok_or(Error::UnintelligibleXml {
                reply: Arc::new(Box::new(xml)),
                why: "Expected smart-AI payload for the requested kind",
            })
        } else {
            Err(Error::UnintelligibleReply {
                reply: Arc::new(Box::new(reply)),
                why: "Expected smart-AI xml payload",
            })
        }
    }

    /// Get the per-AI-type alarm configuration (cmd 342).
    ///
    /// `ai_type` should be a canonical name (see [`AI_CANONICAL_TYPES`]);
    /// aliases like `person` are normalized for you.
    pub async fn get_ai_alarm(&self, ai_type: &str) -> Result<AiDetectCfg> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_GET_AI_ALARM, msg_num).await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_AI_ALARM,
                channel_id: self.channel_id,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: Some(Extension {
                    channel_id: Some(self.channel_id),
                    ..Default::default()
                }),
                payload: Some(BcPayloads::BcXml(BcXml {
                    ai_detect_cfg: Some(AiDetectCfg {
                        version: xml_ver(),
                        channel_id: self.channel_id,
                        ai_type: canonical_ai_type(ai_type).to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
            }),
        };
        sub.send(msg).await?;
        let reply = sub.recv().await?;
        if reply.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: reply.meta.msg_id,
                code: reply.meta.response_code,
            });
        }
        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    ai_detect_cfg: Some(cfg),
                    ..
                })),
            ..
        }) = reply.body
        {
            Ok(cfg)
        } else {
            Err(Error::UnintelligibleReply {
                reply: Arc::new(Box::new(reply)),
                why: "Expected AiDetectCfg",
            })
        }
    }

    /// Set the per-AI-type alarm configuration (cmd 343).
    ///
    /// `cfg.ai_type` is sent verbatim: it should be a type the camera itself
    /// reported, which normally means passing back what
    /// [`BcCamera::get_ai_alarm`] returned. [`canonical_ai_type`] is for
    /// classifying inbound events, and normalizing here could rewrite a name
    /// the camera actually uses.
    pub async fn set_ai_alarm(&self, mut cfg: AiDetectCfg) -> Result<()> {
        cfg.channel_id = self.channel_id;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_SET_AI_ALARM, msg_num).await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_AI_ALARM,
                channel_id: self.channel_id,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: Some(Extension {
                    channel_id: Some(self.channel_id),
                    ..Default::default()
                }),
                payload: Some(BcPayloads::BcXml(BcXml {
                    ai_detect_cfg: Some(cfg),
                    ..Default::default()
                })),
            }),
        };
        sub.send(msg).await?;
        if let Ok(reply) =
            tokio::time::timeout(tokio::time::Duration::from_millis(500), sub.recv()).await
        {
            let reply = reply?;
            if reply.meta.response_code != 200 {
                return Err(Error::CameraServiceUnavailable {
                    id: reply.meta.msg_id,
                    code: reply.meta.response_code,
                });
            }
        }
        Ok(())
    }

    /// Read the `AiCfg` block (cmd 299).
    ///
    /// This carries the auto-tracking settings and, on cameras that support
    /// it, baby-cry detection (`cryDetectAbility` / `cryDetectLevel`).
    pub async fn get_ai_cfg(&self) -> Result<AiCfg> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_GET_AI_CFG, msg_num).await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_AI_CFG,
                channel_id: self.channel_id,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: Some(Extension {
                    channel_id: Some(self.channel_id),
                    ..Default::default()
                }),
                payload: None,
            }),
        };
        sub.send(msg).await?;
        let reply = sub.recv().await?;
        if reply.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: reply.meta.msg_id,
                code: reply.meta.response_code,
            });
        }
        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    ai_cfg: Some(cfg), ..
                })),
            ..
        }) = reply.body
        {
            Ok(cfg)
        } else {
            Err(Error::UnintelligibleReply {
                reply: Arc::new(Box::new(reply)),
                why: "Expected AiCfg",
            })
        }
    }

    /// Write the `AiCfg` block (cmd 300).
    ///
    /// The camera expects the whole block, so read it with
    /// [`BcCamera::get_ai_cfg`] first and edit what you need.
    pub async fn set_ai_cfg(&self, mut cfg: AiCfg) -> Result<()> {
        cfg.channel_id = self.channel_id;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_SET_AI_CFG, msg_num).await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_AI_CFG,
                channel_id: self.channel_id,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: Some(Extension {
                    channel_id: Some(self.channel_id),
                    ..Default::default()
                }),
                payload: Some(BcPayloads::BcXml(BcXml {
                    ai_cfg: Some(cfg),
                    ..Default::default()
                })),
            }),
        };
        sub.send(msg).await?;
        if let Ok(reply) =
            tokio::time::timeout(tokio::time::Duration::from_millis(500), sub.recv()).await
        {
            let reply = reply?;
            if reply.meta.response_code != 200 {
                return Err(Error::CameraServiceUnavailable {
                    id: reply.meta.msg_id,
                    code: reply.meta.response_code,
                });
            }
        }
        Ok(())
    }

    /// The current baby-cry detection sensitivity, or `None` when the camera
    /// does not support cry detection.
    pub async fn get_cry_detection(&self) -> Result<Option<u32>> {
        let cfg = self.get_ai_cfg().await?;
        Ok(if cfg.supports_cry_detection() {
            cfg.cry_detect_level
        } else {
            None
        })
    }

    /// Set the baby-cry detection sensitivity.
    ///
    /// Reads the current [`AiCfg`], changes `cryDetectLevel` and writes it
    /// back, so the auto-tracking settings that share the block are preserved.
    pub async fn set_cry_detection(&self, sensitivity: u32) -> Result<()> {
        let mut cfg = self.get_ai_cfg().await?;
        if !cfg.supports_cry_detection() {
            return Err(Error::Other("Camera does not support cry detection"));
        }
        cfg.cry_detect_level = Some(sensitivity);
        self.set_ai_cfg(cfg).await
    }

    /// Listen for YOLO push events (cmd 600 basic and cmd 696 detailed).
    ///
    /// Ordinary AI detections do not need this — they arrive through the alarm
    /// channel, see [`AiState`]. This is only for the YOLO-world detail that
    /// the newest models push separately.
    ///
    /// Like motion, the camera only pushes once it has been asked to send
    /// events, so this issues the same event-subscribe request
    /// [`BcCamera::listen_on_motion`] does.
    ///
    /// Only one listener per camera is possible: the message handlers are
    /// registered per message id. A second call returns
    /// [`Error::SimultaneousSubscriptionId`] and registers nothing.
    ///
    /// The wrapper element names of the 600/696 payload could not be confirmed
    /// against a camera trace, so a push that cannot be decoded is logged once
    /// at debug level rather than silently dropped.
    pub async fn listen_on_smart_ai(&self) -> Result<SmartAiPush> {
        // Claim the listener slot before touching the connection.
        //
        // `BcConnection::handle_msg` only queues the registration and returns
        // Ok; the poller notices a duplicate asynchronously and answers by
        // returning an error out of its run loop, which tears down the whole
        // connection — streams, motion and all. So a second listener has to be
        // turned away here, before it can register anything.
        if self
            .smart_ai_listening
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            return Err(Error::SimultaneousSubscriptionId {
                msg_id: MSG_ID_YOLO_DETECT,
            });
        }
        let listening = self.smart_ai_listening.clone();
        // From here on every early return must release the slot, so the guard
        // owns it until the SmartAiPush takes over.
        let guard = ListenGuard {
            listening: Some(listening),
        };

        // Ask the camera to start sending us events at all. Without this the
        // handlers below would sit idle forever.
        self.start_motion_query().await?;

        let connection = self.get_connection();
        let channel_id = self.channel_id;
        let (tx, rx) = channel(20);

        // Camera initiated messages must use `handle_msg`, which keys on the
        // message id alone. `subscribe_to_id` rebinds itself to the first
        // message number it sees, so it would go deaf after the first push.
        for (msg_id, detailed) in [
            (MSG_ID_YOLO_DETECT, false),
            (MSG_ID_YOLO_DETECT_DETAIL, true),
        ] {
            let tx = tx.clone();
            let warned = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let result = connection
                .handle_msg(msg_id, move |bc| {
                    let tx = tx.clone();
                    let warned = warned.clone();
                    Box::pin(async move {
                        let events = extract_yolo(bc, channel_id, detailed);
                        if events.is_empty()
                            && !warned.swap(true, std::sync::atomic::Ordering::Relaxed)
                        {
                            log::debug!(
                                "Received a YOLO push (cmd {msg_id}) that could not be decoded; \
                                 please report the payload upstream"
                            );
                        }
                        for event in events {
                            let _ = tx.send(event).await;
                        }
                        Option::<Bc>::None
                    })
                })
                .await;
            if let Err(e) = result {
                // Undo whatever we managed to register so a failed call does
                // not leave a half-installed listener behind.
                let _ = connection.unhandle_msg(MSG_ID_YOLO_DETECT).await;
                let _ = connection.unhandle_msg(MSG_ID_YOLO_DETECT_DETAIL).await;
                return Err(e);
            }
        }

        Ok(SmartAiPush {
            connection,
            listening: guard.release(),
            rx,
        })
    }
}

/// Pull every detection for our channel out of a cmd 600 / 696 push.
///
/// The channel comes from the `<channel>` element of each event, falling back
/// to the message envelope's channel when the payload does not carry one.
fn extract_yolo(msg: &Bc, channel_id: u8, detailed: bool) -> Vec<SmartAiEvent> {
    let envelope_channel = msg.meta.channel_id;
    let list = match &msg.body {
        BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    yolo_world_event_list: Some(list),
                    ..
                })),
            ..
        }) => list,
        _ => return vec![],
    };
    let mut out = vec![];
    for event in list.events.iter() {
        // A camera that omits <channel> leaves this at 0; fall back to the
        // envelope so single channel cameras still match.
        let event_channel = if event.channel == 0 && envelope_channel != 0 {
            envelope_channel
        } else {
            event.channel
        };
        if event_channel != channel_id {
            continue;
        }
        for detection in event.types.iter() {
            if detection.ai_type.is_empty() {
                continue;
            }
            out.push(SmartAiEvent {
                channel_id: event_channel,
                detailed,
                detection: detection.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_ai_kind_ids() {
        assert_eq!(SmartAiKind::Crossline.get_id(), 527);
        assert_eq!(SmartAiKind::Crossline.set_id(), 528);
        assert_eq!(SmartAiKind::Intrusion.get_id(), 529);
        assert_eq!(SmartAiKind::Intrusion.set_id(), 530);
        assert_eq!(SmartAiKind::Loitering.get_id(), 531);
        assert_eq!(SmartAiKind::Loitering.set_id(), 532);
        assert_eq!(SmartAiKind::Legacy.get_id(), 549);
        assert_eq!(SmartAiKind::Legacy.set_id(), 550);
        assert_eq!(SmartAiKind::Loss.get_id(), 551);
        assert_eq!(SmartAiKind::Loss.set_id(), 552);
    }

    #[test]
    fn smart_ai_kind_names_round_trip() {
        for kind in SMART_AI_KINDS.iter().copied() {
            assert_eq!(kind.as_str().parse::<SmartAiKind>().unwrap(), kind);
        }
        assert_eq!(
            "linger".parse::<SmartAiKind>().unwrap(),
            SmartAiKind::Loitering
        );
        assert!("nope".parse::<SmartAiKind>().is_err());
    }

    #[test]
    fn canonical_ai_type_mapping() {
        // reolink_aio AI_DETECT_CONVERSION + YOLO_CONVERSION
        assert_eq!(canonical_ai_type("person"), "people");
        assert_eq!(canonical_ai_type("pet"), "dog_cat");
        assert_eq!(canonical_ai_type("animal"), "dog_cat");
        assert_eq!(canonical_ai_type("motor vehicle"), "vehicle");
        assert_eq!(canonical_ai_type("people"), "people");
        assert_eq!(canonical_ai_type("vehicle"), "vehicle");
        assert_eq!(canonical_ai_type("dog_cat"), "dog_cat");
        assert_eq!(canonical_ai_type("non-motor vehicle"), "non-motor vehicle");
        assert_eq!(canonical_ai_type("package"), "package");
        // Unknown types pass through untouched
        assert_eq!(canonical_ai_type("unknown"), "unknown");
    }

    #[test]
    fn yolo_sub_type_table() {
        assert_eq!(yolo_sub_types("people"), &["man", "woman", "child"]);
        assert_eq!(
            yolo_sub_types("vehicle"),
            &[
                "sedan",
                "suv",
                "pickup_truck",
                "bus",
                "van",
                "truck",
                "motorcycle"
            ]
        );
        assert_eq!(
            yolo_sub_types("dog_cat"),
            &["dog", "cat", "squirrel", "fox", "bear", "cow"]
        );
        assert_eq!(yolo_sub_types("non-motor vehicle"), &["bicycle"]);
        assert_eq!(yolo_sub_types("package"), &["package"]);
        assert!(yolo_sub_types("nonsense").is_empty());
        // Aliases route through canonical_ai_type
        assert_eq!(yolo_sub_types("person"), &["man", "woman", "child"]);
        assert_eq!(
            yolo_sub_types("pet"),
            &["dog", "cat", "squirrel", "fox", "bear", "cow"]
        );
    }

    #[test]
    fn ai_canonical_types_complete() {
        // Mirrors reolink_aio const.py::YOLO_DETECTS
        assert_eq!(AI_CANONICAL_TYPES.len(), 5);
        for expected in [
            "people",
            "vehicle",
            "dog_cat",
            "non-motor vehicle",
            "package",
        ] {
            assert!(AI_CANONICAL_TYPES.contains(&expected), "{}", expected);
        }
    }

    /// Parse a literal camera payload rather than round-tripping our own
    /// serialization, so these tests pin the *wire* format.
    fn parse(xml: &str) -> BcXml {
        BcXml::try_parse(xml.as_bytes()).expect("payload should parse")
    }

    #[test]
    fn crossline_wire_format() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<CrosslineDetect version="1.1">
<channelId>0</channelId>
<crosslineDetectItem>
<location>0</location>
<direction>0</direction>
<enable>1</enable>
<aiType>people,vehicle</aiType>
<sesensitivity>50</sesensitivity>
<stayTime>2</stayTime>
<index>0</index>
<name>line1</name>
</crosslineDetectItem>
</CrosslineDetect>
</body>"#;
        let parsed = parse(xml);
        let detect = parsed.crossline_detect.expect("CrosslineDetect");
        assert_eq!(detect.channel_id, 0);
        assert_eq!(detect.items.len(), 1);
        let item = &detect.items[0];
        assert_eq!(item.enable, 1);
        assert_eq!(item.sesensitivity, 50);
        assert_eq!(item.stay_time, Some(2));
        assert_eq!(item.location, Some(0));
        assert_eq!(item.name.as_deref(), Some("line1"));
        assert_eq!(item.ai_types(), vec!["people", "vehicle"]);
    }

    #[test]
    fn intrusion_and_friends_wire_format() {
        for (element, item_element) in [
            ("IntrusionDetect", "intrusionDetectItem"),
            ("LoiteringDetect", "loiteringDetectItem"),
            ("LegacyDetect", "legacyDetectItem"),
            ("LossDetect", "lossDetectItem"),
        ] {
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<{element} version="1.1">
<channelId>1</channelId>
<{item_element}>
<location>2</location>
<enable>1</enable>
<aiType>dog_cat</aiType>
<sesensitivity>80</sesensitivity>
<timeThresh>3</timeThresh>
<index>1</index>
<name>area1</name>
</{item_element}>
</{element}>
</body>"#
            );
            let parsed = parse(&xml);
            let items = match element {
                "IntrusionDetect" => parsed.intrusion_detect.map(|v| v.items),
                "LoiteringDetect" => parsed.loitering_detect.map(|v| v.items),
                "LegacyDetect" => parsed.legacy_detect.map(|v| v.items),
                _ => parsed.loss_detect.map(|v| v.items),
            }
            .unwrap_or_else(|| panic!("{} should parse", element));
            assert_eq!(items.len(), 1, "{}", element);
            assert_eq!(items[0].location, Some(2), "{}", element);
            assert_eq!(items[0].time_thresh, Some(3), "{}", element);
            assert_eq!(items[0].ai_types(), vec!["dog_cat"], "{}", element);
        }
    }

    #[test]
    fn smart_ai_container_serializes_under_the_right_element() {
        let xml = BcXml {
            loitering_detect: Some(LoiteringDetect {
                version: xml_ver(),
                channel_id: 0,
                op: Some("modify".into()),
                items: vec![SmartDetectItem {
                    enable: 1,
                    ai_type: "people".into(),
                    sesensitivity: 60,
                    stay_time: Some(30),
                    location: Some(0),
                    ..Default::default()
                }],
            }),
            ..Default::default()
        };
        let text = String::from_utf8(xml.serialize(vec![]).unwrap()).unwrap();
        assert!(text.contains("<LoiteringDetect"), "{}", text);
        assert!(text.contains("<loiteringDetectItem>"), "{}", text);
        assert!(
            text.contains("<sesensitivity>60</sesensitivity>"),
            "{}",
            text
        );
        assert!(text.contains("<op>modify</op>"), "{}", text);
        // Nothing from the other detectors leaks in
        assert!(!text.contains("CrosslineDetect"), "{}", text);
    }

    #[test]
    fn ai_detect_cfg_wire_format() {
        // The request template from reolink_aio xmls.py::GetAiAlarm
        let request = BcXml {
            ai_detect_cfg: Some(AiDetectCfg {
                version: xml_ver(),
                channel_id: 3,
                ai_type: "people".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let text = String::from_utf8(request.serialize(vec![]).unwrap()).unwrap();
        assert!(text.contains("<chn>3</chn>"), "{}", text);
        assert!(text.contains("<type>people</type>"), "{}", text);
        assert!(!text.contains("sensitivity"), "{}", text);

        // The reply adds sensitivity + stayTime
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AiDetectCfg version="1.1">
<chn>3</chn>
<type>people</type>
<sensitivity>43</sensitivity>
<stayTime>5</stayTime>
</AiDetectCfg>
</body>"#,
        );
        let cfg = parsed.ai_detect_cfg.expect("AiDetectCfg");
        assert_eq!(cfg.channel_id, 3);
        assert_eq!(cfg.ai_type, "people");
        assert_eq!(cfg.sensitivity, Some(43));
        assert_eq!(cfg.stay_time, Some(5));
    }

    #[test]
    fn ai_cfg_wire_format() {
        // Payload from dissector/messages.md plus the cry fields reolink_aio
        // reads out of the same block.
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AiCfg version="1.1">
<channelId>0</channelId>
<smartTrack>0</smartTrack>
<smartTrackMode>2</smartTrackMode>
<smartTrackModeAbility>14</smartTrackModeAbility>
<detectType>people,vehicle,dog_cat</detectType>
<smartTrackType>people</smartTrackType>
<smartTrackPt>1</smartTrackPt>
<smartTrackObjectStopDelay>20</smartTrackObjectStopDelay>
<smartTrackObjectDisappearDelay>10</smartTrackObjectDisappearDelay>
<cryDetectAbility>1</cryDetectAbility>
<cryDetectLevel>47</cryDetectLevel>
</AiCfg>
</body>"#,
        );
        let cfg = parsed.ai_cfg.expect("AiCfg");
        assert_eq!(cfg.channel_id, 0);
        assert_eq!(cfg.smart_track_mode, Some(2));
        assert_eq!(cfg.smart_track_object_stop_delay, Some(20));
        assert_eq!(cfg.detect_types(), vec!["people", "vehicle", "dog_cat"]);
        assert!(cfg.supports_cry_detection());
        assert_eq!(cfg.cry_detect_level, Some(47));
    }

    #[test]
    fn ai_cfg_without_cry_support() {
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AiCfg version="1.1">
<channelId>0</channelId>
<smartTrack>0</smartTrack>
</AiCfg>
</body>"#,
        );
        let cfg = parsed.ai_cfg.expect("AiCfg");
        assert!(!cfg.supports_cry_detection());
        assert_eq!(cfg.cry_detect_level, None);
        assert!(cfg.detect_types().is_empty());
    }

    #[test]
    fn yolo_push_nesting() {
        // Pins the arity that *is* confirmed: repeated <YoloWorldType>
        // siblings under one event, and a repeated <subTypeList> wrapper each
        // holding a single <subType>. The two outer wrapper names are
        // inferred, so this test is about the shape, not about them.
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<YoloWorldEventList version="1.1">
<YoloWorldEvent>
<channel>2</channel>
<YoloWorldType>
<type>dog_cat</type>
<subTypeList><subType>dog</subType></subTypeList>
<subTypeList><subType>cat</subType></subTypeList>
</YoloWorldType>
<YoloWorldType>
<type>people</type>
</YoloWorldType>
</YoloWorldEvent>
</YoloWorldEventList>
</body>"#,
        );
        let list = parsed.yolo_world_event_list.expect("YoloWorldEventList");
        assert_eq!(list.events.len(), 1);
        let event = &list.events[0];
        assert_eq!(event.channel, 2);
        assert_eq!(event.types.len(), 2);
        assert_eq!(event.types[0].sub_types(), vec!["dog", "cat"]);
        assert!(event.types[1].sub_types().is_empty());
    }

    #[test]
    fn partial_zone_item_still_parses() {
        // A camera that omits fields must not fail the parse: an unparsable
        // payload is a hard error in `bc::de`, which drops the connection.
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<CrosslineDetect version="1.1">
<channelId>0</channelId>
<crosslineDetectItem>
<location>1</location>
</crosslineDetectItem>
</CrosslineDetect>
</body>"#,
        );
        let detect = parsed.crossline_detect.expect("CrosslineDetect");
        let item = &detect.items[0];
        assert_eq!(item.location, Some(1));
        assert_eq!(item.enable, 0);
        assert_eq!(item.sesensitivity, 0);
        assert!(item.ai_types().is_empty());
    }

    #[test]
    fn unmodelled_elements_are_ignored_not_fatal() {
        // Zone geometry is deliberately not modelled. quick-xml must skip it,
        // including when it is nested — that is the whole reason for leaving
        // those fields out.
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<CrosslineDetect version="1.1">
<channelId>0</channelId>
<crosslineDetectItem>
<location>0</location>
<sesensitivity>50</sesensitivity>
<line><point><x>1</x><y>2</y></point></line>
<someFutureField>whatever</someFutureField>
</crosslineDetectItem>
</CrosslineDetect>
</body>"#,
        );
        let detect = parsed.crossline_detect.expect("CrosslineDetect");
        assert_eq!(detect.items[0].sesensitivity, 50);
    }

    #[test]
    fn yolo_detection_sub_types() {
        let detection = YoloWorldType {
            version: xml_ver(),
            ai_type: "vehicle".into(),
            sub_type_lists: vec![
                YoloSubTypeList {
                    sub_type: Some("sedan".into()),
                },
                YoloSubTypeList {
                    sub_type: Some("pickup truck".into()),
                },
            ],
        };
        // Spaces are normalized to underscores, matching reolink_aio
        assert_eq!(detection.sub_types(), vec!["sedan", "pickup_truck"]);
        let event = SmartAiEvent {
            channel_id: 1,
            detailed: true,
            detection,
        };
        assert_eq!(event.ai_type(), "vehicle");
        assert_eq!(event.channel_id(), 1);
    }

    #[test]
    fn alarm_event_ai_types() {
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AlarmEventList version="1.1">
<AlarmEvent version="1.1">
<channelId>0</channelId>
<status>MD</status>
<AItype>people,dog_cat</AItype>
<recording>1</recording>
<timeStamp>0</timeStamp>
</AlarmEvent>
</AlarmEventList>
</body>"#,
        );
        let events = parsed
            .alarm_event_list
            .expect("AlarmEventList")
            .alarm_events;
        assert_eq!(events[0].ai_types(), vec!["people", "dog_cat"]);
    }

    #[test]
    fn ai_state_tracks_alarm_events() {
        let parse_events = |xml: &str| {
            parse(xml)
                .alarm_event_list
                .expect("AlarmEventList")
                .alarm_events
        };

        let mut state = AiState::default();
        let detected = parse_events(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AlarmEventList version="1.1">
<AlarmEvent version="1.1">
<channelId>0</channelId>
<status>MD</status>
<AItype>person,other</AItype>
<recording>1</recording>
<timeStamp>0</timeStamp>
</AlarmEvent>
</AlarmEventList>
</body>"#,
        );
        state.apply_events(detected.iter());
        // `person` is normalized and `other` (plain motion) is not an AI type
        assert!(state.is_detected("people"));
        assert!(state.is_detected("person"));
        assert!(!state.is_detected("other"));
        assert_eq!(state.detected_types().collect::<Vec<_>>(), vec!["people"]);

        // A later event without that type clears it, keeping the key around
        let cleared = parse_events(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AlarmEventList version="1.1">
<AlarmEvent version="1.1">
<channelId>0</channelId>
<status>none</status>
<AItype>none</AItype>
<recording>0</recording>
<timeStamp>0</timeStamp>
</AlarmEvent>
</AlarmEventList>
</body>"#,
        );
        state.apply_events(cleared.iter());
        assert!(!state.is_detected("people"));
        assert!(state.detections().contains_key("people"));
        assert_eq!(state.detected_types().count(), 0);

        // An event with no AItype at all leaves the AI state untouched
        state.detections.insert("vehicle".into(), true);
        let motion_only = parse_events(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AlarmEventList version="1.1">
<AlarmEvent version="1.1">
<channelId>0</channelId>
<status>MD</status>
<recording>0</recording>
<timeStamp>0</timeStamp>
</AlarmEvent>
</AlarmEventList>
</body>"#,
        );
        state.apply_events(motion_only.iter());
        assert!(state.is_detected("vehicle"));
    }

    #[test]
    fn ai_state_folds_a_whole_message_at_once() {
        // A camera may split its report over several <AlarmEvent> elements.
        // Applying them one at a time would let the second wipe the first.
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AlarmEventList version="1.1">
<AlarmEvent version="1.1">
<channelId>0</channelId>
<status>MD</status>
<AItype>people</AItype>
<recording>1</recording>
<timeStamp>0</timeStamp>
<smartAiTypeList>
<smartAiType><type>crossline</type><index>1</index></smartAiType>
</smartAiTypeList>
</AlarmEvent>
<AlarmEvent version="1.1">
<channelId>0</channelId>
<status>MD</status>
<AItype>vehicle</AItype>
<recording>1</recording>
<timeStamp>0</timeStamp>
<smartAiTypeList>
<smartAiType><type>intrusion</type><index>2</index></smartAiType>
</smartAiTypeList>
</AlarmEvent>
</AlarmEventList>
</body>"#,
        );
        let events = parsed
            .alarm_event_list
            .expect("AlarmEventList")
            .alarm_events;
        let mut state = AiState::default();
        state.apply_events(events.iter());
        assert!(state.is_detected("people"));
        assert!(state.is_detected("vehicle"));
        assert!(state.is_zone_detected(SmartAiKind::Crossline));
        assert!(state.is_zone_detected(SmartAiKind::Intrusion));
    }

    #[test]
    fn ai_state_picks_up_visitor_from_status() {
        // A doorbell press is reported in `status`, not in `AItype`.
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AlarmEventList version="1.1">
<AlarmEvent version="1.1">
<channelId>0</channelId>
<status>MD,visitor</status>
<recording>1</recording>
<timeStamp>0</timeStamp>
</AlarmEvent>
</AlarmEventList>
</body>"#,
        );
        let events = parsed
            .alarm_event_list
            .expect("AlarmEventList")
            .alarm_events;
        let mut state = AiState::default();
        state.apply_events(events.iter());
        assert!(state.is_detected("visitor"));
    }

    #[test]
    fn ai_state_tracks_smart_ai_zones() {
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AlarmEventList version="1.1">
<AlarmEvent version="1.1">
<channelId>0</channelId>
<status>MD</status>
<AItype>people</AItype>
<recording>1</recording>
<timeStamp>0</timeStamp>
<smartAiTypeList>
<smartAiType>
<type>crossline</type>
<index>5</index>
</smartAiType>
<smartAiType>
<type>intrusion</type>
<subList>
<index>3</index>
<type>people</type>
</subList>
</smartAiType>
</smartAiTypeList>
</AlarmEvent>
</AlarmEventList>
</body>"#,
        );
        let events = parsed
            .alarm_event_list
            .expect("AlarmEventList")
            .alarm_events;
        let mut state = AiState::default();
        state.apply_events(events.iter());
        // index 5 == bits 0 and 2 == locations 0 and 2
        assert_eq!(
            state.zones().get("crossline"),
            Some(&BTreeSet::from([0, 2])),
        );
        // No bitmask, so the subList indices are used
        assert_eq!(state.zones().get("intrusion"), Some(&BTreeSet::from([3])));
        assert!(state.is_zone_detected(SmartAiKind::Crossline));
        assert!(!state.is_zone_detected(SmartAiKind::Loss));

        // Zones are recomputed on every event, so a quiet event clears them
        let quiet = parse(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AlarmEventList version="1.1">
<AlarmEvent version="1.1">
<channelId>0</channelId>
<status>none</status>
<AItype>none</AItype>
<recording>0</recording>
<timeStamp>0</timeStamp>
</AlarmEvent>
</AlarmEventList>
</body>"#,
        )
        .alarm_event_list
        .expect("AlarmEventList")
        .alarm_events;
        state.apply_events(quiet.iter());
        assert!(state.zones().is_empty());
    }
}
