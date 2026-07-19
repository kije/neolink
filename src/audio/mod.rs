///
/// # Neolink Audio
///
/// Read and write the camera's audio configuration (cmd 264 / 265) and
/// noise-reduction settings (cmd 438 / 439).
///
/// # Usage
///
/// ```bash
/// # Print current audio config as XML:
/// neolink audio --config=config.toml CameraName cfg
///
/// # Set speaker / mic volume:
/// neolink audio --config=config.toml CameraName set-speaker 75
/// neolink audio --config=config.toml CameraName set-mic 60
///
/// # Noise reduction:
/// neolink audio --config=config.toml CameraName noise
/// neolink audio --config=config.toml CameraName set-noise 3
/// ```
///
use anyhow::{Context, Result};

mod cmdline;

use crate::common::NeoReactor;

pub(crate) use cmdline::{Action, Opt};

/// Entry point for the audio subcommand
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;
    let channel = opt.channel;
    let action = opt.action.unwrap_or(Action::Cfg);

    match action {
        Action::Cfg => {
            let cfg = camera
                .run_task(move |cam| {
                    Box::pin(async move {
                        cam.get_audio_cfg(channel)
                            .await
                            .context("Unable to get audio config")
                    })
                })
                .await?;
            print_xml(&cfg);
        }
        Action::SetSpeaker { volume } => {
            camera
                .run_task(move |cam| {
                    Box::pin(async move {
                        cam.set_speaker_volume(channel, volume)
                            .await
                            .context("Unable to set speaker volume")
                    })
                })
                .await?;
        }
        Action::SetMic { volume } => {
            camera
                .run_task(move |cam| {
                    Box::pin(async move {
                        cam.set_mic_volume(channel, volume)
                            .await
                            .context("Unable to set mic volume")
                    })
                })
                .await?;
        }
        Action::Noise => {
            let noise = camera
                .run_task(move |cam| {
                    Box::pin(async move {
                        cam.get_audio_noise(channel)
                            .await
                            .context("Unable to get audio noise config")
                    })
                })
                .await?;
            print_xml(&noise);
        }
        Action::SetNoise { level } => {
            camera
                .run_task(move |cam| {
                    Box::pin(async move {
                        cam.set_audio_noise_level(channel, level)
                            .await
                            .context("Unable to set audio noise level")
                    })
                })
                .await?;
        }
    }

    Ok(())
}

fn print_xml<T: serde::Serialize>(value: &T) {
    let ser = String::from_utf8({
        let mut buf = bytes::BytesMut::new();
        quick_xml::se::to_writer(&mut buf, value).expect("Should Ser the struct");
        buf.to_vec()
    })
    .expect("Should be UTF8");
    println!("{}", ser);
}
