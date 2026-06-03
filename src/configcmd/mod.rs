//! # Neolink Config
//!
//! This module exposes the Baichuan configuration-surface commands:
//! encoder, ISP image controls, OSD overlays, privacy mask and day/night
//! threshold. The cmd-ids are 25/26/44/45/52/53/56/57/78/296/297.
//!
//! ## Usage
//!
//! ```bash
//! # Encoder
//! neolink config --config=config.toml CameraName encoder get
//! neolink config --config=config.toml CameraName encoder set --bitrate 4096
//!
//! # ISP image
//! neolink config --config=config.toml CameraName image get
//! neolink config --config=config.toml CameraName image set --brightness 128
//! neolink config --config=config.toml CameraName image isp
//!
//! # OSD
//! neolink config --config=config.toml CameraName osd get
//! neolink config --config=config.toml CameraName osd datetime --state on --position upperLeft
//! neolink config --config=config.toml CameraName osd name --state on --name "Front Door"
//!
//! # Privacy mask
//! neolink config --config=config.toml CameraName privacy-mask get
//! neolink config --config=config.toml CameraName privacy-mask enable on
//!
//! # Day/Night
//! neolink config --config=config.toml CameraName day-night get
//! neolink config --config=config.toml CameraName day-night set --mode auto --threshold 50
//! ```
//!
//! All `set`-style commands implement read-modify-write: the live XML is
//! re-fetched from the camera, only the requested fields are mutated, and
//! the result is sent back. Reolink firmwares are known to reject re-serialized
//! XML that drops or reorders fields, so this contract MUST be preserved by
//! anything that adds further fields here.
use anyhow::{Context, Result};

mod cmdline;

use crate::common::NeoReactor;
pub(crate) use cmdline::*;

fn print_xml<T: serde::Serialize>(value: &T) -> Result<()> {
    let mut buf = bytes::BytesMut::new();
    quick_xml::se::to_writer(&mut buf, value).context("Could not serialise XML for display")?;
    let ser = String::from_utf8(buf.to_vec()).context("Serialised XML was not UTF-8")?;
    println!("{}", ser);
    Ok(())
}

