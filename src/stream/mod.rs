//!
//! # Neolink Stream
//!
//! Writes one camera's stream to stdout, a file or a FIFO, so that another
//! program can consume it without neolink running a server.
//!
//! # Usage
//!
//! ```bash
//! neolink stream --config=config.toml CameraName > camera.ts
//! ```
//!
//! The reason this exists is go2rtc's `exec:` source, which runs a command
//! and reads its standard output:
//!
//! ```yaml
//! streams:
//!   front: exec:neolink stream --config=/etc/neolink.toml Front
//! ```
//!
//! That arrangement avoids most of what makes neolink's RTSP server
//! awkward in front of go2rtc, because the hard parts simply are not
//! there:
//!
//! * **No connect deadline.** go2rtc puts a five second limit on every
//!   RTSP request, which a battery camera waking up can miss. It waits on
//!   a pipe indefinitely.
//! * **No idle deadline.** An RTSP media connection is dropped after five
//!   seconds of silence. A pipe is not.
//! * **One camera session.** go2rtc starts the process when the first
//!   viewer arrives and stops it when the last one leaves, so the camera
//!   is connected on demand and exactly once — rather than once per RTSP
//!   client, which is what the unshared media factory does today.
//! * **A fixed track list.** The tracks are decided once, before any
//!   output, and cannot change under a viewer afterwards.
//!
//! Because the reader controls the process lifetime, the `pause` settings
//! in the config do not apply here: there is nothing to pause, as nothing
//! is running unless someone is watching.
//!
//! If neolink cannot run inside the reader's container — the usual reason
//! being that it is on the host and go2rtc is in Docker — see
//! [`crate::pipe`], which serves the same bytes over a FIFO or a socket
//! that can be bind-mounted instead.
//!
//! # Formats
//!
//! MPEG-TS by default, carrying the camera's H264 or H265 untouched plus
//! one audio track. AAC is passed through as it arrives, which is the form
//! go2rtc can put straight into MP4, HLS or a recording; ADPCM is decoded
//! and re-encoded as G.711 A-law, which is the one audio codec go2rtc can
//! give a WebRTC viewer without transcoding.
//!
//! `--format annexb` writes the video elementary stream and nothing else,
//! for piping into ffmpeg.

use anyhow::{Context, Result};
use log::*;
use neolink_core::bc_protocol::StreamKind;
use std::sync::Arc;
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub(crate) mod cmdline;
pub(crate) mod fanout;
pub(crate) mod pump;

use crate::common::NeoReactor;
pub(crate) use cmdline::Opt;
use cmdline::Output;
use fanout::{Consumer, Fanout};
use pump::{learn, Packer};

/// Entry point for the stream subcommand
///
/// Opt is the command line options
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;
    let name = opt.camera.clone();
    let kind: StreamKind = opt.stream.into();

    // Deliberately `stream` and not `stream_while_live`: see the module
    // docs on why pausing has no meaning for a pipe.
    let mut media = camera
        .stream(kind)
        .await
        .with_context(|| format!("Could not start streaming {name}"))?;

    let learned = learn(&mut media, opt.settings(), &name, kind).await?;
    let mut packer = Packer::new(&learned, opt.format)?;

    // Only open the output once there is something to write. For a FIFO
    // this matters: opening it blocks until a reader appears, and doing
    // that after the camera is known to be working keeps the failure
    // modes from tangling.
    let sink = open_output(&opt.output).await?;

    // Even with a single reader this goes through the fanout, because
    // that is what keeps a stalled one from turning into a backlog.
    let fanout = Arc::new(Fanout::new(opt.backlog()));
    let consumer = fanout.attach();
    let mut writer = tokio::spawn(async move {
        let result = write_consumer(&consumer, sink).await;
        (result, consumer.overruns())
    });

    for frame in &learned.backlog {
        if let Some(chunk) = packer.frame(frame) {
            fanout.push(&chunk);
        }
    }

    let result = loop {
        tokio::select! {
            frame = media.recv() => {
                let Some(frame) = frame else {
                    debug!("{name}::{kind}: camera stopped sending");
                    break Ok(());
                };
                if let Some(chunk) = packer.frame(&frame) {
                    fanout.push(&chunk);
                }
            }
            // The reader went away, so there is no point reading more.
            done = &mut writer => {
                let (result, overruns) = done.context("The writer task failed")?;
                report_overruns(&name, kind, overruns);
                break result;
            }
        }
    };
    fanout.close();

    // A reader that has gone away is the ordinary way for this to end,
    // not a failure worth a non-zero exit.
    match result {
        Err(e) if is_disconnect(&e) => {
            info!("{name}::{kind}: output closed, stopping");
            Ok(())
        }
        other => other,
    }
}

/// Write everything a consumer is given, until it ends or the sink does.
///
/// Flushing per chunk is what keeps latency down to the camera's own
/// frame interval; without it the last partial buffer would sit unwritten
/// until the next frame filled it.
pub(crate) async fn write_consumer<W: AsyncWrite + Unpin>(
    consumer: &Consumer,
    mut sink: W,
) -> Result<()> {
    while let Some(chunk) = consumer.next().await {
        sink.write_all(&chunk.bytes).await?;
        sink.flush().await?;
    }
    Ok(())
}

/// Say something if a reader fell behind, since silently skipping video
/// is the kind of thing an operator wants told about.
pub(crate) fn report_overruns(name: &str, kind: impl std::fmt::Display, overruns: u64) {
    if overruns > 0 {
        info!(
            "{name}::{kind}: the reader fell behind {overruns} time(s); each time its \
             backlog was dropped and it resumed at the next keyframe. Raise \
             --max-backlog to tolerate more, or find out why it is slow"
        );
    }
}

/// Whether an error is just the consumer having closed the pipe.
pub(crate) fn is_disconnect(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|e| {
        matches!(
            e.kind(),
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
        )
    })
}

/// Open the destination.
async fn open_output(output: &Output) -> Result<Box<dyn AsyncWrite + Unpin + Send>> {
    Ok(match output {
        Output::Stdout => Box::new(tokio::io::stdout()),
        Output::Path(path) => {
            // A FIFO must not be truncated — there is nothing to truncate
            // and POSIX leaves the combination undefined — while a plain
            // file very much should be, or a shorter run would leave the
            // tail of a longer one behind it.
            let fifo = is_fifo(path).await;
            debug!("Opening {path:?} for writing (fifo: {fifo})");
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(!fifo)
                .open(path)
                .await
                .with_context(|| format!("Could not open {path:?} for writing"))?;
            Box::new(file)
        }
    })
}

/// Whether `path` already exists and is a named pipe.
#[cfg(unix)]
pub(crate) async fn is_fifo(path: &std::path::Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|m| m.file_type().is_fifo())
}

/// Windows has no FIFOs to worry about.
#[cfg(not(unix))]
pub(crate) async fn is_fifo(_path: &std::path::Path) -> bool {
    false
}
