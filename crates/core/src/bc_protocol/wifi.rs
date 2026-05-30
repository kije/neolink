//! Handles WiFi info messages
//!
//! Three commands are exposed:
//!
//! - [`MSG_ID_GET_WIFI_SIGNAL`] (cmd 115) — current associated SSID + RSSI
//! - [`MSG_ID_GET_WIFI`] (cmd 116) — current SSID + scan list
//! - [`MSG_ID_NETWORK_LINK_TYPE`] (cmd 464) — push-only when the network
//!   link type or RSSI changes
//!
//! The XML field names follow the `starkillerOG/reolink_aio` reference
//! and may need adjustment once tested against real firmware.

use tokio::sync::mpsc::{channel, Receiver};

use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};

impl BcCamera {
    /// Get just the WiFi signal info (current SSID + RSSI) via cmd 115.
    pub async fn get_wifi_signal(&self) -> Result<Wifi> {
        self.send_wifi_request(MSG_ID_GET_WIFI_SIGNAL).await
    }

    /// Get the full WiFi configuration including the AP scan list via cmd 116.
    pub async fn get_wifi(&self) -> Result<Wifi> {
        self.send_wifi_request(MSG_ID_GET_WIFI).await
    }

    async fn send_wifi_request(&self, msg_id: u32) -> Result<Wifi> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(msg_id, msg_num).await?;

        let msg = Bc {
            meta: BcMeta {
                msg_id,
                channel_id: self.channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
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

        if let Bc {
            body:
                BcBody::ModernMsg(ModernMsg {
                    payload:
                        Some(BcPayloads::BcXml(BcXml {
                            wifi: Some(wifi), ..
                        })),
                    ..
                }),
            ..
        } = reply
        {
            Ok(wifi)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(reply)),
                why: "The camera did not return a Wifi xml payload",
            })
        }
    }

    /// Subscribe to network-link-type push events (cmd 464).
    ///
    /// The camera sends these when the link type or RSSI changes. The
    /// receiver yields the parsed [`Wifi`] payload each time.
    pub async fn network_link_type_stream(&self) -> Result<Receiver<Wifi>> {
        let (tx, rx) = channel(3);
        let connection = self.get_connection();
        connection
            .handle_msg(MSG_ID_NETWORK_LINK_TYPE, move |bc| {
                let tx = tx.clone();
                Box::pin(async move {
                    if let Bc {
                        meta: BcMeta {
                            msg_id: MSG_ID_NETWORK_LINK_TYPE,
                            ..
                        },
                        body:
                            BcBody::ModernMsg(ModernMsg {
                                payload:
                                    Some(BcPayloads::BcXml(BcXml {
                                        wifi: Some(wifi), ..
                                    })),
                                ..
                            }),
                    } = bc
                    {
                        let _ = tx.send(wifi.clone()).await;
                    }
                    None
                })
            })
            .await?;
        Ok(rx)
    }
}
