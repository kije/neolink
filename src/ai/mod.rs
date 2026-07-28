///
/// # Neolink AI
///
/// This module handles the second generation Reolink AI detection surface:
/// the smart-AI detection zones, the per-AI-type alarm configuration, baby-cry
/// detection, and watching detections live.
///
/// # Usage
///
/// ```bash
/// # Dump the intrusion zones
/// neolink ai --config=config.toml CameraName zones intrusion
///
/// # Read then change the people detection sensitivity
/// neolink ai --config=config.toml CameraName alarm people
/// neolink ai --config=config.toml CameraName alarm people --sensitivity 60
///
/// # Read and set the baby cry sensitivity
/// neolink ai --config=config.toml CameraName cry
/// neolink ai --config=config.toml CameraName cry 50
///
/// # Follow detections as they happen
/// neolink ai --config=config.toml CameraName watch
/// ```
///
use anyhow::{Context, Result};
use neolink_core::bc_protocol::{SmartAiKind, SmartAiPayload, SMART_AI_KINDS};

mod cmdline;

use crate::common::NeoReactor;

pub(crate) use cmdline::Opt;
use cmdline::{AiCommand, SmartKind};

/// Entry point for the ai subcommand
///
/// Opt is the command line options
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;

    match opt.cmd {
        AiCommand::Zones { kind } => {
            let kinds: Vec<SmartAiKind> = match kind {
                Some(kind) => vec![SmartKind::into(kind)],
                None => SMART_AI_KINDS.to_vec(),
            };
            for kind in kinds {
                let payload = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.get_smart_ai(kind)
                                .await
                                .with_context(|| format!("Unable to get the {kind} zones"))
                        })
                    })
                    .await;
                match payload {
                    // A camera that doesn't have this detector answers with a
                    // service error; report it and carry on with the others.
                    Err(e) => println!("<!-- {kind}: {e} -->"),
                    Ok(payload) => print_zones(&payload)?,
                }
            }
        }
        AiCommand::Alarm {
            ai_type,
            sensitivity,
            stay_time,
        } => {
            let mut cfg = camera
                .run_task(|cam| {
                    let ai_type = ai_type.clone();
                    Box::pin(async move {
                        cam.get_ai_alarm(&ai_type)
                            .await
                            .context("Unable to get the AI alarm config")
                    })
                })
                .await?;
            if sensitivity.is_some() || stay_time.is_some() {
                // Read/modify/write so the fields we're not changing survive.
                if let Some(sensitivity) = sensitivity {
                    cfg.sensitivity = Some(sensitivity);
                }
                if let Some(stay_time) = stay_time {
                    cfg.stay_time = Some(stay_time);
                }
                let new_cfg = cfg.clone();
                camera
                    .run_task(move |cam| {
                        let new_cfg = new_cfg.clone();
                        Box::pin(async move {
                            cam.set_ai_alarm(new_cfg)
                                .await
                                .context("Unable to set the AI alarm config")
                        })
                    })
                    .await?;
            }
            print_xml(&cfg)?;
        }
        AiCommand::Cry { level } => match level {
            Some(level) => {
                camera
                    .run_task(move |cam| {
                        Box::pin(async move {
                            cam.set_cry_detection(level)
                                .await
                                .context("Unable to set the cry detection sensitivity")
                        })
                    })
                    .await?;
                println!("Cry detection sensitivity set to {level}");
            }
            None => {
                let level = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.get_cry_detection()
                                .await
                                .context("Unable to get the cry detection sensitivity")
                        })
                    })
                    .await?;
                match level {
                    Some(level) => println!("Cry detection sensitivity: {level}"),
                    None => println!("This camera does not support cry detection"),
                }
            }
        },
        AiCommand::Cfg => {
            let cfg = camera
                .run_task(|cam| {
                    Box::pin(async move { cam.get_ai_cfg().await.context("Unable to get AiCfg") })
                })
                .await?;
            print_xml(&cfg)?;
        }
        AiCommand::Watch { yolo } => {
            let watch_camera = camera.clone();
            tokio::select! {
                v = async {
                    // Detections from the ordinary alarm path. This is what the
                    // MQTT and ONVIF bridges consume.
                    let mut ai = watch_camera.ai().await?;
                    loop {
                        let state = ai.borrow_and_update().clone();
                        let detected = state.detected_types().collect::<Vec<_>>();
                        println!(
                            "detected: [{}] zones: {:?}",
                            detected.join(", "),
                            state.zones()
                        );
                        ai.changed().await.context("AI watch dropped")?;
                    }
                    #[allow(unreachable_code)]
                    Result::<()>::Ok(())
                } => v,
                v = async {
                    camera.run_task(|cam| {
                        Box::pin(async move {
                            let mut push = cam
                                .listen_on_smart_ai()
                                .await
                                .context("Unable to listen for YOLO push events")?;
                            loop {
                                let event = push.next_event().await?;
                                println!(
                                    "yolo: channel={} detailed={} type={} sub_types={:?}",
                                    event.channel_id(),
                                    event.detailed,
                                    event.ai_type(),
                                    event.sub_types(),
                                );
                            }
                            #[allow(unreachable_code)]
                            Result::<()>::Ok(())
                        })
                    }).await
                }, if yolo => v,
            }?;
        }
    }

    Ok(())
}

/// Print the zone container of whichever detector replied
fn print_zones(payload: &SmartAiPayload) -> Result<()> {
    match payload {
        SmartAiPayload::Crossline(v) => print_xml(v),
        SmartAiPayload::Intrusion(v) => print_xml(v),
        SmartAiPayload::Loitering(v) => print_xml(v),
        SmartAiPayload::Legacy(v) => print_xml(v),
        SmartAiPayload::Loss(v) => print_xml(v),
    }
}

/// Serialize a payload to XML on stdout, matching how the other subcommands
/// dump camera structures.
fn print_xml<T: serde::Serialize>(value: &T) -> Result<()> {
    let ser = String::from_utf8({
        let mut buf = bytes::BytesMut::new();
        quick_xml::se::to_writer(&mut buf, value).context("Could not serialise the reply")?;
        buf.to_vec()
    })
    .context("Reply was not UTF8")?;
    println!("{}", ser);
    Ok(())
}
