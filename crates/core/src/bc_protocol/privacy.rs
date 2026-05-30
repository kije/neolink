//! Baichuan privacy-mode (cmd 622 set, 623 get + push)
//!
//! Privacy mode is a hard lens-shutter / blackout state on supported Reolink
//! hardware. While active the HTTP and ONVIF surfaces are unresponsive; only
//! the Baichuan TCP socket stays live. The camera also pushes the new state
//! on cmd 623 whenever it flips (e.g. from a physical button press).
//!
//! The XML shapes are documented on [`SleepState`].

use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};
use tokio::sync::mpsc::{channel, Receiver};

impl BcCamera {
    /// Listen for camera-pushed privacy-mode updates on cmd 623.
    ///
    /// The camera pushes a cmd-623 reply asynchronously whenever the privacy
    /// state flips (e.g. user pressed the physical shutter button or the
    /// state changed via another client). The returned [`Receiver`] yields
    /// `bool` (true = privacy on, false = off) per push.
    ///
    /// The handler is registered on the connection's dispatcher and stays
    /// alive for as long as the connection. Drop the receiver to stop
    /// consuming events (the handler itself is best-effort: if the channel
    /// is full or dropped, events are silently discarded).
    ///
    /// Note: when the persistent push-event subscription from #5 lands,
    /// this hook should be migrated to the broadcast channel — see #19.
    pub async fn listen_on_privacy_mode(&self) -> Result<Receiver<bool>> {
        let (tx, rx) = channel(8);
        let connection = self.get_connection();
        connection
            .handle_msg(MSG_ID_GET_PRIVACY_MODE, move |bc| {
                let tx = tx.clone();
                Box::pin(async move {
                    if let Bc {
                        meta:
                            BcMeta {
                                msg_id: MSG_ID_GET_PRIVACY_MODE,
                                ..
                            },
                        body:
                            BcBody::ModernMsg(ModernMsg {
                                payload:
                                    Some(BcPayloads::BcXml(BcXml {
                                        sleep_state:
                                            Some(SleepState {
                                                sleep: Some(s), ..
                                            }),
                                        ..
                                    })),
                                ..
                            }),
                    } = bc
                    {
                        let _ = tx.send(*s != 0).await;
                    }
                    None
                })
            })
            .await?;
        Ok(rx)
    }

    /// Get the current privacy-mode state.
    ///
    /// Returns `true` if privacy mode is currently on (shutter closed),
    /// `false` if off. Errors propagate when the camera refuses the
    /// command (e.g. unsupported model) or returns an unparsable reply.
    pub async fn get_privacy_mode(&self) -> Result<bool> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_GET_PRIVACY_MODE, msg_num)
            .await?;

        let get = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_PRIVACY_MODE,
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

        sub.send(get).await?;
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
                    sleep_state: Some(SleepState { sleep, .. }),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(sleep.unwrap_or(0) != 0)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected sleepState xml on privacy-mode reply but it was missing",
            })
        }
    }

    /// Set the privacy-mode state.
    ///
    /// `true` enables privacy mode (closes the shutter and silences the
    /// HTTP / ONVIF surfaces); `false` disables it. The Baichuan TCP
    /// socket remains live in both states, so a future `set_privacy_mode`
    /// is always reachable.
    ///
    /// The wire payload uses the magic `operate = 2` flag together with
    /// `sleep = 0|1`; see [`SleepState`] for details.
    pub async fn set_privacy_mode(&self, enable: bool) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_SET_PRIVACY_MODE, msg_num)
            .await?;

        let payload = SleepState {
            version: Some(xml_ver()),
            operate: Some(2),
            sleep: Some(if enable { 1 } else { 0 }),
        };

        let set = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_PRIVACY_MODE,
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
                    sleep_state: Some(payload),
                    ..Default::default()
                })),
            }),
        };

        sub.send(set).await?;

        // Some cameras don't send a reply on success — mirror the
        // existing floodlight / LED pattern and treat a 500 ms silence
        // as ok.
        if let Ok(reply) =
            tokio::time::timeout(tokio::time::Duration::from_millis(500), sub.recv()).await
        {
            let msg = reply?;
            if let BcMeta {
                response_code: 200, ..
            } = msg.meta
            {
                Ok(())
            } else {
                Err(Error::UnintelligibleReply {
                    reply: std::sync::Arc::new(Box::new(msg)),
                    why: "The camera did not accept the privacy-mode set command",
                })
            }
        } else {
            Ok(())
        }
    }
}
