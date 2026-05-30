use tokio::sync::mpsc::{channel, Receiver};

use super::connection::BcConnection;
use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};

/// Directions used for Ptz
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Direction {
    /// To move the camera Up
    Up,
    /// To move the camera Down
    Down,
    /// To move the camera Left
    Left,
    /// To move the camera Right
    Right,
    /// To move the camera Up and Left (diagonal)
    LeftUp,
    /// To move the camera Up and Right (diagonal)
    RightUp,
    /// To move the camera Down and Left (diagonal)
    LeftDown,
    /// To move the camera Down and Right (diagonal)
    RightDown,
    /// To stop currently active PTZ command
    Stop,
}

impl BcCamera {
    /// Send a PTZ message to the camera
    pub async fn send_ptz(&self, direction: Direction, amount: f32) -> Result<()> {
        self.has_ability_rw("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub_set = connection.subscribe(MSG_ID_PTZ_CONTROL, msg_num).await?;

        let direction_str = match direction {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::Left => "left",
            Direction::Right => "right",
            Direction::LeftUp => "leftUp",
            Direction::RightUp => "rightUp",
            Direction::LeftDown => "leftDown",
            Direction::RightDown => "rightDown",
            Direction::Stop => "stop",
        }
        .to_string();
        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_PTZ_CONTROL,
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
                    ptz_control: Some(PtzControl {
                        version: xml_ver(),
                        channel_id: self.channel_id,
                        speed: amount,
                        command: direction_str,
                    }),
                    ..Default::default()
                })),
            }),
        };

        sub_set.send(send).await?;
        let msg = sub_set.recv().await?;

        if let BcMeta {
            response_code: 200, ..
        } = msg.meta
        {
            Ok(())
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "The camera did not accept the PtzControl xml",
            })
        }
    }

    /// Get the [PtzPreset] XML which contains the list of the preset positions known to the camera
    pub async fn get_ptz_preset(&self) -> Result<PtzPreset> {
        self.has_ability_rw("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub_set = connection.subscribe(MSG_ID_GET_PTZ_PRESET, msg_num).await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_PTZ_PRESET,
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

        sub_set.send(send).await?;
        let msg = sub_set.recv().await?;

        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    ptz_preset: Some(ptz_preset),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(ptz_preset)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "The camera did not return a valid PtzPreset xml",
            })
        }
    }

    /// Set a PTZ preset.
    ///
    /// The current position will be saved as a preset with the given [preset_id] and [name]
    pub async fn set_ptz_preset(&self, preset_id: u8, name: String) -> Result<()> {
        self.has_ability_rw("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub_set = connection
            .subscribe(MSG_ID_PTZ_CONTROL_PRESET, msg_num)
            .await?;

        let preset = Preset {
            id: preset_id,
            name: Some(name),
            command: Some("setPos".to_owned()),
        };
        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_PTZ_CONTROL_PRESET,
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
                    ptz_preset: Some(PtzPreset {
                        preset_list: PresetList {
                            preset: vec![preset],
                        },
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
            }),
        };

        sub_set.send(send).await?;
        let msg = sub_set.recv().await?;

        if let BcMeta {
            response_code: 200, ..
        } = msg.meta
        {
            Ok(())
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "The camera did not accept the PtzPreset xml",
            })
        }
    }

    /// The camera will attempt to move to the preset with the given ID.
    pub async fn moveto_ptz_preset(&self, preset_id: u8) -> Result<()> {
        self.has_ability_rw("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub_set = connection
            .subscribe(MSG_ID_PTZ_CONTROL_PRESET, msg_num)
            .await?;

        let preset = Preset {
            id: preset_id,
            name: None,
            command: Some("toPos".to_owned()),
        };
        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_PTZ_CONTROL_PRESET,
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
                    ptz_preset: Some(PtzPreset {
                        preset_list: PresetList {
                            preset: vec![preset],
                        },
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
            }),
        };

        sub_set.send(send).await?;
        let msg = sub_set.recv().await?;

        if let BcMeta {
            response_code: 200, ..
        } = msg.meta
        {
            Ok(())
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "The camera did not accept the PtzPreset xml",
            })
        }
    }

    /// The camera will zoom to a given zoom amount.
    /// Not sure what the units for this are, seems to be 1000 is 1x and 2000 is 2x
    pub async fn zoom_to(&self, zoom_pos: u32) -> Result<()> {
        let current = self.get_zoom().await?;
        let zoom_pos = zoom_pos.clamp(current.zoom.min_pos, current.zoom.max_pos);

        self.has_ability_rw("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub_set = connection.subscribe(MSG_ID_SET_ZOOM_FOCUS, msg_num).await?;
        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_ZOOM_FOCUS,
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
                    start_zoom_focus: Some(StartZoomFocus {
                        version: xml_ver(),
                        channel_id: self.channel_id,
                        command: "zoomPos".to_string(),
                        move_pos: zoom_pos,
                    }),
                    ..Default::default()
                })),
            }),
        };

        sub_set.send(send).await?;

        let msg = sub_set.recv().await?;

        if let BcMeta {
            response_code: 200, ..
        } = msg.meta
        {
            Ok(())
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "The camera did not accept the StartZoomFocus xml",
            })
        }
    }

    /// Get the zoom xml, that has current min and max zoom values
    pub async fn get_zoom(&self) -> Result<PtzZoomFocus> {
        self.has_ability_ro("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub_get = connection.subscribe(MSG_ID_GET_ZOOM_FOCUS, msg_num).await?;
        let get = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_ZOOM_FOCUS,
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
                    ptz_zoom_focus: Some(xml),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(xml)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected PtzZoomFocus xml but it was not recieved",
            })
        }
    }

    /// Fetch the list of PTZ patrols (cruise tours) configured on the camera.
    pub async fn list_patrols(&self) -> Result<PatrolList> {
        self.has_ability_ro("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_PTZ_PATROL, msg_num).await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_PTZ_PATROL,
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
                    patrol_list: Some(list),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(list)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected PatrolList xml but it was not received",
            })
        }
    }

    /// Start a configured PTZ patrol by id.
    ///
    /// The patrol must already exist on the camera (use [`list_patrols`]
    /// to enumerate them). The camera will move through the patrol's
    /// preset stops until [`stop_patrol`] is called.
    ///
    /// [`list_patrols`]: BcCamera::list_patrols
    /// [`stop_patrol`]: BcCamera::stop_patrol
    pub async fn start_patrol(&self, patrol_id: u32) -> Result<()> {
        self.send_patrol_command(patrol_id, "start").await
    }

    /// Stop an in-progress PTZ patrol.
    pub async fn stop_patrol(&self, patrol_id: u32) -> Result<()> {
        self.send_patrol_command(patrol_id, "stop").await
    }

    async fn send_patrol_command(&self, patrol_id: u32, command: &str) -> Result<()> {
        self.has_ability_rw("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_PTZ_PATROL, msg_num).await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_PTZ_PATROL,
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
                    patrol_list: Some(PatrolList {
                        version: Some(xml_ver()),
                        channel_id: Some(self.channel_id),
                        patrol: vec![Patrol {
                            id: patrol_id,
                            name: None,
                            command: Some(command.to_string()),
                            preset_list: None,
                        }],
                    }),
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
                why: "The camera did not accept the PatrolList command",
            })
        }
    }

    /// Read the PTZ guard (return-to-home) configuration.
    pub async fn get_guard(&self) -> Result<PtzGuard> {
        self.has_ability_ro("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_GET_PTZ_GUARD, msg_num).await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_PTZ_GUARD,
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
                    ptz_guard: Some(guard),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(guard)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected PtzGuard xml but it was not received",
            })
        }
    }

    /// Set the PTZ guard (return-to-home) configuration.
    ///
    /// * `enable` — whether the guard behaviour is active.
    /// * `command` — one of `"setPos"` (save current position as guard),
    ///   `"toPos"` (move to guard), or `"delPos"` (delete configured guard).
    ///   Pass `None` to update only the enable / timeout fields.
    /// * `timeout` — inactivity seconds after which the camera auto-returns.
    /// * `need_set_pos` — `Some(1)` to refresh the saved position when
    ///   applying this config.
    pub async fn set_guard(
        &self,
        enable: bool,
        command: Option<&str>,
        timeout: u32,
        need_set_pos: Option<u8>,
    ) -> Result<()> {
        self.has_ability_rw("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_SET_PTZ_GUARD, msg_num).await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_PTZ_GUARD,
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
                    ptz_guard: Some(PtzGuard {
                        version: Some(xml_ver()),
                        channel_id: self.channel_id,
                        benable: u8::from(enable),
                        command: command.map(str::to_string),
                        timeout,
                        need_set_pos,
                    }),
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
                why: "The camera did not accept the PtzGuard xml",
            })
        }
    }

    /// Drive the camera back to its configured guard / return-to-home position.
    pub async fn goto_guard(&self) -> Result<()> {
        // Read the current config to preserve the enable/timeout fields, then
        // re-send it with the `toPos` command.
        let current = self.get_guard().await?;
        self.set_guard(current.benable != 0, Some("toPos"), current.timeout, None)
            .await
    }

    /// Send a 3D click-to-zoom request.
    ///
    /// The camera will pan, tilt and zoom so that the indicated screen
    /// coordinate becomes the new view centre.
    ///
    /// * `screen_x`, `screen_y` — pixel position of the click in the
    ///   displayed frame.
    /// * `frame_w`, `frame_h` — size of the bounding box (use `1, 1` for a
    ///   simple click, larger values for a drag-to-zoom rectangle).
    /// * `screen_w`, `screen_h` — total pixel size of the displayed video.
    /// * `speed` — movement speed in the camera's units (typical range 1-64).
    #[allow(clippy::too_many_arguments)]
    pub async fn ptz_3d_click(
        &self,
        screen_x: u32,
        screen_y: u32,
        frame_w: u32,
        frame_h: u32,
        screen_w: u32,
        screen_h: u32,
        speed: u32,
    ) -> Result<()> {
        self.has_ability_rw("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_PTZ_3D_LOCATION, msg_num)
            .await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_PTZ_3D_LOCATION,
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
                    ptz_3d_location: Some(Ptz3DLocation {
                        version: Some(xml_ver()),
                        channel_id: self.channel_id,
                        pos_x: screen_x,
                        pos_y: screen_y,
                        width: frame_w,
                        height: frame_h,
                        speed,
                        screen_width: screen_w,
                        screen_height: screen_h,
                    }),
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
                why: "The camera did not accept the Ptz3DLocation xml",
            })
        }
    }

    /// Read the camera's auto-focus enable state.
    ///
    /// Returns `true` when auto-focus is enabled.
    pub async fn get_auto_focus(&self) -> Result<bool> {
        self.has_ability_ro("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_GET_AUTO_FOCUS, msg_num).await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_AUTO_FOCUS,
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
                    auto_focus: Some(af),
                    ..
                })),
            ..
        }) = msg.body
        {
            // The wire field is named `disable` — invert.
            Ok(af.disable == 0)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected AutoFocus xml but it was not received",
            })
        }
    }

    /// Toggle the camera's auto-focus.
    ///
    /// `enable` of `true` turns auto-focus on; `false` switches the camera
    /// to manual focus.
    pub async fn set_auto_focus(&self, enable: bool) -> Result<()> {
        self.has_ability_rw("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_SET_AUTO_FOCUS, msg_num).await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_SET_AUTO_FOCUS,
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
                    auto_focus: Some(AutoFocus {
                        version: Some(xml_ver()),
                        channel_id: self.channel_id,
                        disable: u8::from(!enable),
                    }),
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
                why: "The camera did not accept the AutoFocus xml",
            })
        }
    }

    /// Poll the camera for its current pan/tilt position.
    ///
    /// Returns `(pan, tilt)` in the camera's native angular units (typically
    /// hundredths of a degree, signed).
    pub async fn get_ptz_position(&self) -> Result<(i32, i32)> {
        self.has_ability_ro("control").await?;
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_GET_PTZ_POSITION, msg_num)
            .await?;

        let send = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_PTZ_POSITION,
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
                    ptz_pos: Some(pos),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok((pos.p_pos, pos.t_pos))
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected PtzPos xml but it was not received",
            })
        }
    }

    /// Listen for pan/tilt position updates.
    ///
    /// Two kinds of messages drive this channel:
    ///
    /// - Direct camera pushes on [`MSG_ID_GET_PTZ_POSITION`] forwarded
    ///   verbatim, and
    /// - Camera "I'm moving" pushes on [`MSG_ID_PTZ_MOVING_STATUS`] that
    ///   trigger a follow-up poll on cmd 433 so we always emit a fresh
    ///   position to subscribers.
    ///
    /// Each [`PtzPos`] yielded carries the most recent reported pan/tilt
    /// for `self.channel_id`.
    pub async fn listen_on_ptz_position(&self) -> Result<Receiver<PtzPos>> {
        let (tx, rx) = channel(4);
        let connection = self.get_connection();
        let channel_id = self.channel_id;

        // Direct pushes on cmd 433.
        let tx_position = tx.clone();
        connection
            .handle_msg(MSG_ID_GET_PTZ_POSITION, move |bc| {
                let tx = tx_position.clone();
                Box::pin(async move {
                    if let Bc {
                        body:
                            BcBody::ModernMsg(ModernMsg {
                                payload:
                                    Some(BcPayloads::BcXml(BcXml {
                                        ptz_pos: Some(pos),
                                        ..
                                    })),
                                ..
                            }),
                        ..
                    } = bc
                    {
                        let _ = tx.send(pos.clone()).await;
                    }
                    None
                })
            })
            .await?;

        // Moving-status push: cmd 542 only carries a 0|1 flag, so we
        // synthesise a fresh poll of cmd 433 whenever it arrives. The poll
        // runs in a detached task on a cloned connection so we do not
        // block message dispatch.
        let conn_for_push = connection.clone();
        let push_msg_num = self.new_message_num();
        connection
            .handle_msg(MSG_ID_PTZ_MOVING_STATUS, move |_bc| {
                let conn = conn_for_push.clone();
                let tx = tx.clone();
                Box::pin(async move {
                    tokio::spawn(async move {
                        if let Ok(pos) =
                            poll_ptz_position(&conn, channel_id, push_msg_num).await
                        {
                            let _ = tx.send(pos).await;
                        }
                    });
                    None
                })
            })
            .await?;

        Ok(rx)
    }
}

/// Internal helper: send cmd 433 on an already-opened connection.
///
/// Used by [`BcCamera::listen_on_ptz_position`] from inside the
/// moving-status push handler, where calling back into `BcCamera` would
/// require taking out a borrow of state we have only as `Arc<BcConnection>`.
async fn poll_ptz_position(
    connection: &BcConnection,
    channel_id: u8,
    msg_num: u16,
) -> Result<PtzPos> {
    let mut sub = connection
        .subscribe(MSG_ID_GET_PTZ_POSITION, msg_num)
        .await?;

    let send = Bc {
        meta: BcMeta {
            msg_id: MSG_ID_GET_PTZ_POSITION,
            channel_id,
            msg_num,
            response_code: 0,
            stream_type: 0,
            class: 0x6414,
        },
        body: BcBody::ModernMsg(ModernMsg {
            extension: Some(Extension {
                channel_id: Some(channel_id),
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
                ptz_pos: Some(pos),
                ..
            })),
        ..
    }) = msg.body
    {
        Ok(pos)
    } else {
        Err(Error::UnintelligibleReply {
            reply: std::sync::Arc::new(Box::new(msg)),
            why: "Expected PtzPos xml but it was not received",
        })
    }
}
