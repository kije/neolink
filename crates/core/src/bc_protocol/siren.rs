//! Trigger and observe the siren
//!
//! Three protocol surfaces are exposed:
//!
//! - The legacy `siren()` method (cmd 263, [`MSG_ID_PLAY_AUDIO`]) which is
//!   the long-standing audio-play-info path used by older firmwares.
//! - Manual control via cmd 481 ([`MSG_ID_SIREN_MANUAL`]).
//! - Scheduled siren via cmd 480 ([`MSG_ID_SET_SIREN_TIMES`]).
//! - Push events via cmd 547 ([`MSG_ID_SIREN_STATUS`]).

use tokio::sync::mpsc::{channel, Receiver};

use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};

impl BcCamera {
    /// Trigger the siren via the legacy audioPlayInfo (cmd 263) path.
    pub async fn siren(&self) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub_get = connection.subscribe(MSG_ID_PLAY_AUDIO, msg_num).await?;
        let get = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_PLAY_AUDIO,
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
                    audio_play_info: Some(AudioPlayInfo {
                        channel_id: self.channel_id,
                        play_mode: 0,
                        play_duration: 0,
                        play_times: 1,
                        on_off: 0,
                    }),
                    ..Default::default()
                })),
            }),
        };

        sub_get.send(get).await?;
        let msg = sub_get.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        Ok(())
    }

    /// Fire the siren manually for `duration` seconds (cmd 481).
    ///
    /// Pass `duration == 0` to use the firmware's default duration.
    /// To stop a currently-firing siren, call [`Self::stop_siren`].
    pub async fn fire_siren(&self, duration: u32) -> Result<()> {
        self.send_siren_manual(SirenManual {
            version: "1.1".to_string(),
            channel_id: self.channel_id,
            play_mode: 0,
            play_duration: duration,
            play_times: 1,
            on_off: 1,
        })
        .await
    }

    /// Stop a currently-firing siren (cmd 481 with `onOff=0`).
    pub async fn stop_siren(&self) -> Result<()> {
        self.send_siren_manual(SirenManual {
            version: "1.1".to_string(),
            channel_id: self.channel_id,
            play_mode: 0,
            play_duration: 0,
            play_times: 0,
            on_off: 0,
        })
        .await
    }

    async fn send_siren_manual(&self, payload: SirenManual) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_SIREN_MANUAL, msg_num).await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SIREN_MANUAL,
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
                    siren_manual: Some(payload),
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
        Ok(())
    }

    /// Configure the scheduled-siren windows (cmd 480).
    ///
    /// `play_times` controls how many times the siren fires within the
    /// schedule window. `play_duration` is in seconds. `enable` toggles
    /// the schedule on / off.
    pub async fn schedule_siren(
        &self,
        play_times: u32,
        play_duration: u32,
        enable: bool,
    ) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_SET_SIREN_TIMES, msg_num)
            .await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_SIREN_TIMES,
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
                    siren_times: Some(SirenTimes {
                        version: "1.1".to_string(),
                        channel_id: self.channel_id,
                        play_mode: 2,
                        play_duration,
                        play_times,
                        on_off: if enable { 1 } else { 0 },
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
        Ok(())
    }

    /// Subscribe to siren-status push events (cmd 547).
    ///
    /// Each push delivers the boolean siren state (`true` = firing,
    /// `false` = stopped).
    pub async fn siren_status_stream(&self) -> Result<Receiver<bool>> {
        let (tx, rx) = channel(8);
        let connection = self.get_connection();
        connection
            .handle_msg(MSG_ID_SIREN_STATUS, move |bc| {
                let tx = tx.clone();
                Box::pin(async move {
                    if let Bc {
                        meta: BcMeta {
                            msg_id: MSG_ID_SIREN_STATUS,
                            ..
                        },
                        body:
                            BcBody::ModernMsg(ModernMsg {
                                payload:
                                    Some(BcPayloads::BcXml(BcXml {
                                        siren_status: Some(status),
                                        ..
                                    })),
                                ..
                            }),
                    } = bc
                    {
                        let _ = tx.send(status.status != 0).await;
                    }
                    None
                })
            })
            .await?;
        Ok(rx)
    }
}
