//! Second-generation Reolink AI / smart-detect surface over Baichuan.
//!
//! Wraps cmd ids 527-552 (smart-AI zones), 600/696 (YOLO push events),
//! 342/343 (AI alarm config), and 299/300 (baby-cry detection).
//!
//! The five smart-AI kinds share a common get/set shape, so they are
//! dispatched via the [`SmartAiKind`] enum. Each kind has its own
//! per-zone item type in [`crate::bc::xml`].
//!
//! See kije/neolink#6 for the full spec.

use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};
use tokio::sync::mpsc::{channel, Receiver};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Which smart-AI detector cmd-pair to operate on.
///
/// Each variant maps to a `(get, set)` MSG_ID pair. The shape of the
/// per-zone item differs per variant (e.g. crossline has a `line`
/// coordinate while intrusion / loitering have a `region`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

/// Untagged enum representing the contents of any smart-AI cmd reply.
///
/// Returned by [`BcCamera::get_smart_ai`]. The variant indicates which
/// detector replied, and carries the channel-id, zone list, and any
/// kind-specific fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SmartAiPayload {
    /// Crossline (cmd 527) reply
    Crossline(CrosslineDetection),
    /// Intrusion (cmd 529) reply
    Intrusion(IntrusionDetection),
    /// Loitering (cmd 531) reply
    Loitering(LoiteringDetection),
    /// Legacy / forgotten-object (cmd 549) reply
    Legacy(LegacyDetection),
    /// Loss / taken-object (cmd 551) reply
    Loss(LossDetection),
}

impl SmartAiPayload {
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
}

/// A smart-AI push event received from the camera.
///
/// Wraps either a YOLO basic / detailed event or a cry-detection event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SmartAiEvent {
    /// YOLO basic event (cmd 600)
    YoloBasic(YoloDetectInfo),
    /// YOLO detailed event with sub-class (cmd 696)
    YoloDetailed(YoloWorldType),
    /// Baby-cry event (cmd 299 push)
    Cry(CryDetection),
}

impl SmartAiEvent {
    /// The channel id the event was reported on
    pub fn channel_id(&self) -> u8 {
        match self {
            Self::YoloBasic(v) => v.channel_id,
            Self::YoloDetailed(v) => v.channel_id,
            Self::Cry(v) => v.channel_id,
        }
    }

    /// The canonical AI type for the event, or `None` for cry events.
    pub fn ai_type(&self) -> Option<&str> {
        match self {
            Self::YoloBasic(v) => Some(canonical_ai_type(&v.ai_type)),
            Self::YoloDetailed(v) => Some(canonical_ai_type(&v.ai_type)),
            Self::Cry(_) => None,
        }
    }

