//! Baichuan scene mode / arming scenarios (cmd 603 list, 604 info, 605 set)
//!
//! Scene mode is the host-level arming scenario that drives which alarms
//! fire, which lights enable, etc. Reolink's mobile app exposes this as
//! "Scenarios". Conventional ids (stock firmware):
//!
//! - `0` → `off` / disabled
//! - `1` → `away`
//! - `2` → `home`
//! - `3` → `disarm`
//!
//! These are the *default* names; cameras can rename a scene, so callers
//! that want the camera-side label should look it up via [`BcCamera::get_scene_info`].
//!
//! See the XML payload docs on [`SceneCfg`], [`SceneModeCfg`] and [`SceneList`].

use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};

/// Convenience: the conventional scene id for "disarm / off".
pub const SCENE_ID_DISABLED: u8 = 0;

impl BcCamera {
    /// Get the list of available scene ids configured on the camera.
    ///
    /// The cmd 603 reply only carries ids — names need a follow-up
    /// [`BcCamera::get_scene_info`] per id.
    pub async fn get_scenes(&self) -> Result<Vec<u8>> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_GET_SCENE_LIST, msg_num).await?;

        let get = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_SCENE_LIST,
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
                    scene_list: Some(list),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(list.all_ids())
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected sceneList xml but it was not received",
            })
        }
    }

    /// Get the details (id + camera-side name) of a specific scene.
    ///
    /// Returns the parsed [`SceneCfg`] reply. The `name` field is the
    /// camera-configured label and may be absent on firmwares that
    /// don't expose it.
    pub async fn get_scene_info(&self, scene_id: u8) -> Result<SceneCfg> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_GET_SCENE_INFO, msg_num).await?;

        let req = SceneCfg {
            version: Some(xml_ver()),
            id: scene_id,
            name: None,
        };

        let get = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_SCENE_INFO,
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
                    scene_cfg: Some(req),
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
                    scene_cfg: Some(cfg),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(cfg)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected sceneCfg xml on scene-info reply but it was missing",
            })
        }
    }

    /// Activate a scene by id (cmd 605).
    ///
    /// Sends a `SceneModeCfg { enable = 1, curSceneId = scene_id }` payload.
    /// Pass `0` (or use [`BcCamera::disable_scene`]) to deactivate.
    pub async fn set_scene(&self, scene_id: u8) -> Result<()> {
        if scene_id == SCENE_ID_DISABLED {
            return self.disable_scene().await;
        }
        let cfg = SceneModeCfg {
            version: Some(xml_ver()),
            enable: 1,
            cur_scene_id: Some(scene_id),
        };
        self.send_scene_mode_cfg(cfg).await
    }

    /// Disable scene mode (cmd 605 with `enable = 0`).
    ///
    /// This is the explicit "off / disarm" path matching upstream's
    /// `disable_scene` semantics. After this call no host scenario is
    /// active; alarms / lights fall back to their per-feature config.
    pub async fn disable_scene(&self) -> Result<()> {
        let cfg = SceneModeCfg {
            version: Some(xml_ver()),
            enable: 0,
            cur_scene_id: None,
        };
        self.send_scene_mode_cfg(cfg).await
    }

    /// Internal: send a cmd-605 SceneModeCfg payload, tolerating a 500 ms
    /// reply silence as success (mirrors the floodlight / ledstate /
    /// privacy idiom).
    async fn send_scene_mode_cfg(&self, cfg: SceneModeCfg) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_SET_SCENE, msg_num).await?;

        let set = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_SCENE,
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
                    scene_mode_cfg: Some(cfg),
                    ..Default::default()
                })),
            }),
        };

        sub.send(set).await?;

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
                    why: "The camera did not accept the scene-mode set command",
                })
            }
        } else {
            Ok(())
        }
    }
}
