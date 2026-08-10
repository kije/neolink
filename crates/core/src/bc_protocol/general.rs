use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};

impl BcCamera {
    /// Get the [SystemGeneral] xml from the camera.
    ///
    /// This is the same message [`BcCamera::get_time`] reads, but it hands back
    /// the whole structure rather than just the clock fields. Besides the time
    /// it carries the camera's display name, its OSD date format and its
    /// language.
    pub async fn get_general(&self) -> Result<SystemGeneral> {
        self.has_ability_ro("general").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub_get = connection.subscribe(MSG_ID_GET_GENERAL, msg_num).await?;
        let get = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_GENERAL,
                channel_id: self.channel_id,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg::default()),
        };

        sub_get.send(get).await?;
        let msg = sub_get.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    system_general: Some(general),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(general)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected SystemGeneral xml but it was not received",
            })
        }
    }

    /// Write a [SystemGeneral] back to the camera.
    ///
    /// Every field is optional and Reolink treats an omitted field as "leave
    /// alone", so the safe way to change one setting is to read the current
    /// structure, edit the field and send the result back.
    pub async fn set_general(&self, general: SystemGeneral) -> Result<()> {
        self.has_ability_rw("general").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub_set = connection.subscribe(MSG_ID_SET_GENERAL, msg_num).await?;
        let set = Bc::new_from_xml(
            BcMeta {
                msg_id: MSG_ID_SET_GENERAL,
                channel_id: self.channel_id,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            BcXml {
                system_general: Some(general),
                ..Default::default()
            },
        );

        sub_set.send(set).await?;
        let msg = sub_set.recv().await?;
        if let BcMeta {
            response_code: 200, ..
        } = msg.meta
        {
            Ok(())
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "The camera did not accept the SystemGeneral xml",
            })
        }
    }
}
