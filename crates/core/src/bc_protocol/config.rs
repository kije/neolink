//! Configuration-surface commands for the Baichuan protocol.
//!
//! This module groups the get/set helpers for the camera's stream-tuning,
//! ISP, OSD, privacy-mask and day/night controls. They follow the same
//! read-modify-write contract that nodelink-js uses with its
//! `applyXmlTagPatch`/`applyStreamPatch` helpers: real firmwares reject
//! re-serialized XML that drops or reorders fields they expect, so every
//! mutator first re-reads the live XML from the camera, mutates the parsed
//! tree in place and sends it back.
//!
//! The XML structs in `bc::xml` deliberately use `Option<_>` for every field
//! so that fields the camera sent but we do not understand (or that aren't
//! present on this firmware) survive a `BcXml::try_parse` ->
//! `BcXml::serialize` round-trip unchanged.
use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};

/// Build a standard `ModernMsg` header for a config get/set
fn meta(msg_id: u32, channel_id: u8, msg_num: u16) -> BcMeta {
    BcMeta {
        msg_id,
        channel_id,
        msg_num,
        response_code: 0,
        stream_type: 0,
        class: 0x6414,
    }
}

/// Build the standard `Extension` block carrying the channel id. Most config
/// commands require it -- the camera uses it to identify which channel the
/// payload applies to.
fn ext(channel_id: u8) -> Extension {
    Extension {
        channel_id: Some(channel_id),
        ..Default::default()
    }
}

impl BcCamera {
    // ------------------------------------------------------------------
    // Generic helpers
    // ------------------------------------------------------------------

    /// Send a get-style request and return the deserialised `BcXml` payload.
    async fn config_get(&self, msg_id: u32, include_ext: bool) -> Result<BcXml> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(msg_id, msg_num).await?;

        let req = Bc {
            meta: meta(msg_id, self.channel_id, msg_num),
            body: BcBody::ModernMsg(ModernMsg {
                extension: if include_ext { Some(ext(self.channel_id)) } else { None },
                payload: None,
            }),
        };

