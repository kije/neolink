//! Doorbell + wireless chime support over Baichuan.
//!
//! Implements the message family used by Reolink doorbells with paired wireless
//! chimes plus the hardwired-chime relay built into some models:
//!
//! - `MSG_ID_GET_DING_DONG_LIST` / `_ALT` (cmd 484/608) — list paired chimes
//! - `MSG_ID_DING_DONG_OPT` (cmd 485) — del / getParam / setParam / ringWithMusic
//! - `MSG_ID_GET_DING_DONG_CFG` / `_ALT` (cmd 486/606) — per-event ringtone map (read)
//! - `MSG_ID_SET_DING_DONG_CFG` / `_ALT` (cmd 487/607) — per-event ringtone map (write)
//! - `MSG_ID_GET_DING_DONG_SILENT` / `_SET_` (cmd 609/610) — silent windows
//! - `MSG_ID_DING_DONG_CTRL` / `_SET_` (cmd 482/483) — hardwired chime relay
//! - `MSG_ID_QUICK_REPLY_PLAY` (cmd 349) — quick-reply audio playback
//!
//! All commands are Baichuan-only and not exposed via HTTP/CGI.

use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};

// Sub-op identifiers for `MSG_ID_DING_DONG_OPT` (cmd 485)
const DINGDONG_OP_DEL: u32 = 1;
const DINGDONG_OP_GET_PARAM: u32 = 2;
const DINGDONG_OP_SET_PARAM: u32 = 3;
const DINGDONG_OP_RING_WITH_MUSIC: u32 = 4;

/// Named tone IDs documented by Home Assistant.
///
/// IDs match the order seen in the HA frontend; cameras may use a different
/// numbering and runtime confirmation via [`BcCamera::chime_param`] is recommended.
/// See `ToneId::id` for the integer value sent over the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneId {
    /// "citybird"
    Citybird,
    /// "originaltune"
    OriginalTune,
    /// "pianokey"
    PianoKey,
    /// "loop"
    Loop,
    /// "attraction"
    Attraction,
    /// "hophop"
    HopHop,
    /// "goodday"
    GoodDay,
    /// "operetta"
    Operetta,
    /// "moonlight"
    Moonlight,
    /// "waybackhome"
    WayBackHome,
}

impl ToneId {
    /// Map a [`ToneId`] to its on-camera numeric tone id.
    ///
    /// TODO(#10): IDs are inferred from Home Assistant's ordering and may need
    /// to be adjusted after testing against a real chime. Confirm via
    /// `BcCamera::chime_param(...)`.
    pub fn id(self) -> u32 {
        match self {
            ToneId::Citybird => 1,
            ToneId::OriginalTune => 2,
            ToneId::PianoKey => 3,
            ToneId::Loop => 4,
            ToneId::Attraction => 5,
            ToneId::HopHop => 6,
            ToneId::GoodDay => 7,
            ToneId::Operetta => 8,
            ToneId::Moonlight => 9,
            ToneId::WayBackHome => 10,
        }
    }

    /// The HA-canonical short name for the tone.
    pub fn name(self) -> &'static str {
        match self {
            ToneId::Citybird => "citybird",
            ToneId::OriginalTune => "originaltune",
            ToneId::PianoKey => "pianokey",
            ToneId::Loop => "loop",
            ToneId::Attraction => "attraction",
            ToneId::HopHop => "hophop",
            ToneId::GoodDay => "goodday",
            ToneId::Operetta => "operetta",
            ToneId::Moonlight => "moonlight",
            ToneId::WayBackHome => "waybackhome",
        }
    }

    /// Lookup a [`ToneId`] by HA name, case-insensitive.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "citybird" => Some(ToneId::Citybird),
            "originaltune" => Some(ToneId::OriginalTune),
            "pianokey" => Some(ToneId::PianoKey),
            "loop" => Some(ToneId::Loop),
            "attraction" => Some(ToneId::Attraction),
            "hophop" => Some(ToneId::HopHop),
            "goodday" => Some(ToneId::GoodDay),
            "operetta" => Some(ToneId::Operetta),
            "moonlight" => Some(ToneId::Moonlight),
            "waybackhome" => Some(ToneId::WayBackHome),
            _ => None,
        }
    }
}

/// Parameters reported for a single chime via cmd 485 sub-op 2 (getParam).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChimeParams {
    /// Chime device ID
    pub device_id: u32,
    /// Friendly name
    pub name: Option<String>,
    /// Volume level (1..3) when reported
    pub vol_level: Option<u32>,
    /// LED state (0/1) when reported
    pub led_state: Option<u32>,
    /// Last selected tone id, when reported
    pub music_id: Option<u32>,
}

/// A silent-window definition (returned by cmd 609, accepted by cmd 610).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SilentWindow {
    /// Weekday bitmask — bit 0 = Sunday, bit 1 = Monday, …, bit 6 = Saturday.
    /// The spec example value `63` (`0b0111111`) means Mon-Sat with Sunday off.
    pub weekday_mask: u32,
    /// Silent-window start, `HH:MM`
    pub start_time: String,
    /// Silent-window end, `HH:MM`
    pub end_time: String,
}

