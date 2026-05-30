//! # Neolink Playback
//!
//! This module exposes the Baichuan recording-playback surface via the
//! command line. See the issue tracker for the underlying spec.
//!
//! ## Usage
//!
//! ```bash
//! # list recordings between two dates
//! neolink playback --config=config.toml list \
//!     --from 2024-11-26 --to 2024-11-26 CameraName
//!
//! # fetch a cover preview
//! neolink playback --config=config.toml cover \
//!     --file preview.jpg --time 1700000000 CameraName
//!
//! # download a clip by its file name (raw BcMedia bytes)
//! neolink playback --config=config.toml download \
//!     --name Mp4Record/2024-11-26/RecS01_..._M.mp4 \
//!     --file out.bcmedia CameraName
//! ```

use anyhow::{anyhow, Context, Result};
use neolink_core::{
    bc::xml::{FileInfo, PlaybackTime},
    bc_protocol::{PlaybackTimeRange, RecordingHandle},
    bcmedia::model::BcMedia,
};
use tokio::{fs::File, io::AsyncWriteExt};

mod cmdline;

use crate::common::NeoReactor;

pub(crate) use cmdline::{Action, Opt};

/// Entry point for the playback subcommand.
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    match opt.cmd {
        Action::List(args) => list(args, reactor).await,
        Action::Cover(args) => cover(args, reactor).await,
        Action::Download(args) => download(args, reactor).await,
    }
}

async fn list(args: cmdline::ListArgs, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&args.camera).await?;
    let (from_year, from_month, from_day) = parse_ymd(&args.from)?;
    let (to_year, to_month, to_day) = parse_ymd(&args.to)?;
    let time_range = PlaybackTimeRange::new(
        PlaybackTime::start_of_day(from_year, from_month, from_day),
        PlaybackTime::end_of_day(to_year, to_month, to_day),
    );
    let channel = args.channel;
    let stream = args.stream.clone();
    let record_types = args.record_types.clone();
    let alarm_types = args.alarm_types.clone();
    let use_alarm = alarm_types.is_some();

    let results: Vec<FileInfo> = camera
        .run_task(move |cam| {
            let stream = stream.clone();
            let record_types = record_types.clone();
            let alarm_types = alarm_types.clone();
            Box::pin(async move {
                let r = if use_alarm {
                    cam.find_alarm_videos(
                        channel,
                        time_range,
                        alarm_types.as_deref(),
                        Some(&stream),
                    )
                    .await
                    .context("Unable to find alarm videos")?
                } else {
                    cam.list_recordings(channel, time_range, record_types.as_deref(), Some(&stream))
                        .await
                        .context("Unable to list recordings")?
                };
                Ok(r)
            })
        })
        .await?;

    for info in &results {
        let start = info
            .start_time
            .map(format_playback_time)
            .unwrap_or_else(|| "??".to_string());
        let end = info
            .end_time
            .map(format_playback_time)
            .unwrap_or_else(|| "??".to_string());
        let size = info.file_size.map(|n| format!(" {n}B")).unwrap_or_default();
        let kind = info.record_type.as_deref().unwrap_or("?");
        println!(
            "{}  [{} - {}]  {}{}",
            info.file_name, start, end, kind, size
        );
    }
    if results.is_empty() {
        println!("(no recordings in range)");
    }
    Ok(())
}

async fn cover(args: cmdline::CoverArgs, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&args.camera).await?;
    let channel = args.channel;
    let stream = args.stream.clone();
    let time = args.time;
    let bytes = camera
        .run_task(move |cam| {
            let stream = stream.clone();
            Box::pin(async move {
                cam.cover_preview(channel, time, Some(&stream))
                    .await
                    .context("Unable to fetch cover preview")
            })
        })
        .await?;
    let mut file = File::create(&args.file)
        .await
        .with_context(|| format!("Unable to create {}", args.file.display()))?;
    file.write_all(&bytes).await?;
    println!("wrote {} bytes to {}", bytes.len(), args.file.display());
    Ok(())
}

async fn download(args: cmdline::DownloadArgs, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&args.camera).await?;
    let channel = args.channel;
    let stream = args.stream.clone();
    let name = args.name.clone();
    let out_path = args.file.clone();

    camera
        .run_task(move |cam| {
            let stream = stream.clone();
            let name = name.clone();
            let out_path = out_path.clone();
            Box::pin(async move {
                let handle = RecordingHandle {
                    name,
                    stream_type: stream,
                    channel_id: channel,
                    start: None,
                    end: None,
                };
                let mut replay = cam
                    .download(&handle)
                    .await
                    .context("Unable to begin download")?;
                let mut file = File::create(&out_path)
                    .await
                    .with_context(|| format!("Unable to create {}", out_path.display()))?;
                let mut total = 0usize;
                loop {
                    match replay.next().await {
                        Ok(media) => {
                            let bytes = bcmedia_payload(&media);
                            if !bytes.is_empty() {
                                file.write_all(bytes).await?;
                                total += bytes.len();
                            }
                        }
                        Err(neolink_core::Error::StreamFinished) => break,
                        Err(e) => return Err(anyhow!(e)),
                    }
                }
                file.flush().await?;
                println!(
                    "wrote {} bytes (raw BcMedia) to {}",
                    total,
                    out_path.display()
                );
                Ok(())
            })
        })
        .await?;
    Ok(())
}

fn parse_ymd(s: &str) -> Result<(u16, u8, u8)> {
    let mut parts = s.split('-');
    let y: u16 = parts
        .next()
        .ok_or_else(|| anyhow!("date {s} missing year"))?
        .parse()
        .with_context(|| format!("bad year in {s}"))?;
    let m: u8 = parts
        .next()
        .ok_or_else(|| anyhow!("date {s} missing month"))?
        .parse()
        .with_context(|| format!("bad month in {s}"))?;
    let d: u8 = parts
        .next()
        .ok_or_else(|| anyhow!("date {s} missing day"))?
        .parse()
        .with_context(|| format!("bad day in {s}"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(anyhow!("date {s} out of range"));
    }
    Ok((y, m, d))
}

fn format_playback_time(t: PlaybackTime) -> String {
    let year = t.year / 10000;
    let month = (t.year / 100) % 100;
    let day = t.year % 100;
    let hour = t.hour / 10000;
    let minute = (t.hour / 100) % 100;
    let second = t.hour % 100;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

fn bcmedia_payload(media: &BcMedia) -> &[u8] {
    match media {
        BcMedia::Iframe(f) => &f.data,
        BcMedia::Pframe(f) => &f.data,
        BcMedia::Aac(f) => &f.data,
        BcMedia::Adpcm(f) => &f.data,
        BcMedia::InfoV1(_) | BcMedia::InfoV2(_) => &[],
    }
}
