//! Turning a camera's frames into ready-to-write chunks.
//!
//! Shared by both subcommands that write a stream out: `neolink stream`,
//! which does it once to a pipe, and `neolink pipe`, which does it for as
//! long as anyone is reading. Everything here is about the stream itself
//! rather than where it goes — what the camera is sending, and how to turn
//! each frame into bytes.

use anyhow::{anyhow, Result};
use log::*;
use neolink_core::{
    bc_protocol::StreamKind,
    bcmedia::model::{BcMedia, BcMediaAac, BcMediaAdpcm, BcMediaIframe, BcMediaPframe, VideoType},
};
use tokio::{
    sync::mpsc::Receiver,
    time::{timeout, Duration, Instant},
};

use super::cmdline::{Audio, Format};
use super::fanout::Chunk;
use crate::audio::{adpcm, alaw, ADPCM_SAMPLE_RATE};
use crate::common::TimestampTracker;
use crate::mpegts::{StreamType, TrackId, TsMuxer};

/// Starting capacity of the per-frame staging buffer.
///
/// One frame's packets are assembled here and handed on in a single
/// chunk. Sized so that a typical keyframe does not force a
/// reallocation; it grows if one does.
const STAGING_BUFFER: usize = 64 * 1024;

/// What the camera turned out to be sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CameraAudio {
    Aac,
    Adpcm,
}

/// The audio track we publish, once the camera's own format and the
/// user's request have been reconciled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioPlan {
    /// Pass the camera's AAC through untouched.
    PassAac,
    /// Decode ADPCM and re-encode it as A-law.
    AdpcmToAlaw,
    /// Publish no audio.
    Silent,
}

impl AudioPlan {
    /// The MPEG-TS stream type this plan publishes, if any.
    pub(crate) fn stream_type(self) -> Option<StreamType> {
        match self {
            AudioPlan::PassAac => Some(StreamType::Aac),
            AudioPlan::AdpcmToAlaw => Some(StreamType::Pcma),
            AudioPlan::Silent => None,
        }
    }
}

/// What the probe found before any output was written.
pub(crate) struct Learned {
    pub(crate) video: VideoType,
    pub(crate) plan: AudioPlan,
    /// Frames from the most recent keyframe onward, to be replayed.
    pub(crate) backlog: Vec<BcMedia>,
}

