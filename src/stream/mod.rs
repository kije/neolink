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

use anyhow::{anyhow, Context, Result};
use log::*;
use neolink_core::{
    bc_protocol::StreamKind,
    bcmedia::model::{BcMedia, BcMediaAac, BcMediaAdpcm, BcMediaIframe, BcMediaPframe, VideoType},
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc::Receiver,
    time::{timeout, Duration, Instant},
};

mod cmdline;

use crate::audio::{adpcm, alaw, ADPCM_SAMPLE_RATE};
use crate::common::{NeoReactor, TimestampTracker};
use crate::mpegts::{StreamType, TrackId, TsMuxer};
pub(crate) use cmdline::Opt;
use cmdline::{Audio, Format, Output};

/// Starting capacity of the per-frame staging buffer.
///
/// One frame's packets are assembled here and written in a single call.
/// Sized so that a typical keyframe does not force a reallocation; it
/// grows if one does.
const WRITE_BUFFER: usize = 64 * 1024;

/// What the camera turned out to be sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CameraAudio {
    Aac,
    Adpcm,
}

/// The audio track we publish, once the camera's own format and the
/// user's request have been reconciled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioPlan {
    /// Pass the camera's AAC through untouched.
    PassAac,
    /// Decode ADPCM and re-encode it as A-law.
    AdpcmToAlaw,
    /// Publish no audio.
    Silent,
}

impl AudioPlan {
    /// The MPEG-TS stream type this plan publishes, if any.
    fn stream_type(self) -> Option<StreamType> {
        match self {
            AudioPlan::PassAac => Some(StreamType::Aac),
            AudioPlan::AdpcmToAlaw => Some(StreamType::Pcma),
            AudioPlan::Silent => None,
        }
    }
}

/// What the probe found before any output was written.
struct Learned {
    video: VideoType,
    plan: AudioPlan,
    /// Frames from the most recent keyframe onward, to be replayed.
    backlog: Vec<BcMedia>,
}

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

    let learned = learn(&mut media, &opt, &name, kind).await?;

    // Only open the output once there is something to write. For a FIFO
    // this matters: opening it blocks until a reader appears, and doing
    // that after the camera is known to be working keeps the failure
    // modes from tangling. There is no buffered writer around it because
    // each frame is already assembled in one buffer and written in a
    // single call.
    let mut sink = open_output(&opt.output).await?;

    let result = match opt.format {
        Format::MpegTs => pump_mpegts(&mut media, &mut sink, learned, &name, kind).await,
        Format::Annexb => pump_annexb(&mut media, &mut sink, learned).await,
    };

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

/// Whether an error is just the consumer having closed the pipe.
fn is_disconnect(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| matches!(e.kind(), std::io::ErrorKind::BrokenPipe))
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
async fn is_fifo(path: &std::path::Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|m| m.file_type().is_fifo())
}

/// Windows has no FIFOs to worry about.
#[cfg(not(unix))]
async fn is_fifo(_path: &std::path::Path) -> bool {
    false
}

