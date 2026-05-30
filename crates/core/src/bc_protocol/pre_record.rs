//! Pre-record (battery-camera pre-roll) configuration.
//!
//! Implements the cmd 594 (`MSG_ID_GET_PRE_RECORD`) and cmd 595
//! (`MSG_ID_SET_PRE_RECORD`) round-trip. The XML carries a
//! [`LongRunModeCfg`] payload describing how many seconds of pre-roll the
//! camera should keep buffered, at what framerate, optionally bounded by a
//! daily schedule and an auto-disable battery threshold.

use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};

impl BcCamera {
    /// Read the camera's current pre-record configuration.
    pub async fn get_pre_record(&self) -> Result<LongRunModeCfg> {
        // Pre-record is a recording-side feature; gate on the same ability
        // as the other record-config commands.
        self.has_ability_ro("record").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_GET_PRE_RECORD, msg_num).await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_PRE_RECORD,
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

        sub.send(send).await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    long_run_mode_cfg: Some(cfg),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(cfg)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected LongRunModeCfg xml but it was not received",
            })
        }
    }

    /// Write a new pre-record configuration to the camera.
    ///
    /// `cfg.channel_id` is overridden with `self.channel_id` so callers do
    /// not need to thread it through.
    pub async fn set_pre_record(&self, mut cfg: LongRunModeCfg) -> Result<()> {
        self.has_ability_rw("record").await?;
        cfg.channel_id = self.channel_id;
        if cfg.version.is_none() {
            cfg.version = Some(xml_ver());
        }

        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_SET_PRE_RECORD, msg_num).await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_PRE_RECORD,
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
                    long_run_mode_cfg: Some(cfg),
                    ..Default::default()
                })),
            }),
        };

        sub.send(send).await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code == 200 {
            Ok(())
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "The camera did not accept the LongRunModeCfg xml",
            })
        }
    }
}