        sub.send(req).await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        if let BcBody::ModernMsg(ModernMsg {
            payload: Some(BcPayloads::BcXml(xml)),
            ..
        }) = msg.body
        {
            Ok(xml)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected XML payload from config get",
            })
        }
    }

    /// Send a set-style request with the given `BcXml` payload.
    async fn config_set(&self, msg_id: u32, payload: BcXml, include_ext: bool) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(msg_id, msg_num).await?;

        let req = Bc {
            meta: meta(msg_id, self.channel_id, msg_num),
            body: BcBody::ModernMsg(ModernMsg {
                extension: if include_ext { Some(ext(self.channel_id)) } else { None },
                payload: Some(BcPayloads::BcXml(payload)),
            }),
        };

        sub.send(req).await?;
        // Like `set_services`, some cameras return immediately and others do
        // not reply at all on success. Wait briefly for a reply but treat
        // silence as success.
        match tokio::time::timeout(tokio::time::Duration::from_millis(2000), sub.recv()).await {
            Ok(reply) => {
                let msg = reply?;
                if msg.meta.response_code != 200 {
                    Err(Error::CameraServiceUnavailable {
                        id: msg.meta.msg_id,
                        code: msg.meta.response_code,
                    })
                } else {
                    Ok(())
                }
            }
            Err(_) => Ok(()),
        }
    }

    // ------------------------------------------------------------------
    // Encoder (MSG_ID_GET_ENCODER / MSG_ID_SET_ENCODER)
    // ------------------------------------------------------------------

    /// Get the encoder configuration ([`Enc`]) for the current channel.
    pub async fn get_enc(&self) -> Result<Enc> {
        let mut bcxml = self.config_get(MSG_ID_GET_ENCODER, true).await?;
        if let Some(enc) = bcxml.enc.take() {
            Ok(enc)
        } else {
            Err(Error::UnintelligibleXml {
                reply: std::sync::Arc::new(Box::new(bcxml)),
                why: "Expected Enc xml but it was not received",
            })
        }
    }

    /// Send an [`Enc`] payload back to the camera. Prefer
    /// [`BcCamera::update_enc`] which encapsulates the mandatory
    /// read-modify-write pattern.
    pub async fn set_enc(&self, enc: Enc) -> Result<()> {
        self.config_set(
            MSG_ID_SET_ENCODER,
            BcXml {
                enc: Some(enc),
                ..Default::default()
            },
            true,
        )
        .await
    }

    /// Read-modify-write the encoder XML. The closure receives the parsed
    /// [`Enc`] block from the camera; any mutations performed inside the
    /// closure are sent back. Fields the closure does not touch are
    /// preserved bit-for-bit (within the limits of our struct coverage --
    /// see the round-trip tests in `bc::xml`).
    pub async fn update_enc<F>(&self, mutate: F) -> Result<Enc>
    where
        F: FnOnce(&mut Enc),
    {
        let mut enc = self.get_enc().await?;
        mutate(&mut enc);
        self.set_enc(enc.clone()).await?;
        Ok(enc)
    }

    // ------------------------------------------------------------------
    // Video input / ISP image controls
    // (MSG_ID_GET_VIDEO_INPUT / MSG_ID_SET_VIDEO_INPUT / MSG_ID_GET_ISP)
    // ------------------------------------------------------------------

    /// Get the [`VideoInput`] XML for the current channel.
    pub async fn get_video_input(&self) -> Result<VideoInput> {
        let bcxml = self.config_get(MSG_ID_GET_VIDEO_INPUT, true).await?;
        if let Some(vi) = bcxml.video_input.clone() {
            return Ok(vi);
        }
        // Some firmwares wrap VideoInput inside an Isp block.
        if let Some(isp) = bcxml.isp.as_ref().and_then(|i| i.video_input.clone()) {
            return Ok(isp);
        }
        Err(Error::UnintelligibleXml {
            reply: std::sync::Arc::new(Box::new(bcxml)),
            why: "Expected VideoInput xml but it was not received",
        })
    }

    /// Get the [`InputAdvanceCfg`] block (advanced ISP settings) for the
    /// current channel. Returns `None` when the camera doesn't surface a
    /// separate advanced block.
    pub async fn get_input_advance_cfg(&self) -> Result<Option<InputAdvanceCfg>> {
        let bcxml = self.config_get(MSG_ID_GET_VIDEO_INPUT, true).await?;
        if let Some(adv) = bcxml.input_advance_cfg.clone() {
            return Ok(Some(adv));
        }
        if let Some(adv) = bcxml
            .isp
            .as_ref()
            .and_then(|i| i.input_advance_cfg.clone())
        {
            return Ok(Some(adv));
        }
        Ok(None)
    }

    /// Get the [`Isp`] XML (newer firmwares only).
    pub async fn get_isp(&self) -> Result<Isp> {
        let mut bcxml = self.config_get(MSG_ID_GET_ISP, true).await?;
        if let Some(isp) = bcxml.isp.take() {
            Ok(isp)
        } else {
            Err(Error::UnintelligibleXml {
                reply: std::sync::Arc::new(Box::new(bcxml)),
                why: "Expected Isp xml but it was not received",
            })
        }
    }

    /// Send a [`VideoInput`] payload (only). Prefer [`BcCamera::update_video_input`].
    pub async fn set_video_input(&self, vi: VideoInput) -> Result<()> {
        self.config_set(
            MSG_ID_SET_VIDEO_INPUT,
            BcXml {
                video_input: Some(vi),
                ..Default::default()
            },
            true,
        )
        .await
    }

    /// Read-modify-write the VideoInput XML.
    pub async fn update_video_input<F>(&self, mutate: F) -> Result<VideoInput>
    where
        F: FnOnce(&mut VideoInput),
    {
        let mut vi = self.get_video_input().await?;
        mutate(&mut vi);
        self.set_video_input(vi.clone()).await?;
        Ok(vi)
    }

    /// Read-modify-write the InputAdvanceCfg XML. If the camera does not
    /// surface a separate advanced block we error out rather than
    /// fabricating one.
    pub async fn update_input_advance_cfg<F>(&self, mutate: F) -> Result<InputAdvanceCfg>
    where
        F: FnOnce(&mut InputAdvanceCfg),
    {
        let mut adv = self.get_input_advance_cfg().await?.ok_or(Error::Other(
            "Camera did not return an InputAdvanceCfg block; not supported on this firmware",
        ))?;
        mutate(&mut adv);
        self.config_set(
            MSG_ID_SET_VIDEO_INPUT,
            BcXml {
                input_advance_cfg: Some(adv.clone()),
                ..Default::default()
            },
            true,
        )
        .await?;
        Ok(adv)
    }

    // ------------------------------------------------------------------
    // OSD (MSG_ID_GET_OSD / MSG_ID_SET_OSD)
    // ------------------------------------------------------------------

    /// Get the [`OsdDatetime`] XML.
    pub async fn get_osd_datetime(&self) -> Result<OsdDatetime> {
        let mut bcxml = self.config_get(MSG_ID_GET_OSD, true).await?;
        if let Some(dt) = bcxml.osd_datetime.take() {
            Ok(dt)
        } else {
            Err(Error::UnintelligibleXml {
                reply: std::sync::Arc::new(Box::new(bcxml)),
                why: "Expected OsdDatetime xml but it was not received",
            })
        }
    }

    /// Get the [`OsdChannelName`] XML.
    pub async fn get_osd_channel_name(&self) -> Result<OsdChannelName> {
        let mut bcxml = self.config_get(MSG_ID_GET_OSD, true).await?;
        if let Some(name) = bcxml.osd_channel_name.take() {
            Ok(name)
        } else {
            Err(Error::UnintelligibleXml {
                reply: std::sync::Arc::new(Box::new(bcxml)),
                why: "Expected OsdChannelName xml but it was not received",
            })
        }
    }

    /// Read-modify-write the OSD datetime overlay. Always re-reads the full
    /// OSD block from the camera and sends back both `OsdDatetime` and
    /// `OsdChannelName` so the camera does not drop one half of the OSD
    /// because the other half was missing.
    pub async fn update_osd_datetime<F>(&self, mutate: F) -> Result<OsdDatetime>
    where
        F: FnOnce(&mut OsdDatetime),
    {
        let mut bcxml = self.config_get(MSG_ID_GET_OSD, true).await?;
        let mut dt = bcxml.osd_datetime.clone().unwrap_or_default();
        mutate(&mut dt);
        bcxml.osd_datetime = Some(dt.clone());
        self.config_set(MSG_ID_SET_OSD, bcxml, true).await?;
        Ok(dt)
    }

    /// Read-modify-write the OSD channel-name overlay. See
    /// [`BcCamera::update_osd_datetime`] for the rationale behind sending
    /// both halves of the OSD XML.
    pub async fn update_osd_channel_name<F>(&self, mutate: F) -> Result<OsdChannelName>
    where
        F: FnOnce(&mut OsdChannelName),
    {
        let mut bcxml = self.config_get(MSG_ID_GET_OSD, true).await?;
        let mut name = bcxml.osd_channel_name.clone().unwrap_or_default();
        mutate(&mut name);
        bcxml.osd_channel_name = Some(name.clone());
        self.config_set(MSG_ID_SET_OSD, bcxml, true).await?;
        Ok(name)
    }

    // ------------------------------------------------------------------
    // Privacy mask (MSG_ID_GET_PRIVACY_MASK / MSG_ID_SET_PRIVACY_MASK)
    // ------------------------------------------------------------------

    /// Get the [`Shelter`] (privacy mask) XML.
    pub async fn get_privacy_mask(&self) -> Result<Shelter> {
        let mut bcxml = self.config_get(MSG_ID_GET_PRIVACY_MASK, true).await?;
        if let Some(shelter) = bcxml.shelter.take() {
            Ok(shelter)
        } else {
            Err(Error::UnintelligibleXml {
                reply: std::sync::Arc::new(Box::new(bcxml)),
                why: "Expected Shelter xml but it was not received",
            })
        }
    }

    /// Send a [`Shelter`] payload. Prefer [`BcCamera::update_privacy_mask`].
    pub async fn set_privacy_mask(&self, shelter: Shelter) -> Result<()> {
        self.config_set(
            MSG_ID_SET_PRIVACY_MASK,
            BcXml {
                shelter: Some(shelter),
                ..Default::default()
            },
            true,
        )
        .await
    }

    /// Read-modify-write the privacy mask.
    pub async fn update_privacy_mask<F>(&self, mutate: F) -> Result<Shelter>
    where
        F: FnOnce(&mut Shelter),
    {
        let mut shelter = self.get_privacy_mask().await?;
        mutate(&mut shelter);
        self.set_privacy_mask(shelter.clone()).await?;
        Ok(shelter)
    }

    // ------------------------------------------------------------------
    // Day/Night threshold (MSG_ID_GET_DAY_NIGHT_THRESHOLD /
    // MSG_ID_SET_DAY_NIGHT_THRESHOLD)
    // ------------------------------------------------------------------

    /// Get the [`DayNightSwitch`] XML.
    pub async fn get_day_night_threshold(&self) -> Result<DayNightSwitch> {
        let mut bcxml = self.config_get(MSG_ID_GET_DAY_NIGHT_THRESHOLD, true).await?;
        if let Some(dns) = bcxml.day_night_switch.take() {
            Ok(dns)
        } else {
            Err(Error::UnintelligibleXml {
                reply: std::sync::Arc::new(Box::new(bcxml)),
                why: "Expected DayNightSwitch xml but it was not received",
            })
        }
    }

    /// Send a [`DayNightSwitch`] payload. Prefer
    /// [`BcCamera::update_day_night_threshold`].
    pub async fn set_day_night_threshold(&self, dns: DayNightSwitch) -> Result<()> {
        self.config_set(
            MSG_ID_SET_DAY_NIGHT_THRESHOLD,
            BcXml {
                day_night_switch: Some(dns),
                ..Default::default()
            },
            true,
        )
        .await
    }

    /// Read-modify-write the day/night switch / threshold XML.
    pub async fn update_day_night_threshold<F>(&self, mutate: F) -> Result<DayNightSwitch>
    where
        F: FnOnce(&mut DayNightSwitch),
    {
        let mut dns = self.get_day_night_threshold().await?;
        mutate(&mut dns);
        self.set_day_night_threshold(dns.clone()).await?;
        Ok(dns)
    }
}