/// Watch the stream until the track list can be settled.
///
/// Two things have to be known before a single byte is written. The video
/// codec, which cannot be guessed. And whether there is audio, because a
/// track list that gains or loses a track later is worse than one that
/// was never offered — go2rtc pins a viewer's session to the tracks it
/// saw at connect time.
///
/// The wait for video is unbounded: a battery camera can take a while to
/// wake, and there is no deadline to miss. The wait for audio is bounded
/// by `--audio-probe`, since a camera with its microphone off would
/// otherwise never let us start.
async fn learn(
    media: &mut Receiver<BcMedia>,
    opt: &Opt,
    name: &str,
    kind: StreamKind,
) -> Result<Learned> {
    let probe = Duration::from_secs_f32(opt.audio_probe.max(0.0));
    let want_audio = opt.audio != Audio::None && opt.format != Format::Annexb;

    let mut video = None;
    let mut audio = None;
    let mut backlog: Vec<BcMedia> = Vec::new();
    let mut started = None;

    loop {
        // Once there is a keyframe to start from, the only thing left to
        // wait for is audio, and only for as long as the probe allows.
        let settled = video.is_some() && (!want_audio || audio.is_some());
        if settled {
            break;
        }

        let frame = if video.is_some() {
            let elapsed = started.map(|s: Instant| s.elapsed()).unwrap_or_default();
            let Some(remaining) = probe.checked_sub(elapsed) else {
                debug!("{name}::{kind}: no audio within the probe window");
                break;
            };
            match timeout(remaining, media.recv()).await {
                Ok(frame) => frame,
                Err(_) => {
                    debug!("{name}::{kind}: no audio within the probe window");
                    break;
                }
            }
        } else {
            media.recv().await
        };

        let Some(frame) = frame else {
            return Err(anyhow!(
                "{name}::{kind}: camera stopped before it sent a usable frame"
            ));
        };

        match &frame {
            BcMedia::Iframe(BcMediaIframe { video_type, .. }) => {
                // The probe clock starts at the first keyframe, not at the
                // first frame of any kind. A camera joined mid-GOP sends
                // P-frames for up to a whole keyframe interval first, and
                // letting those run the clock down would spend the audio
                // window before there was anything to pair audio with.
                started.get_or_insert_with(Instant::now);
                video = Some(*video_type);
                // Start from the newest keyframe: everything before it is
                // undecodable to a consumer joining now, and the parameter
                // sets a decoder needs travel with it.
                backlog.clear();
                backlog.push(frame);
            }
            BcMedia::Pframe(BcMediaPframe { video_type, .. }) => {
                if video.is_some() {
                    video = Some(*video_type);
                    backlog.push(frame);
                }
            }
            BcMedia::Aac(_) => {
                audio.get_or_insert(CameraAudio::Aac);
                if !backlog.is_empty() {
                    backlog.push(frame);
                }
            }
            BcMedia::Adpcm(_) => {
                audio.get_or_insert(CameraAudio::Adpcm);
                if !backlog.is_empty() {
                    backlog.push(frame);
                }
            }
            BcMedia::InfoV1(_) | BcMedia::InfoV2(_) => {}
        }
    }

    let video =
        video.ok_or_else(|| anyhow!("{name}::{kind}: camera stopped before it sent a keyframe"))?;

    let plan = decide_audio(opt.audio, audio, name, kind);
    if matches!(video, VideoType::H265) && opt.format == Format::MpegTs {
        warn!(
            "{name}::{kind}: this stream is H265. Browsers are far pickier about it \
             than about H264 — no desktop Firefox at all, and WebRTC needs Chrome 136+ \
             or Safari 18+. Consider `--stream sub`, which is usually H264"
        );
    }

    if plan == AudioPlan::AdpcmToAlaw {
        info!(
            "{name}::{kind}: this camera's audio is A-law, which needs a go2rtc newer \
             than 1.9.14 — older ones read the stream as video only"
        );
    }

    info!(
        "{name}::{kind}: streaming {} with {}",
        match video {
            VideoType::H264 => "H264",
            VideoType::H265 => "H265",
        },
        match plan {
            AudioPlan::PassAac => "AAC audio",
            AudioPlan::AdpcmToAlaw => "A-law audio converted from ADPCM",
            AudioPlan::Silent => "no audio",
        }
    );

    Ok(Learned {
        video,
        plan,
        backlog,
    })
}