    /// For YOLO detailed events, returns the list of reported sub-types,
    /// preferring the `subTypeList` form over a single `subType`.
    pub fn sub_types(&self) -> Vec<String> {
        match self {
            Self::YoloDetailed(v) => {
                if let Some(list) = &v.sub_type_list {
                    list.sub_types.clone()
                } else if let Some(s) = &v.sub_type {
                    vec![s.clone()]
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }
}

/// Handle on a running smart-AI push subscription.
///
/// Push events arrive via [`SmartAiPush::next_event`]; dropping the
/// handle cancels the background task.
pub struct SmartAiPush {
    handle: JoinSet<Result<()>>,
    cancel: CancellationToken,
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
        self.cancel.cancel();
        let mut handle = std::mem::take(&mut self.handle);
        let _gt = tokio::runtime::Handle::current().enter();
        tokio::task::spawn(async move {
            while handle.join_next().await.is_some() {}
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
                SmartAiKind::Crossline => xml.crossline_detection.take().map(SmartAiPayload::Crossline),
                SmartAiKind::Intrusion => xml.intrusion_detection.take().map(SmartAiPayload::Intrusion),
                SmartAiKind::Loitering => xml.loitering_detection.take().map(SmartAiPayload::Loitering),
                SmartAiKind::Legacy => xml.legacy_detection.take().map(SmartAiPayload::Legacy),
                SmartAiKind::Loss => xml.loss_detection.take().map(SmartAiPayload::Loss),
            };
            extracted.ok_or(Error::UnintelligibleXml {
                reply: std::sync::Arc::new(Box::new(xml)),
                why: "Expected smart-AI payload for the requested kind",
            })
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(reply)),
                why: "Expected smart-AI xml payload",
            })
        }
    }

    /// Push a new smart-AI configuration to the camera. The variant of
    /// `payload` must match `kind`; mismatch yields [`Error::Other`].
    pub async fn set_smart_ai(&self, kind: SmartAiKind, payload: SmartAiPayload) -> Result<()> {
        let mut xml = BcXml::default();
        match (kind, payload) {
            (SmartAiKind::Crossline, SmartAiPayload::Crossline(v)) => {
                xml.crossline_detection = Some(v);
            }
            (SmartAiKind::Intrusion, SmartAiPayload::Intrusion(v)) => {
                xml.intrusion_detection = Some(v);
            }
            (SmartAiKind::Loitering, SmartAiPayload::Loitering(v)) => {
                xml.loitering_detection = Some(v);
            }
            (SmartAiKind::Legacy, SmartAiPayload::Legacy(v)) => {
                xml.legacy_detection = Some(v);
            }
            (SmartAiKind::Loss, SmartAiPayload::Loss(v)) => {
                xml.loss_detection = Some(v);
            }
            _ => return Err(Error::Other("SmartAi kind / payload mismatch")),
        };
        self.send_smart_ai_set(kind.set_id(), xml).await
    }

    async fn send_smart_ai_set(&self, set_id: u32, xml: BcXml) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(set_id, msg_num).await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: set_id,
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
                payload: Some(BcPayloads::BcXml(xml)),
            }),
        };
        sub.send(msg).await?;
        // Some cameras don't reply on success - mirror pirstate.rs.
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

    /// Get the per-AI-type alarm configuration (cmd 342).
    ///
    /// `ai_type` should be a canonical name (see [`AI_CANONICAL_TYPES`]).
    pub async fn get_ai_alarm(&self, ai_type: &str) -> Result<AiDetectCfg> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_GET_AI_ALARM, msg_num).await?;
        let canonical = canonical_ai_type(ai_type).to_string();
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
                        ai_type: canonical,
                        sesensitivity: 0,
                        stay_time: None,
                        area_mask: None,
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
                reply: std::sync::Arc::new(Box::new(reply)),
                why: "Expected AiDetectCfg",
            })
        }
    }

    /// Set the per-AI-type alarm configuration (cmd 343).
    pub async fn set_ai_alarm(&self, mut cfg: AiDetectCfg) -> Result<()> {
        // Normalize the AI type so callers using `person` / `pet` work.
        cfg.ai_type = canonical_ai_type(&cfg.ai_type).to_string();
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

    /// Get the cry-detection configuration (cmd 299).
    pub async fn get_cry_detection(&self) -> Result<CryDetection> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_GET_CRY_DETECTION, msg_num)
            .await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_CRY_DETECTION,
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
                    cry_detection: Some(cry),
                    ..
                })),
            ..
        }) = reply.body
        {
            Ok(cry)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(reply)),
                why: "Expected CryDetection",
            })
        }
    }

    /// Set the cry-detection configuration (cmd 300).
    pub async fn set_cry_detection(&self, cry: CryDetection) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_SET_CRY_DETECTION, msg_num)
            .await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_CRY_DETECTION,
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
                    cry_detection: Some(cry),
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

    /// Listen for push smart-AI events.
    ///
    /// Forwards YOLO basic (cmd 600), YOLO detailed (cmd 696), and cry
    /// (cmd 299 push) onto a single channel as [`SmartAiEvent`].
    ///
    /// Push delivery depends on the persistent subscription added by the
    /// push-channel work; on cameras that don't push, callers should
    /// poll [`BcCamera::get_cry_detection`] etc. instead.
    pub async fn listen_on_smart_ai(&self) -> Result<SmartAiPush> {
        let connection = self.get_connection();
        let channel_id = self.channel_id;
        let (tx, rx) = channel(20);
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        let mut set = JoinSet::new();
        set.spawn(async move {
            tokio::select! {
                _ = thread_cancel.cancelled() => Result::Ok(()),
                v = async {
                    let mut sub_basic = connection.subscribe_to_id(MSG_ID_YOLO_DETECT).await?;
                    let mut sub_detail = connection.subscribe_to_id(MSG_ID_YOLO_DETECT_DETAIL).await?;
                    let mut sub_cry = connection.subscribe_to_id(MSG_ID_GET_CRY_DETECTION).await?;
                    loop {
                        tokio::task::yield_now().await;
                        let event = tokio::select! {
                            msg = sub_basic.recv() => extract_basic(msg?, channel_id),
                            msg = sub_detail.recv() => extract_detail(msg?, channel_id),
                            msg = sub_cry.recv() => extract_cry(msg?, channel_id),
                        };
                        if let Some(event) = event {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(())
                } => v,
            }
        });
        Ok(SmartAiPush {
            handle: set,
            cancel,
            rx,
        })
    }
}

fn extract_basic(msg: Bc, channel_id: u8) -> Option<SmartAiEvent> {
    if let BcBody::ModernMsg(ModernMsg {
        payload:
            Some(BcPayloads::BcXml(BcXml {
                yolo_detect_info: Some(yolo),
                ..
            })),
        ..
    }) = msg.body
    {
        if yolo.channel_id == channel_id {
            return Some(SmartAiEvent::YoloBasic(yolo));
        }
    }
    None
}

fn extract_detail(msg: Bc, channel_id: u8) -> Option<SmartAiEvent> {
    if let BcBody::ModernMsg(ModernMsg {
        payload:
            Some(BcPayloads::BcXml(BcXml {
                yolo_world_type: Some(yolo),
                ..
            })),
        ..
    }) = msg.body
    {
        if yolo.channel_id == channel_id {
            return Some(SmartAiEvent::YoloDetailed(yolo));
        }
    }
    None
}

fn extract_cry(msg: Bc, channel_id: u8) -> Option<SmartAiEvent> {
    if let BcBody::ModernMsg(ModernMsg {
        payload:
            Some(BcPayloads::BcXml(BcXml {
                cry_detection: Some(cry),
                ..
            })),
        ..
    }) = msg.body
    {
        if cry.channel_id == channel_id {
            return Some(SmartAiEvent::Cry(cry));
        }
    }
    None
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
    fn canonical_ai_type_mapping() {
        assert_eq!(canonical_ai_type("person"), "people");
        assert_eq!(canonical_ai_type("pet"), "dog_cat");
        assert_eq!(canonical_ai_type("motor"), "vehicle");
        assert_eq!(canonical_ai_type("motor vehicle"), "vehicle");
        assert_eq!(canonical_ai_type("people"), "people");
        assert_eq!(canonical_ai_type("vehicle"), "vehicle");
        assert_eq!(canonical_ai_type("dog_cat"), "dog_cat");
        assert_eq!(canonical_ai_type("non-motor vehicle"), "non-motor vehicle");
        assert_eq!(canonical_ai_type("package"), "package");
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
        assert!(yolo_sub_types("package").is_empty());
        // Aliases route through canonical_ai_type
        assert_eq!(yolo_sub_types("person"), &["man", "woman", "child"]);
        assert_eq!(
            yolo_sub_types("pet"),
            &["dog", "cat", "squirrel", "fox", "bear", "cow"]
        );
    }

    #[test]
    fn ai_canonical_types_complete() {
        assert!(AI_CANONICAL_TYPES.contains(&"people"));
        assert!(AI_CANONICAL_TYPES.contains(&"vehicle"));
        assert!(AI_CANONICAL_TYPES.contains(&"dog_cat"));
        assert!(AI_CANONICAL_TYPES.contains(&"non-motor vehicle"));
        assert!(AI_CANONICAL_TYPES.contains(&"package"));
    }

    /// Build a fresh `BcXml` via the supplied factory twice, serialize
    /// the first, parse it back, and assert they're equal. This pattern
    /// is used so we don't need `BcXml: Clone`.
    fn assert_round_trip(factory: impl Fn() -> BcXml) {
        let original = factory();
        let serialized = factory().serialize(vec![]).unwrap();
        let parsed = BcXml::try_parse(serialized.as_slice()).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn crossline_round_trip() {
        assert_round_trip(|| BcXml {
            crossline_detection: Some(CrosslineDetection {
                version: xml_ver(),
                channel_id: 0,
                items: vec![CrosslineDetectItem {
                    enable: 1,
                    ai_type: "people".into(),
                    sesensitivity: 50,
                    stay_time: Some(2),
                    direction: Some(0),
                    index: Some(0),
                    name: Some("line1".into()),
                    line: Some("0,0;100,100".into()),
                }],
            }),
            ..Default::default()
        });
    }

    #[test]
    fn intrusion_round_trip() {
        assert_round_trip(|| BcXml {
            intrusion_detection: Some(IntrusionDetection {
                version: xml_ver(),
                channel_id: 0,
                items: vec![IntrusionDetectItem {
                    enable: 1,
                    ai_type: "vehicle".into(),
                    sesensitivity: 80,
                    stay_time: Some(3),
                    index: Some(1),
                    name: Some("area1".into()),
                    region: Some("0,0;1,1;2,2;3,3".into()),
                }],
            }),
            ..Default::default()
        });
    }

    #[test]
    fn loitering_round_trip() {
        assert_round_trip(|| BcXml {
            loitering_detection: Some(LoiteringDetection {
                version: xml_ver(),
                channel_id: 0,
                items: vec![LoiteringDetectItem {
                    enable: 0,
                    ai_type: "people".into(),
                    sesensitivity: 60,
                    stay_time: Some(30),
                    index: Some(0),
                    name: Some("area1".into()),
                    region: None,
                }],
            }),
            ..Default::default()
        });
    }

    #[test]
    fn legacy_round_trip() {
        assert_round_trip(|| BcXml {
            legacy_detection: Some(LegacyDetection {
                version: xml_ver(),
                channel_id: 0,
                items: vec![LegacyDetectItem {
                    enable: 1,
                    ai_type: "package".into(),
                    sesensitivity: 70,
                    stay_time: Some(60),
                    index: Some(0),
                    name: Some("zone1".into()),
                    region: Some("polygon".into()),
                }],
            }),
            ..Default::default()
        });
    }

    #[test]
    fn loss_round_trip() {
        assert_round_trip(|| BcXml {
            loss_detection: Some(LossDetection {
                version: xml_ver(),
                channel_id: 0,
                items: vec![LossDetectItem {
                    enable: 1,
                    ai_type: "package".into(),
                    sesensitivity: 50,
                    stay_time: Some(15),
                    index: Some(0),
                    name: Some("zone1".into()),
                    region: None,
                }],
            }),
            ..Default::default()
        });
    }

    #[test]
    fn yolo_basic_round_trip() {
        assert_round_trip(|| BcXml {
            yolo_detect_info: Some(YoloDetectInfo {
                version: xml_ver(),
                channel_id: 0,
                ai_type: "people".into(),
            }),
            ..Default::default()
        });
    }

    #[test]
    fn yolo_detailed_round_trip_single() {
        assert_round_trip(|| BcXml {
            yolo_world_type: Some(YoloWorldType {
                version: xml_ver(),
                channel_id: 0,
                ai_type: "dog_cat".into(),
                sub_type: Some("dog".into()),
                sub_type_list: None,
            }),
            ..Default::default()
        });
    }

    #[test]
    fn yolo_detailed_round_trip_list() {
        assert_round_trip(|| BcXml {
            yolo_world_type: Some(YoloWorldType {
                version: xml_ver(),
                channel_id: 0,
                ai_type: "vehicle".into(),
                sub_type: None,
                sub_type_list: Some(YoloSubTypeList {
                    sub_types: vec!["sedan".into(), "truck".into()],
                }),
            }),
            ..Default::default()
        });
    }

    #[test]
    fn ai_detect_cfg_round_trip() {
        assert_round_trip(|| BcXml {
            ai_detect_cfg: Some(AiDetectCfg {
                version: xml_ver(),
                channel_id: 0,
                ai_type: "people".into(),
                sesensitivity: 50,
                stay_time: Some(2),
                area_mask: Some("ffffffff".into()),
            }),
            ..Default::default()
        });
    }

    #[test]
    fn cry_detection_round_trip() {
        assert_round_trip(|| BcXml {
            cry_detection: Some(CryDetection {
                version: xml_ver(),
                channel_id: 0,
                enable: 1,
                sesensitivity: Some(50),
                stay_time: Some(2),
            }),
            ..Default::default()
        });
    }

    #[test]
    fn smart_ai_event_helpers() {
        let basic = SmartAiEvent::YoloBasic(YoloDetectInfo {
            version: xml_ver(),
            channel_id: 1,
            ai_type: "person".into(),
        });
        assert_eq!(basic.channel_id(), 1);
        assert_eq!(basic.ai_type(), Some("people"));
        assert!(basic.sub_types().is_empty());

        let detailed = SmartAiEvent::YoloDetailed(YoloWorldType {
            version: xml_ver(),
            channel_id: 2,
            ai_type: "pet".into(),
            sub_type: Some("dog".into()),
            sub_type_list: None,
        });
        assert_eq!(detailed.channel_id(), 2);
        assert_eq!(detailed.ai_type(), Some("dog_cat"));
        assert_eq!(detailed.sub_types(), vec!["dog".to_string()]);

        let detailed_list = SmartAiEvent::YoloDetailed(YoloWorldType {
            version: xml_ver(),
            channel_id: 0,
            ai_type: "vehicle".into(),
            sub_type: None,
            sub_type_list: Some(YoloSubTypeList {
                sub_types: vec!["sedan".into(), "truck".into()],
            }),
        });
        assert_eq!(detailed_list.sub_types(), vec!["sedan", "truck"]);

        let cry = SmartAiEvent::Cry(CryDetection {
            version: xml_ver(),
            channel_id: 3,
            enable: 1,
            sesensitivity: None,
            stay_time: None,
        });
        assert_eq!(cry.channel_id(), 3);
        assert_eq!(cry.ai_type(), None);
    }
}
