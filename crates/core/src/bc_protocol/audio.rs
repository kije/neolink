//! Handles audio configuration messages
//!
//! Two pairs of get/set commands are exposed:
//!
//! - [`MSG_ID_GET_AUDIO_CFG`] / [`MSG_ID_SET_AUDIO_CFG`] (cmd 264 / 265):
//!   speaker + microphone volume and the configured audio encoding.
//! - [`MSG_ID_GET_AUDIO_NOISE`] / [`MSG_ID_SET_AUDIO_NOISE`]
//!   (cmd 438 / 439): noise-reduction enable, level, and denoise mode
//!   (`ai` vs `classic`).
//!
//! The XML field names follow the `starkillerOG/reolink_aio` reference
//! and may need adjustment once tested against real firmware.

use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};

impl BcCamera {
    /// Get the current audio configuration (cmd 264) for `channel`.
    pub async fn get_audio_cfg(&self, channel: u8) -> Result<AudioCfg> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_GET_AUDIO_CFG, msg_num).await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_AUDIO_CFG,
                channel_id: channel,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: Some(Extension {
                    channel_id: Some(channel),
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
                            audio_cfg: Some(cfg),
                            ..
                        })),
                    ..
                }),
            ..
        } = reply
        {
            Ok(cfg)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(reply)),
                why: "The camera did not return an AudioCfg xml payload",
            })
        }
    }

    /// Set the audio configuration (cmd 265).
    pub async fn set_audio_cfg(&self, cfg: AudioCfg) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let channel = cfg.channel_id;
        let mut sub = connection.subscribe(MSG_ID_SET_AUDIO_CFG, msg_num).await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_AUDIO_CFG,
                channel_id: channel,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: Some(Extension {
                    channel_id: Some(channel),
                    ..Default::default()
                }),
                payload: Some(BcPayloads::BcXml(BcXml {
                    audio_cfg: Some(cfg),
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

    /// Convenience: update only the speaker volume.
    pub async fn set_speaker_volume(&self, channel: u8, volume: u8) -> Result<()> {
        let mut cfg = self.get_audio_cfg(channel).await?;
        cfg.speaker_volume = volume;
        self.set_audio_cfg(cfg).await
    }

    /// Convenience: update only the microphone volume.
    pub async fn set_mic_volume(&self, channel: u8, volume: u8) -> Result<()> {
        let mut cfg = self.get_audio_cfg(channel).await?;
        cfg.mic_volume = volume;
        self.set_audio_cfg(cfg).await
    }

    /// Get the audio noise-reduction configuration (cmd 438).
    pub async fn get_audio_noise(&self, channel: u8) -> Result<AudioNoise> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_GET_AUDIO_NOISE, msg_num)
            .await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_AUDIO_NOISE,
                channel_id: channel,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: Some(Extension {
                    channel_id: Some(channel),
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
                            audio_noise: Some(noise),
                            ..
                        })),
                    ..
                }),
            ..
        } = reply
        {
            Ok(noise)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(reply)),
                why: "The camera did not return an AudioNoise xml payload",
            })
        }
    }

    /// Set the audio noise-reduction configuration (cmd 439).
    pub async fn set_audio_noise(&self, noise: AudioNoise) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let channel = noise.channel_id;
        let mut sub = connection
            .subscribe(MSG_ID_SET_AUDIO_NOISE, msg_num)
            .await?;
        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_AUDIO_NOISE,
                channel_id: channel,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: Some(Extension {
                    channel_id: Some(channel),
                    ..Default::default()
                }),
                payload: Some(BcPayloads::BcXml(BcXml {
                    audio_noise: Some(noise),
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

    /// Convenience: set only the noise-reduction level for `channel`.
    pub async fn set_audio_noise_level(&self, channel: u8, level: u8) -> Result<()> {
        let mut noise = self.get_audio_noise(channel).await?;
        noise.level = level;
        // Setting a non-zero level implies enable.
        noise.enable = if level > 0 { 1 } else { 0 };
        self.set_audio_noise(noise).await
    }
}