/// Reconcile what was asked for with what the camera actually sends.
fn decide_audio(
    requested: Audio,
    found: Option<CameraAudio>,
    name: &str,
    kind: StreamKind,
) -> AudioPlan {
    match (requested, found) {
        (Audio::None, _) => AudioPlan::Silent,
        (_, None) => AudioPlan::Silent,

        (Audio::Auto, Some(CameraAudio::Aac)) | (Audio::Aac, Some(CameraAudio::Aac)) => {
            AudioPlan::PassAac
        }
        (Audio::Auto, Some(CameraAudio::Adpcm)) | (Audio::Pcma, Some(CameraAudio::Adpcm)) => {
            AudioPlan::AdpcmToAlaw
        }

        (Audio::Aac, Some(CameraAudio::Adpcm)) => {
            warn!(
                "{name}::{kind}: `--audio aac` was asked for but this camera sends ADPCM, \
                 so there will be no audio. Use `--audio auto` to get it as A-law instead"
            );
            AudioPlan::Silent
        }
        (Audio::Pcma, Some(CameraAudio::Aac)) => {
            warn!(
                "{name}::{kind}: `--audio pcma` was asked for but this camera sends AAC, \
                 which neolink does not decode, so there will be no audio. Use \
                 `--audio auto` to pass the AAC through instead"
            );
            AudioPlan::Silent
        }
    }
}

/// Mux the stream as MPEG-TS until the camera or the reader stops.
async fn pump_mpegts<W: AsyncWrite + Unpin>(
    media: &mut Receiver<BcMedia>,
    sink: &mut W,
    learned: Learned,
    name: &str,
    kind: StreamKind,
) -> Result<()> {
    let video_type = match learned.video {
        VideoType::H264 => StreamType::H264,
        VideoType::H265 => StreamType::H265,
    };

    let mut kinds = vec![video_type];
    kinds.extend(learned.plan.stream_type());

    let mut muxer = TsMuxer::new(&kinds);
    let video_track = muxer
        .track(video_type)
        .ok_or_else(|| anyhow!("the muxer lost its video track"))?;
    let audio_track = learned.plan.stream_type().and_then(|t| muxer.track(t));

    let mut tracker = TimestampTracker::new();
    let mut out = Vec::with_capacity(WRITE_BUFFER);

    for frame in learned.backlog {
        mux_frame(
            &mut muxer,
            &mut tracker,
            video_track,
            audio_track,
            learned.plan,
            &frame,
            &mut out,
        );
    }
    flush(sink, &mut out).await?;

    while let Some(frame) = media.recv().await {
        mux_frame(
            &mut muxer,
            &mut tracker,
            video_track,
            audio_track,
            learned.plan,
            &frame,
            &mut out,
        );
        flush(sink, &mut out).await?;
    }

    debug!("{name}::{kind}: camera stopped sending");
    Ok(())
}

/// Mux one camera frame, appending TS packets to `out`.
fn mux_frame(
    muxer: &mut TsMuxer,
    tracker: &mut TimestampTracker,
    video_track: TrackId,
    audio_track: Option<TrackId>,
    plan: AudioPlan,
    frame: &BcMedia,
    out: &mut Vec<u8>,
) {
    match frame {
        BcMedia::Iframe(BcMediaIframe {
            microseconds, data, ..
        }) => {
            let pts = tracker.next_video_us(*microseconds);
            muxer.write_frame(video_track, pts, true, data, out);
        }
        BcMedia::Pframe(BcMediaPframe {
            microseconds, data, ..
        }) => {
            let pts = tracker.next_video_us(*microseconds);
            muxer.write_frame(video_track, pts, false, data, out);
        }
        BcMedia::Aac(aac) => {
            if plan != AudioPlan::PassAac {
                return;
            }
            let Some(track) = audio_track else { return };
            // The camera's frames are already ADTS, which is exactly what
            // an MPEG-TS AAC track carries, so there is nothing to do but
            // hand them over.
            let pts = tracker.next_audio_us(audio_duration(aac));
            muxer.write_frame(track, pts, false, &aac.data, out);
        }
        BcMedia::Adpcm(adpcm) => {
            if plan != AudioPlan::AdpcmToAlaw {
                return;
            }
            let Some(track) = audio_track else { return };
            let pts = tracker.next_audio_us(adpcm_duration(adpcm));
            match adpcm::decode(&adpcm.data) {
                Ok(samples) => {
                    let alaw = alaw::from_pcm(&samples);
                    muxer.write_frame(track, pts, false, &alaw, out);
                }
                Err(e) => debug!("Dropping an undecodable ADPCM frame: {e}"),
            }
        }
        BcMedia::InfoV1(_) | BcMedia::InfoV2(_) => {}
    }
}