/// Entry point for the `config` subcommand.
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;

    match opt.cmd {
        ConfigCmd::Encoder { action } => match action {
            EncoderAction::Get => {
                let enc = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.get_enc()
                                .await
                                .context("Unable to get encoder configuration")
                        })
                    })
                    .await?;
                print_xml(&enc)?;
            }
            EncoderAction::Set {
                stream,
                bitrate,
                framerate,
                width,
                height,
                codec,
                encoder_type,
                profile,
                gop,
            } => {
                camera
                    .run_task(move |cam| {
                        let codec = codec.clone();
                        let encoder_type = encoder_type.clone();
                        let profile = profile.clone();
                        Box::pin(async move {
                            cam.update_enc(|enc| {
                                let slot = match stream {
                                    EncStreamSel::Main => &mut enc.main_stream,
                                    EncStreamSel::Sub => &mut enc.sub_stream,
                                    EncStreamSel::Third => &mut enc.third_stream,
                                };
                                let stream_block = slot.get_or_insert_with(Default::default);
                                if let Some(v) = bitrate {
                                    stream_block.bit_rate = Some(v);
                                }
                                if let Some(v) = framerate {
                                    stream_block.frame_rate = Some(v);
                                }
                                if let Some(v) = width {
                                    stream_block.width = Some(v);
                                }
                                if let Some(v) = height {
                                    stream_block.height = Some(v);
                                }
                                if let Some(v) = codec.clone() {
                                    stream_block.video_enc_type = Some(v);
                                }
                                if let Some(v) = encoder_type.clone() {
                                    stream_block.encoder_type = Some(v);
                                }
                                if let Some(v) = profile.clone() {
                                    stream_block.encoder_profile = Some(v);
                                }
                                if let Some(v) = gop {
                                    let g = stream_block.gop.get_or_insert_with(Default::default);
                                    g.cur = Some(v);
                                }
                            })
                            .await
                            .context("Unable to update encoder configuration")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
        },
        ConfigCmd::Image { action } => match action {
            ImageAction::Get => {
                let vi = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.get_video_input()
                                .await
                                .context("Unable to get image (VideoInput) configuration")
                        })
                    })
                    .await?;
                print_xml(&vi)?;
            }
            ImageAction::Isp => {
                let isp = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.get_isp().await.context("Unable to get ISP configuration")
                        })
                    })
                    .await?;
                print_xml(&isp)?;
            }
            ImageAction::Set {
                brightness,
                contrast,
                saturation,
                hue,
                sharpness,
            } => {
                camera
                    .run_task(move |cam| {
                        Box::pin(async move {
                            cam.update_video_input(|vi| {
                                if let Some(v) = brightness {
                                    vi.bright = Some(v);
                                }
                                if let Some(v) = contrast {
                                    vi.contrast = Some(v);
                                }
                                if let Some(v) = saturation {
                                    vi.saturation = Some(v);
                                }
                                if let Some(v) = hue {
                                    vi.hue = Some(v);
                                }
                                if let Some(v) = sharpness {
                                    vi.sharpen = Some(v);
                                }
                            })
                            .await
                            .context("Unable to update image (VideoInput) configuration")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
        },
        ConfigCmd::Osd { action } => match action {
            OsdAction::Get => {
                let dt = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.get_osd_datetime()
                                .await
                                .context("Unable to get OSD datetime")
                        })
                    })
                    .await?;
                print_xml(&dt)?;
                let name = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.get_osd_channel_name()
                                .await
                                .context("Unable to get OSD channel name")
                        })
                    })
                    .await?;
                print_xml(&name)?;
            }
            OsdAction::Datetime { state, position } => {
                camera
                    .run_task(move |cam| {
                        let position = position.clone();
                        Box::pin(async move {
                            cam.update_osd_datetime(|dt| {
                                if let Some(s) = state {
                                    dt.enable = Some(s.as_u32());
                                }
                                if let Some(p) = position.clone() {
                                    dt.position = Some(p);
                                }
                            })
                            .await
                            .context("Unable to update OSD datetime")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
            OsdAction::Name {
                state,
                name,
                position,
            } => {
                camera
                    .run_task(move |cam| {
                        let name = name.clone();
                        let position = position.clone();
                        Box::pin(async move {
                            cam.update_osd_channel_name(|cn| {
                                if let Some(s) = state {
                                    cn.enable = Some(s.as_u32());
                                }
                                if let Some(n) = name.clone() {
                                    cn.name = Some(n);
                                }
                                if let Some(p) = position.clone() {
                                    cn.position = Some(p);
                                }
                            })
                            .await
                            .context("Unable to update OSD channel name")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
        },
        ConfigCmd::PrivacyMask { action } => match action {
            PrivacyMaskAction::Get => {
                let shelter = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.get_privacy_mask()
                                .await
                                .context("Unable to get privacy mask")
                        })
                    })
                    .await?;
                print_xml(&shelter)?;
            }
            PrivacyMaskAction::Enable { state } => {
                camera
                    .run_task(move |cam| {
                        Box::pin(async move {
                            cam.update_privacy_mask(|shelter| {
                                shelter.enable = Some(state.as_u32());
                            })
                            .await
                            .context("Unable to update privacy mask")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
        },
        ConfigCmd::DayNight { action } => match action {
            DayNightAction::Get => {
                let dns = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.get_day_night_threshold()
                                .await
                                .context("Unable to get day/night threshold")
                        })
                    })
                    .await?;
                print_xml(&dns)?;
            }
            DayNightAction::Set { mode, threshold } => {
                camera
                    .run_task(move |cam| {
                        let mode = mode.clone();
                        Box::pin(async move {
                            cam.update_day_night_threshold(|dns| {
                                if let Some(m) = mode.clone() {
                                    dns.mode = Some(m);
                                }
                                if let Some(t) = threshold {
                                    dns.threshold = Some(t);
                                }
                            })
                            .await
                            .context("Unable to update day/night threshold")?;
                            Ok(())
                        })
                    })
                    .await?;
            }
        },
    }

    Ok(())
}