/// State of the hardwired-chime relay (cmd 482/483).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HardwiredChime {
    /// Whether the hardwired chime is enabled
    pub enabled: bool,
    /// Whether the setting should be persisted across reboots
    pub save: bool,
    /// Relay hold-time in seconds when the doorbell is pressed
    pub hold_time: u32,
}

impl BcCamera {
    /// List the wireless chimes paired with the doorbell.
    ///
    /// Tries the newer firmware encoding (cmd 484) first and falls back to the
    /// older one (cmd 608) on `CameraServiceUnavailable`.
    pub async fn list_chimes(&self) -> Result<Vec<DingDongDevice>> {
        match self.fetch_chime_list(MSG_ID_GET_DING_DONG_LIST).await {
            Ok(list) => Ok(list),
            Err(Error::CameraServiceUnavailable { .. }) => {
                self.fetch_chime_list(MSG_ID_GET_DING_DONG_LIST_ALT).await
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_chime_list(&self, msg_id: u32) -> Result<Vec<DingDongDevice>> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(msg_id, msg_num).await?;
        let get = Bc {
            meta: BcMeta {
                msg_id,
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
                    ding_dong_list: Some(list),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(list.devices)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected DingDongList payload but it was not received",
            })
        }
    }

    /// Get the per-event ringtone mapping for the chime identified by `id`.
    ///
    /// Tries the newer firmware encoding (cmd 486) first and falls back to the
    /// older one (cmd 606).
    pub async fn get_chime_cfg(&self, id: u32) -> Result<DingDongCfg> {
        match self.fetch_chime_cfg(MSG_ID_GET_DING_DONG_CFG, id).await {
            Ok(cfg) => Ok(cfg),
            Err(Error::CameraServiceUnavailable { .. }) => {
                self.fetch_chime_cfg(MSG_ID_GET_DING_DONG_CFG_ALT, id)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_chime_cfg(&self, msg_id: u32, id: u32) -> Result<DingDongCfg> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(msg_id, msg_num).await?;
        let get = Bc {
            meta: BcMeta {
                msg_id,
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
                    ding_dong_cfg: Some(DingDongCfg {
                        version: xml_ver(),
                        channel_id: self.channel_id,
                        device_id: id,
                        device_cfg: DingDongDeviceCfg::default(),
                    }),
                    ..Default::default()
                })),
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
                    ding_dong_cfg: Some(cfg),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(cfg)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected DingDongCfg payload but it was not received",
            })
        }
    }

    /// Set the per-event ringtone mapping for a chime.
    ///
    /// Tries the newer firmware encoding (cmd 487) first and falls back to the
    /// older one (cmd 607).
    pub async fn set_chime_cfg(&self, id: u32, cfg: DingDongCfg) -> Result<()> {
        let mut cfg = cfg;
        cfg.device_id = id;
        if cfg.version.is_empty() {
            cfg.version = xml_ver();
        }
        match self
            .push_chime_cfg(MSG_ID_SET_DING_DONG_CFG, cfg.clone())
            .await
        {
            Ok(()) => Ok(()),
            Err(Error::CameraServiceUnavailable { .. }) => {
                self.push_chime_cfg(MSG_ID_SET_DING_DONG_CFG_ALT, cfg).await
            }
            Err(e) => Err(e),
        }
    }

    async fn push_chime_cfg(&self, msg_id: u32, cfg: DingDongCfg) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(msg_id, msg_num).await?;
        let bc = Bc {
            meta: BcMeta {
                msg_id,
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
                    ding_dong_cfg: Some(cfg),
                    ..Default::default()
                })),
            }),
        };
        sub.send(bc).await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }
        Ok(())
    }

    /// Read the current parameters of a chime via cmd 485 sub-op 2 (getParam).
    pub async fn chime_param(&self, id: u32) -> Result<ChimeParams> {
        let reply = self
            .ding_dong_opt(DingDongDeviceOpt {
                version: xml_ver(),
                channel_id: self.channel_id,
                type_: DINGDONG_OP_GET_PARAM,
                device_id: id,
                ..Default::default()
            })
            .await?;

        Ok(ChimeParams {
            device_id: reply.device_id,
            name: reply.name,
            vol_level: reply.vol_level,
            led_state: reply.led_state,
            music_id: reply.music_id,
        })
    }

    /// Update a chime's parameters via cmd 485 sub-op 3 (setParam).
    ///
    /// Any field left as `None` is omitted from the request, so callers can
    /// update only the fields they care about.
    pub async fn set_chime_param(
        &self,
        id: u32,
        vol_level: Option<u32>,
        led_state: Option<u32>,
        name: Option<String>,
    ) -> Result<()> {
        let _ = self
            .ding_dong_opt(DingDongDeviceOpt {
                version: xml_ver(),
                channel_id: self.channel_id,
                type_: DINGDONG_OP_SET_PARAM,
                device_id: id,
                vol_level,
                led_state,
                name,
                music_id: None,
            })
            .await?;
        Ok(())
    }

    /// Unpair a chime via cmd 485 sub-op 1 (delDevice).
    pub async fn delete_chime(&self, id: u32) -> Result<()> {
        let _ = self
            .ding_dong_opt(DingDongDeviceOpt {
                version: xml_ver(),
                channel_id: self.channel_id,
                type_: DINGDONG_OP_DEL,
                device_id: id,
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Trigger a chime to play `tone_id` via cmd 485 sub-op 4 (ringWithMusic).
    pub async fn ring_chime(&self, id: u32, tone_id: u32) -> Result<()> {
        let _ = self
            .ding_dong_opt(DingDongDeviceOpt {
                version: xml_ver(),
                channel_id: self.channel_id,
                type_: DINGDONG_OP_RING_WITH_MUSIC,
                device_id: id,
                music_id: Some(tone_id),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Underlying cmd 485 transport — sends the supplied `DingDongDeviceOpt` and
    /// returns the camera's reply payload (for ops like getParam) or a default
    /// instance if the camera only acknowledges the message.
    async fn ding_dong_opt(&self, opt: DingDongDeviceOpt) -> Result<DingDongDeviceOpt> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_DING_DONG_OPT, msg_num).await?;
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_DING_DONG_OPT,
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
                    ding_dong_device_opt: Some(opt),
                    ..Default::default()
                })),
            }),
        };
        sub.send(bc).await?;
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
                    ding_dong_device_opt: Some(reply),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(reply)
        } else {
            // Some sub-ops (del / set / ring) just ack — synthesise a default
            Ok(DingDongDeviceOpt::default())
        }
    }

    /// Read the silent-window configuration (cmd 609) for the chime identified by `id`.
    pub async fn get_chime_silent(&self, id: u32) -> Result<SilentWindow> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_GET_DING_DONG_SILENT, msg_num)
            .await?;
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_DING_DONG_SILENT,
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
                    ding_dong_device_opt: Some(DingDongDeviceOpt {
                        version: xml_ver(),
                        channel_id: self.channel_id,
                        type_: DINGDONG_OP_GET_PARAM,
                        device_id: id,
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
            }),
        };
        sub.send(bc).await?;
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
                    ding_dong_silent_mode: Some(silent),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(SilentWindow {
                weekday_mask: silent.type_,
                start_time: silent.start_time,
                end_time: silent.end_time,
            })
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected DingDongSilentMode payload but it was not received",
            })
        }
    }

    /// Set the silent-window configuration (cmd 610) for the chime identified by `id`.
    pub async fn set_chime_silent(&self, _id: u32, window: SilentWindow) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_SET_DING_DONG_SILENT, msg_num)
            .await?;
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_DING_DONG_SILENT,
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
                    ding_dong_silent_mode: Some(DingDongSilentMode {
                        version: xml_ver(),
                        channel_id: self.channel_id,
                        type_: window.weekday_mask,
                        start_time: window.start_time,
                        end_time: window.end_time,
                    }),
                    ..Default::default()
                })),
            }),
        };
        sub.send(bc).await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }
        Ok(())
    }

    /// Read the hardwired chime relay state (cmd 483 — `machineStateGet`).
    pub async fn get_hardwired_chime(&self) -> Result<HardwiredChime> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_DING_DONG_CTRL, msg_num).await?;
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_DING_DONG_CTRL,
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
                    ding_dong_ctrl: Some(DingDongCtrl {
                        version: xml_ver(),
                        channel_id: self.channel_id,
                        type_: "machineStateGet".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
            }),
        };
        sub.send(bc).await?;
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
                    ding_dong_ctrl: Some(ctrl),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(HardwiredChime {
                enabled: ctrl.bopen.unwrap_or(0) != 0,
                save: ctrl.bsave.unwrap_or(0) != 0,
                hold_time: ctrl.time.unwrap_or(0),
            })
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected DingDongCtrl payload but it was not received",
            })
        }
    }

    /// Write the hardwired chime relay state (cmd 482 — `machineStateSet`).
    pub async fn set_hardwired_chime(&self, enable: bool, hold_time: u32) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_SET_DING_DONG_CTRL, msg_num)
            .await?;
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_DING_DONG_CTRL,
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
                    ding_dong_ctrl: Some(DingDongCtrl {
                        version: xml_ver(),
                        channel_id: self.channel_id,
                        type_: "machineStateSet".to_string(),
                        bopen: Some(if enable { 1 } else { 0 }),
                        bsave: Some(1),
                        time: Some(hold_time),
                    }),
                    ..Default::default()
                })),
            }),
        };
        sub.send(bc).await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }
        Ok(())
    }

    /// Play a pre-recorded quick-reply audio clip stored on the camera (cmd 349).
    pub async fn play_quick_reply(
        &self,
        file_id: u32,
        play_duration: u32,
        play_times: u32,
    ) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_QUICK_REPLY_PLAY, msg_num).await?;
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_QUICK_REPLY_PLAY,
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
                    audio_file_info: Some(AudioFileInfo {
                        version: xml_ver(),
                        channel_id: self.channel_id,
                        file_id,
                        play_mode: 1,
                        play_duration,
                        play_times,
                    }),
                    ..Default::default()
                })),
            }),
        };
        sub.send(bc).await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }
        Ok(())
    }
}