/// How long an AAC frame lasts, in microseconds.
///
/// Read from the ADTS header. Audio frames carry no timestamp of their
/// own, so the clock is advanced by each frame's own duration.
fn audio_duration(aac: &BcMediaAac) -> u32 {
    aac.duration().unwrap_or_else(|| {
        // 1024 samples is the AAC frame size; 16kHz is what these cameras
        // use. Only reached if the header was unreadable, in which case
        // the frame is probably being dropped downstream anyway.
        trace!("An AAC frame had no readable ADTS header; assuming 16kHz");
        1_024 * 1_000_000 / 16_000
    })
}

/// How long an ADPCM block lasts, in microseconds.
fn adpcm_duration(adpcm: &BcMediaAdpcm) -> u32 {
    adpcm
        .duration()
        .unwrap_or_else(|| adpcm.block_size() * 2 * 1_000_000 / ADPCM_SAMPLE_RATE)
}

/// Write the camera's video elementary stream with nothing around it.
async fn pump_annexb<W: AsyncWrite + Unpin>(
    media: &mut Receiver<BcMedia>,
    sink: &mut W,
    learned: Learned,
) -> Result<()> {
    let mut out = Vec::with_capacity(WRITE_BUFFER);

    for frame in &learned.backlog {
        append_annexb(frame, &mut out);
    }
    flush(sink, &mut out).await?;

    while let Some(frame) = media.recv().await {
        append_annexb(&frame, &mut out);
        flush(sink, &mut out).await?;
    }
    Ok(())
}

fn append_annexb(frame: &BcMedia, out: &mut Vec<u8>) {
    match frame {
        BcMedia::Iframe(BcMediaIframe { data, .. })
        | BcMedia::Pframe(BcMediaPframe { data, .. }) => out.extend_from_slice(data),
        _ => {}
    }
}