/// The parts of the command line that shape the stream itself.
///
/// Both subcommands offer these, so the pump takes them rather than
/// either subcommand's whole `Opt`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Settings {
    pub(crate) format: Format,
    pub(crate) audio: Audio,
    pub(crate) audio_probe: f32,
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
pub(crate) async fn learn(
    media: &mut Receiver<BcMedia>,
    settings: Settings,
    name: &str,
    kind: StreamKind,
) -> Result<Learned> {
    let probe = Duration::from_secs_f32(settings.audio_probe.max(0.0));
    let want_audio = settings.audio != Audio::None && settings.format != Format::Annexb;

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

    let plan = decide_audio(settings.audio, audio, name, kind);
    if matches!(video, VideoType::H265) && settings.format == Format::MpegTs {
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
pub(crate) fn decide_audio(
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

/// Turns camera frames into chunks of output.
///
/// Holds the muxer and the clock, so one of these is one continuous
/// stream: its MPEG-TS continuity counters and timestamps only make
/// sense in sequence. A reader that joins late is caught up by the
/// fanout waiting for the next [`Chunk::resync_point`], not by making a
/// second packer.
pub(crate) struct Packer {
    kind: PackerKind,
    tracker: TimestampTracker,
    staging: Vec<u8>,
}

enum PackerKind {
    Ts {
        muxer: TsMuxer,
        video: TrackId,
        audio: Option<TrackId>,
        plan: AudioPlan,
    },
    /// A bare video elementary stream, with nothing wrapped around it.
    Annexb,
}

impl Packer {
    pub(crate) fn new(learned: &Learned, format: Format) -> Result<Self> {
        let kind = match format {
            Format::Annexb => PackerKind::Annexb,
            Format::MpegTs => {
                let video_type = match learned.video {
                    VideoType::H264 => StreamType::H264,
                    VideoType::H265 => StreamType::H265,
                };
                let mut kinds = vec![video_type];
                kinds.extend(learned.plan.stream_type());

                let muxer = TsMuxer::new(&kinds);
                let video = muxer
                    .track(video_type)
                    .ok_or_else(|| anyhow!("the muxer lost its video track"))?;
                let audio = learned.plan.stream_type().and_then(|t| muxer.track(t));
                PackerKind::Ts {
                    muxer,
                    video,
                    audio,
                    plan: learned.plan,
                }
            }
        };

        Ok(Self {
            kind,
            tracker: TimestampTracker::new(),
            staging: Vec::with_capacity(STAGING_BUFFER),
        })
    }

    /// Pack one camera frame, or `None` if it produced no output —
    /// stream info, or audio on a stream that is not publishing any.
    pub(crate) fn frame(&mut self, frame: &BcMedia) -> Option<Chunk> {
        self.staging.clear();

        let (pts_us, resync) = match &mut self.kind {
            PackerKind::Annexb => match frame {
                BcMedia::Iframe(BcMediaIframe {
                    microseconds, data, ..
                }) => {
                    self.staging.extend_from_slice(data);
                    (self.tracker.next_video_us(*microseconds), true)
                }
                BcMedia::Pframe(BcMediaPframe {
                    microseconds, data, ..
                }) => {
                    self.staging.extend_from_slice(data);
                    (self.tracker.next_video_us(*microseconds), false)
                }
                _ => return None,
            },
            PackerKind::Ts {
                muxer,
                video,
                audio,
                plan,
            } => match frame {
                BcMedia::Iframe(BcMediaIframe {
                    microseconds, data, ..
                }) => {
                    let pts = self.tracker.next_video_us(*microseconds);
                    muxer.write_frame(*video, pts, true, data, &mut self.staging);
                    (pts, true)
                }
                BcMedia::Pframe(BcMediaPframe {
                    microseconds, data, ..
                }) => {
                    let pts = self.tracker.next_video_us(*microseconds);
                    muxer.write_frame(*video, pts, false, data, &mut self.staging);
                    (pts, false)
                }
                BcMedia::Aac(aac) => {
                    if *plan != AudioPlan::PassAac {
                        return None;
                    }
                    let track = (*audio)?;
                    // The camera's frames are already ADTS, which is
                    // exactly what an MPEG-TS AAC track carries, so
                    // there is nothing to do but hand them over.
                    let pts = self.tracker.next_audio_us(audio_duration(aac));
                    muxer.write_frame(track, pts, false, &aac.data, &mut self.staging);
                    (pts, false)
                }
                BcMedia::Adpcm(adpcm_frame) => {
                    if *plan != AudioPlan::AdpcmToAlaw {
                        return None;
                    }
                    let track = (*audio)?;
                    let pts = self.tracker.next_audio_us(adpcm_duration(adpcm_frame));
                    match adpcm::decode(&adpcm_frame.data) {
                        Ok(samples) => {
                            let alaw = alaw::from_pcm(&samples);
                            muxer.write_frame(track, pts, false, &alaw, &mut self.staging);
                        }
                        Err(e) => {
                            debug!("Dropping an undecodable ADPCM frame: {e}");
                            return None;
                        }
                    }
                    (pts, false)
                }
                BcMedia::InfoV1(_) | BcMedia::InfoV2(_) => return None,
            },
        };

        if self.staging.is_empty() {
            return None;
        }
        Some(Chunk::new(
            std::mem::replace(&mut self.staging, Vec::with_capacity(STAGING_BUFFER)),
            resync,
            pts_us,
        ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::channel;

    fn opt(audio: Audio, format: Format, probe: f32) -> Settings {
        Settings {
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
    async fn learn_from(
        frames: Vec<BcMedia>,
        opt: &Settings,
    ) -> Result<(Learned, Receiver<BcMedia>)> {
        let (tx, mut rx) = channel(64);
        for frame in frames {
            tx.send(frame).await.unwrap();
        }
        let learned = learn(&mut rx, *opt, "Test", StreamKind::Main).await?;
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
        assert!(learn(&mut rx, opt, "Test", StreamKind::Main).await.is_err());
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

    #[test]
    fn the_packer_emits_a_resync_point_only_on_keyframes() {
        // The fanout uses this flag to decide where a reader may start,
        // so it has to line up with where the muxer writes a PAT/PMT.
        let learned = Learned {
            video: VideoType::H264,
            plan: AudioPlan::Silent,
            backlog: Vec::new(),
        };
        let mut packer = Packer::new(&learned, Format::MpegTs).unwrap();

        let key = packer.frame(&iframe(0)).expect("a keyframe should pack");
        assert!(key.resync_point);
        assert!(!key.bytes.is_empty());

        let delta = packer
            .frame(&pframe(40_000))
            .expect("a P-frame should pack");
        assert!(!delta.resync_point);
    }

    #[test]
    fn the_packer_drops_audio_the_plan_did_not_ask_for() {
        let learned = Learned {
            video: VideoType::H264,
            plan: AudioPlan::Silent,
            backlog: Vec::new(),
        };
        let mut packer = Packer::new(&learned, Format::MpegTs).unwrap();
        packer.frame(&iframe(0));
        assert!(
            packer.frame(&aac()).is_none(),
            "a silent plan must not emit audio"
        );
    }

    #[test]
    fn annexb_packs_video_only() {
        let learned = Learned {
            video: VideoType::H264,
            plan: AudioPlan::Silent,
            backlog: Vec::new(),
        };
        let mut packer = Packer::new(&learned, Format::Annexb).unwrap();

        let key = packer.frame(&iframe(0)).expect("a keyframe should pack");
        assert!(key.resync_point);
        // Straight through, with nothing wrapped around it.
        assert_eq!(&key.bytes[..], &[0, 0, 0, 1, 0x67, 0x42]);
        assert!(packer.frame(&aac()).is_none());
    }

    #[test]
    fn packed_timestamps_follow_the_camera_clock() {
        let learned = Learned {
            video: VideoType::H264,
            plan: AudioPlan::Silent,
            backlog: Vec::new(),
        };
        let mut packer = Packer::new(&learned, Format::MpegTs).unwrap();

        assert_eq!(packer.frame(&iframe(1_000_000)).unwrap().pts_us, 0);
        assert_eq!(packer.frame(&pframe(1_040_000)).unwrap().pts_us, 40_000);
        assert_eq!(packer.frame(&pframe(1_080_000)).unwrap().pts_us, 80_000);
    }
}