/// Write everything buffered and clear it.
///
/// Flushing per frame is what keeps the consumer's latency down to the
/// camera's own frame interval; without it the last partial buffer sits
/// unwritten until the next frame fills it.
async fn flush<W: AsyncWrite + Unpin>(sink: &mut W, out: &mut Vec<u8>) -> Result<()> {
    if out.is_empty() {
        return Ok(());
    }
    sink.write_all(out).await?;
    sink.flush().await?;
    out.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use neolink_core::bcmedia::model::{BcMediaAac, BcMediaAdpcm, BcMediaIframe, BcMediaPframe};
    use tokio::sync::mpsc::channel;

    fn opt(audio: Audio, format: Format, probe: f32) -> Opt {
        Opt {
            camera: "Test".to_string(),
            output: cmdline::Output::Stdout,
            stream: cmdline::Stream::Main,
            format,
            audio,
            audio_probe: probe,
        }
    }

    fn iframe(us: u32) -> BcMedia {
        BcMedia::Iframe(BcMediaIframe {
            video_type: VideoType::H264,
            microseconds: us,
            time: None,
            data: vec![0, 0, 0, 1, 0x67, 0x42],
        })
    }

    fn pframe(us: u32) -> BcMedia {
        BcMedia::Pframe(BcMediaPframe {
            video_type: VideoType::H264,
            microseconds: us,
            data: vec![0, 0, 0, 1, 0x41, 0x9A],
        })
    }

    /// One ADTS-framed AAC frame: 16 kHz, mono, 1024 samples.
    fn aac() -> BcMedia {
        let len = 7 + 16;
        // Byte 2 is profile(2) | sampling_frequency_index(4) | private(1)
        // | the top bit of channel_config: AAC-LC, index 8 (16 kHz), and
        // a channel count of 1 whose low bits carry into byte 3.
        let mut data = vec![
            0xFF,
            0xF1,
            (0b01 << 6) | (8 << 2),
            0b0100_0000 | ((len >> 11) as u8 & 0x03),
            ((len >> 3) & 0xFF) as u8,
            (((len & 0x07) << 5) as u8) | 0x1F,
            0xFC,
        ];
        data.resize(len, 0);
        BcMedia::Aac(BcMediaAac { data })
    }

    fn adpcm() -> BcMedia {
        // Four bytes of predictor state, then a block of nibbles.
        BcMedia::Adpcm(BcMediaAdpcm {
            data: vec![0u8; 4 + 240],
        })
    }

    /// Feed `frames` through `learn` on a channel that then stays open,
    /// so only the probe timeout can end the wait.
    async fn learn_from(frames: Vec<BcMedia>, opt: &Opt) -> Result<(Learned, Receiver<BcMedia>)> {
        let (tx, mut rx) = channel(64);
        for frame in frames {
            tx.send(frame).await.unwrap();
        }
        let learned = learn(&mut rx, opt, "Test", StreamKind::Main).await?;
        // Keep the sender alive so `recv` blocks rather than returning None.
        drop(tx);
        Ok((learned, rx))
    }

    #[tokio::test]
    async fn output_starts_at_the_newest_keyframe() {
        // go2rtc reads the codec out of the first access unit it sees, so
        // anything before a keyframe has to be dropped rather than sent.
        // A second keyframe arriving while the audio probe is still
        // running supersedes the first: replaying the older one would put
        // a whole extra keyframe interval of catch-up in front of the
        // viewer for no benefit.
        let frames = vec![
            pframe(0),
            pframe(40_000),
            iframe(80_000),
            pframe(120_000),
            iframe(160_000),
            pframe(200_000),
        ];
        let opt = opt(Audio::Auto, Format::MpegTs, 0.2);
        let (learned, _rx) = learn_from(frames, &opt).await.unwrap();

        assert!(
            matches!(learned.backlog.first(), Some(BcMedia::Iframe(_))),
            "the backlog must open on a keyframe"
        );
        assert_eq!(
            learned.backlog.len(),
            2,
            "only the newest keyframe and what followed it should be kept"
        );
    }

    #[tokio::test]
    async fn with_audio_off_the_first_keyframe_is_enough() {
        // Nothing left to learn once the video codec is known, so this
        // must not sit on a second keyframe before starting.
        let opt = opt(Audio::None, Format::MpegTs, 30.0);
        let (learned, _rx) = learn_from(vec![iframe(0), pframe(40_000)], &opt)
            .await
            .unwrap();
        assert_eq!(learned.backlog.len(), 1);
    }

    #[tokio::test]
    async fn frames_before_the_first_keyframe_are_discarded() {
        let opt = opt(Audio::None, Format::MpegTs, 0.0);
        let (learned, _rx) = learn_from(vec![pframe(0), aac(), iframe(40_000)], &opt)
            .await
            .unwrap();
        assert_eq!(learned.backlog.len(), 1);
    }

    #[tokio::test]
    async fn an_aac_camera_gets_a_passthrough_track() {
        let opt = opt(Audio::Auto, Format::MpegTs, 5.0);
        let (learned, _rx) = learn_from(vec![iframe(0), aac()], &opt).await.unwrap();
        assert_eq!(learned.plan, AudioPlan::PassAac);
    }

    #[tokio::test]
    async fn an_adpcm_camera_gets_an_alaw_track() {
        let opt = opt(Audio::Auto, Format::MpegTs, 5.0);
        let (learned, _rx) = learn_from(vec![iframe(0), adpcm()], &opt).await.unwrap();
        assert_eq!(learned.plan, AudioPlan::AdpcmToAlaw);
    }

    #[tokio::test]
    async fn a_silent_camera_settles_on_no_audio_after_the_probe() {
        // The probe is the only thing that can end this wait, so a short
        // one keeps the test quick while still exercising the timeout.
        let opt = opt(Audio::Auto, Format::MpegTs, 0.2);
        let (learned, _rx) = learn_from(vec![iframe(0), pframe(40_000)], &opt)
            .await
            .unwrap();
        assert_eq!(learned.plan, AudioPlan::Silent);
    }

    #[tokio::test]
    async fn no_audio_means_no_waiting() {
        // With audio switched off there is nothing to probe for, so this
        // must return immediately despite a long probe setting.
        let opt = opt(Audio::None, Format::MpegTs, 30.0);
        let started = Instant::now();
        let (learned, _rx) = learn_from(vec![iframe(0)], &opt).await.unwrap();
        assert_eq!(learned.plan, AudioPlan::Silent);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn annexb_never_waits_for_audio_either() {
        let opt = opt(Audio::Auto, Format::Annexb, 30.0);
        let started = Instant::now();
        let (learned, _rx) = learn_from(vec![iframe(0)], &opt).await.unwrap();
        assert_eq!(learned.plan, AudioPlan::Silent);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_camera_that_stops_before_a_keyframe_is_an_error() {
        let (tx, mut rx) = channel(4);
        tx.send(pframe(0)).await.unwrap();
        drop(tx);
        let opt = opt(Audio::None, Format::MpegTs, 0.0);
        assert!(learn(&mut rx, &opt, "Test", StreamKind::Main)
            .await
            .is_err());
    }

    #[test]
    fn requested_audio_is_reconciled_with_what_the_camera_sends() {
        use CameraAudio::*;
        let cases = [
            (Audio::Auto, Some(Aac), AudioPlan::PassAac),
            (Audio::Auto, Some(Adpcm), AudioPlan::AdpcmToAlaw),
            (Audio::Auto, None, AudioPlan::Silent),
            (Audio::Aac, Some(Aac), AudioPlan::PassAac),
            // Asked for AAC from a camera that cannot give it.
            (Audio::Aac, Some(Adpcm), AudioPlan::Silent),
            (Audio::Pcma, Some(Adpcm), AudioPlan::AdpcmToAlaw),
            // Asked for A-law from an AAC camera; we do not decode AAC.
            (Audio::Pcma, Some(Aac), AudioPlan::Silent),
            (Audio::None, Some(Aac), AudioPlan::Silent),
            (Audio::None, Some(Adpcm), AudioPlan::Silent),
        ];
        for (requested, found, expected) in cases {
            assert_eq!(
                decide_audio(requested, found, "Test", StreamKind::Main),
                expected,
                "{requested:?} with {found:?}"
            );
        }
    }

    #[test]
    fn the_muxer_publishes_the_track_the_plan_asked_for() {
        assert_eq!(AudioPlan::PassAac.stream_type(), Some(StreamType::Aac));
        assert_eq!(AudioPlan::AdpcmToAlaw.stream_type(), Some(StreamType::Pcma));
        assert_eq!(AudioPlan::Silent.stream_type(), None);
    }

    #[test]
    fn an_aac_frames_duration_comes_from_its_adts_header() {
        let BcMedia::Aac(frame) = aac() else {
            unreachable!()
        };
        // 1024 samples at 16 kHz is 64 ms.
        assert_eq!(audio_duration(&frame), 64_000);
    }

    #[test]
    fn an_adpcm_blocks_duration_comes_from_its_size() {
        let BcMedia::Adpcm(frame) = adpcm() else {
            unreachable!()
        };
        // 240 bytes is 480 samples, and at 8 kHz that is 60 ms.
        assert_eq!(adpcm_duration(&frame), 60_000);
    }
}
