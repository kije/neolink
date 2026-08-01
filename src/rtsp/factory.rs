use gstreamer::ClockTime;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use gstreamer::{
    prelude::*, Bin, Caps, Element, ElementFactory, FlowError, GhostPad, Pipeline, State,
};
use gstreamer_app::{AppLeakyType, AppSrc, AppSrcCallbacks, AppStreamType};
use neolink_core::{
    bc_protocol::StreamKind,
    bcmedia::model::{
        BcMedia, BcMediaAac, BcMediaIframe, BcMediaInfoV1, BcMediaInfoV2, BcMediaPframe, VideoType,
    },
};
use tokio::{sync::mpsc::channel as mpsc, task::JoinHandle};

use crate::{
    common::NeoInstance,
    config::AudioFormat,
    rtsp::{gst::NeoMediaFactory, timestamps::TimestampTracker},
    AnyResult,
};

#[derive(Clone, Debug)]
pub enum AudioType {
    Aac(AacFraming),
    Adpcm(u32),
}

/// What the camera's AAC frames actually look like on the wire.
///
/// Learned from the first AAC frame. Telling the appsrc exactly what it is
/// producing beats making `aacparse` typefind it, and it has to be *right*:
/// `aacparse` will not convert framing when the declared MPEG version
/// contradicts the one in the ADTS header, it just refuses to negotiate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacFraming {
    /// Whether the frames carry ADTS framing, which every camera seen so
    /// far does.
    adts: bool,
    /// MPEG version from the ADTS `ID` bit: 4 for MPEG-4 AAC, 2 for
    /// MPEG-2 AAC. `None` when there is no ADTS header to read it from.
    mpegversion: Option<i32>,
}

impl AacFraming {
    /// Read the framing out of a camera AAC frame.
    ///
    /// `duration()` returns `None` unless the ADTS syncword is there, so it
    /// doubles as the ADTS probe; past it, byte 1 bit 3 is the `ID` field,
    /// set for MPEG-2 and clear for MPEG-4.
    fn from_frame(aac: &BcMediaAac) -> Self {
        if aac.duration().is_none() {
            return Self {
                adts: false,
                mpegversion: None,
            };
        }
        Self {
            adts: true,
            mpegversion: Some(if aac.data[1] & 0b0000_1000 != 0 { 2 } else { 4 }),
        }
    }

    /// The caps to declare on the audio appsrc, if we know enough to
    /// declare any.
    fn caps(&self) -> Option<Caps> {
        if !self.adts {
            return None;
        }
        let mut caps = Caps::builder("audio/mpeg").field("stream-format", "adts");
        if let Some(mpegversion) = self.mpegversion {
            caps = caps.field("mpegversion", mpegversion);
        }
        Some(caps.build())
    }

    /// Why this framing cannot be passed through at all, if it cannot.
    ///
    /// Both passthrough payloaders take `mpegversion=4` in `raw` framing,
    /// and `aacparse` can only produce that from a stream whose framing it
    /// has agreed on. Anything else has to be decoded to L16 instead.
    fn passthrough_blocker(&self) -> Option<&'static str> {
        match (self.adts, self.mpegversion) {
            (true, Some(4)) => None,
            (true, Some(_)) => Some(
                "the camera sends MPEG-2 AAC and the RTP payloaders only take MPEG-4 AAC",
            ),
            _ => Some("the camera's AAC frames are not ADTS framed, so `aacparse` cannot be relied on to unpack them"),
        }
    }
}

/// An RTP payload format that carries the camera's AAC frames untouched.
///
/// The two differ only in how RTP frames the same bytes: both take the
/// `raw` AAC that [`pipe_aac_raw_tail`] produces, and neither decodes
/// anything. Which one a client can use is the whole question — notably
/// go2rtc recognises AAC only as `MPEG4-GENERIC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AacPayload {
    /// RFC 3640 `mode=AAC-hbr`, what native RTSP cameras almost always
    /// emit.
    Mpeg4Generic,
    /// RFC 6416.
    Latm,
}

impl AacPayload {
    /// Every passthrough format, in the order `all` offers them.
    ///
    /// `MPEG4-GENERIC` leads because it is the one the most clients can
    /// take, and a client that picks only the first audio track it
    /// understands should land on it.
    const ALL: [Self; 2] = [Self::Mpeg4Generic, Self::Latm];

    /// The GStreamer payloader element for this format.
    fn element(&self) -> &'static str {
        match self {
            Self::Mpeg4Generic => "rtpmp4gpay",
            Self::Latm => "rtpmp4apay",
        }
    }

    /// The `a=rtpmap` encoding name it puts in the SDP.
    fn encoding_name(&self) -> &'static str {
        match self {
            Self::Mpeg4Generic => "MPEG4-GENERIC",
            Self::Latm => "MP4A-LATM",
        }
    }

    /// A short name for element and log-line use.
    fn slug(&self) -> &'static str {
        match self {
            Self::Mpeg4Generic => "generic",
            Self::Latm => "latm",
        }
    }
}

/// Lower bound applied to the per-queue time limit.
///
/// `buffer_duration` is validated to at least 1ms, but a queue that can
/// hold less than a frame just stalls the pipeline, so we never configure
/// one below this.
const MIN_QUEUE_TIME: Duration = Duration::from_millis(50);

fn clamp_queue_time(requested: Duration) -> Duration {
    requested.max(MIN_QUEUE_TIME)
}

#[derive(Clone, Debug)]
struct StreamConfig {
    #[allow(dead_code)]
    resolution: [u32; 2],
    bitrate: u32,
    fps: u32,
    bitrate_table: Vec<u32>,
    fps_table: Vec<u32>,
    vid_type: Option<VideoType>,
    aud_type: Option<AudioType>,
    /// How much media the pipeline queues are allowed to hold. This is the
    /// dominant contributor to the latency neolink itself adds, so it is
    /// exposed as the per-camera `buffer_duration` option.
    queue_time: Duration,
    /// Whether AAC is passed through as LATM or decoded to L16.
    audio_format: AudioFormat,
}
impl StreamConfig {
    async fn new(
        instance: &NeoInstance,
        name: StreamKind,
        queue_time: Duration,
        audio_format: AudioFormat,
    ) -> AnyResult<Self> {
        let (resolution, bitrate, fps, fps_table, bitrate_table) = instance
            .run_passive_task(|cam| {
                Box::pin(async move {
                    let infos = cam
                        .get_stream_info()
                        .await?
                        .stream_infos
                        .iter()
                        .flat_map(|info| info.encode_tables.clone())
                        .collect::<Vec<_>>();
                    if let Some(encode) =
                        infos.iter().find(|encode| encode.name == name.to_string())
                    {
                        let bitrate_table = encode
                            .bitrate_table
                            .split(',')
                            .filter_map(|c| {
                                let i: Result<u32, _> = c.parse();
                                i.ok()
                            })
                            .collect::<Vec<u32>>();
                        let framerate_table = encode
                            .framerate_table
                            .split(',')
                            .filter_map(|c| {
                                let i: Result<u32, _> = c.parse();
                                i.ok()
                            })
                            .collect::<Vec<u32>>();

                        Ok((
                            [encode.resolution.width, encode.resolution.height],
                            bitrate_table
                                .get(encode.default_bitrate as usize)
                                .copied()
                                .unwrap_or(encode.default_bitrate)
                                * 1024,
                            framerate_table
                                .get(encode.default_framerate as usize)
                                .copied()
                                .unwrap_or(encode.default_framerate),
                            framerate_table.clone(),
                            bitrate_table.clone(),
                        ))
                    } else {
                        Ok(([0, 0], 0, 0, vec![], vec![]))
                    }
                })
            })
            .await?;

        Ok(StreamConfig {
            resolution,
            bitrate,
            fps,
            fps_table,
            bitrate_table,
            vid_type: None,
            aud_type: None,
            queue_time: clamp_queue_time(queue_time),
            audio_format,
        })
    }

    fn update_fps(&mut self, fps: u32) {
        let new_fps = self.fps_table.get(fps as usize).copied().unwrap_or(fps);
        self.fps = new_fps;
    }
    #[allow(dead_code)]
    fn update_bitrate(&mut self, bitrate: u32) {
        let new_bitrate = self
            .bitrate_table
            .get(bitrate as usize)
            .copied()
            .unwrap_or(bitrate);
        self.bitrate = new_bitrate;
    }

    fn update_from_media(&mut self, media: &BcMedia) {
        match media {
            BcMedia::InfoV1(BcMediaInfoV1 { fps, .. })
            | BcMedia::InfoV2(BcMediaInfoV2 { fps, .. }) => self.update_fps(*fps as u32),
            BcMedia::Aac(aac) => {
                self.aud_type = Some(AudioType::Aac(AacFraming::from_frame(aac)));
            }
            BcMedia::Adpcm(adpcm) => {
                self.aud_type = Some(AudioType::Adpcm(adpcm.block_size()));
            }
            BcMedia::Iframe(BcMediaIframe { video_type, .. })
            | BcMedia::Pframe(BcMediaPframe { video_type, .. }) => {
                self.vid_type = Some(*video_type);
            }
        }
    }
}

pub(super) async fn make_dummy_factory(
    use_splash: bool,
    pattern: String,
) -> AnyResult<NeoMediaFactory> {
    NeoMediaFactory::new_with_callback(move |element, _audio_format| {
        clear_bin(&element)?;
        if !use_splash {
            Ok(None)
        } else {
            build_unknown(&element, &pattern)?;
            Ok(Some(element))
        }
    })
    .await
}

enum ClientMsg {
    NewClient {
        element: Element,
        /// The format this client asked for on its URL, overriding the
        /// camera's configured one for this client alone.
        audio_format: Option<AudioFormat>,
        reply: tokio::sync::oneshot::Sender<Element>,
    },
}

pub(super) async fn make_factory(
    camera: NeoInstance,
    stream: StreamKind,
) -> AnyResult<(NeoMediaFactory, JoinHandle<AnyResult<()>>)> {
    let (client_tx, mut client_rx) = mpsc(100);
    // Create the task that creates the pipelines
    let thread = tokio::task::spawn(async move {
        let name = camera.config().await?.borrow().name.clone();

        while let Some(msg) = client_rx.recv().await {
            match msg {
                ClientMsg::NewClient {
                    element,
                    audio_format,
                    reply,
                } => {
                    log::debug!("New client for {name}::{stream}");
                    let camera = camera.clone();
                    let name = name.clone();
                    tokio::task::spawn(async move {
                        clear_bin(&element)?;
                        log::trace!("{name}::{stream}: Starting camera");

                        // Start the camera
                        let config = camera.config().await?.borrow().clone();
                        let mut media_rx = camera.stream_while_live(stream).await?;

                        log::trace!("{name}::{stream}: Learning camera stream type");
                        // Learn the camera data type
                        let mut buffer = vec![];
                        let mut frame_count = 0usize;

                        // A `?audio=` on the client's URL applies to that
                        // client only; every other client on this camera
                        // keeps the configured format. Each one gets its
                        // own media (the factory is not shared), so they
                        // can hold different formats at the same time.
                        let audio_format = audio_format.unwrap_or(config.audio_format);
                        let mut stream_config = StreamConfig::new(
                            &camera,
                            stream,
                            Duration::from_millis(config.buffer_duration),
                            audio_format,
                        )
                        .await?;
                        // Bound stream-type negotiation. A slow or flaky camera that
                        // never delivers frames must not hang this per-client task
                        // forever holding `media_rx` / `element`; on timeout we build
                        // with whatever we have learned so far (falling back to the
                        // "Stream not Ready" splash when the video type is still
                        // unknown).
                        let _ = tokio::time::timeout(Duration::from_secs(10), async {
                            while let Some(media) = media_rx.recv().await {
                                stream_config.update_from_media(&media);
                                buffer.push(media);
                                if frame_count > 10
                                    || (stream_config.vid_type.is_some()
                                        && stream_config.aud_type.is_some())
                                {
                                    break;
                                }
                                frame_count += 1;
                            }
                        })
                        .await;

                        log::trace!("{name}::{stream}: Building the pipeline");
                        // Build the right video pipeline
                        let vid_src = match stream_config.vid_type.as_ref() {
                            Some(VideoType::H264) => {
                                let src = build_h264(&element, &stream_config)?;
                                AnyResult::Ok(Some(src))
                            }
                            Some(VideoType::H265) => {
                                let src = build_h265(&element, &stream_config)?;
                                AnyResult::Ok(Some(src))
                            }
                            None => {
                                build_unknown(&element, &config.splash_pattern.to_string())?;
                                AnyResult::Ok(None)
                            }
                        }?;

                        // Build the right audio pipeline
                        let aud_src = match stream_config.aud_type.as_ref() {
                            Some(AudioType::Aac(framing)) => {
                                // The frames we learned the stream from are
                                // also what the LATM negotiation probe gets
                                // to try, so the decision is made against
                                // this camera's real audio.
                                let samples = buffer
                                    .iter()
                                    .filter_map(|media| match media {
                                        BcMedia::Aac(aac) => Some(aac.data.clone()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>();
                                let tracks = decide_audio_tracks_off_thread(
                                    samples,
                                    *framing,
                                    stream_config.clone(),
                                )
                                .await;
                                let src = build_aac(&element, *framing, tracks, &stream_config)?;
                                AnyResult::Ok(Some(src))
                            }
                            Some(AudioType::Adpcm(block_size)) => {
                                let src = build_adpcm(&element, *block_size, &stream_config)?;
                                AnyResult::Ok(Some(src))
                            }
                            None => AnyResult::Ok(None),
                        }?;

                        if let Some(app) = vid_src.as_ref() {
                            app.set_callbacks(
                                AppSrcCallbacks::builder()
                                    .seek_data(move |_, _seek_pos| true)
                                    .build(),
                            );
                        }
                        if let Some(app) = aud_src.as_ref() {
                            app.set_callbacks(
                                AppSrcCallbacks::builder()
                                    .seek_data(move |_, _seek_pos| true)
                                    .build(),
                            );
                        }

                        log::trace!("{name}::{stream}: Sending pipeline to gstreamer");
                        // Send the pipeline back to the factory so it can start
                        let _ = reply.send(element);

                        // Run blocking code on a separate thread
                        // This is not an async thread
                        let pump_handle = tokio::runtime::Handle::current();
                        // Opt-in only: with `max_fps` unset (or 0) this is None,
                        // no limiter state exists, and every frame is forwarded
                        // exactly as it was before the option existed.
                        let mut fps_limiter = GopLimiter::for_config(config.max_fps);
                        if fps_limiter.is_some() {
                            log::debug!(
                                "{name}::{stream}: limiting video to {:?} fps (GOP-aligned)",
                                config.max_fps
                            );
                        }
                        std::thread::spawn(move || {
                            let mut tracker = TimestampTracker::new();

                            log::trace!("{name}::{stream}: Sending buffered frames");
                            for buffered in buffer.drain(..) {
                                send_to_sources(
                                    buffered,
                                    &vid_src,
                                    &aud_src,
                                    &mut tracker,
                                    fps_limiter.as_mut(),
                                )?;
                            }

                            log::trace!("{name}::{stream}: Sending new frames");
                            loop {
                                // Wait for the next frame, but wake periodically so we
                                // notice the client disconnecting even when the camera
                                // has stopped sending frames (e.g. motion paused).
                                // Without this bound the thread parks on recv() forever
                                // after the client leaves, pinning the AppSrcs and the
                                // upstream camera session.
                                match pump_handle.block_on(async {
                                    tokio::time::timeout(Duration::from_secs(2), media_rx.recv())
                                        .await
                                }) {
                                    Ok(Some(data)) => {
                                        let r = send_to_sources(
                                            data,
                                            &vid_src,
                                            &aud_src,
                                            &mut tracker,
                                            fps_limiter.as_mut(),
                                        );
                                        if let Err(r) = &r {
                                            log::info!("Failed to send to source: {r:?}");
                                        }
                                        r?;
                                    }
                                    // Channel closed: nothing more will arrive.
                                    Ok(None) => break,
                                    // No frame for a while: stop if the media has been
                                    // torn down (i.e. the client disconnected). Log and
                                    // break gracefully rather than `?`-ing the error
                                    // away, since this thread's JoinHandle is discarded.
                                    Err(_) => {
                                        if let Some(src) = vid_src.as_ref().or(aud_src.as_ref()) {
                                            if let Err(e) = check_live(src) {
                                                log::debug!(
                                                    "{name}::{stream}: Stopping frame pump: {e:?}"
                                                );
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            log::trace!("All media received");
                            AnyResult::Ok(())
                        });
                        AnyResult::Ok(())
                    });
                }
            }
        }
        AnyResult::Ok(())
    });

    // Now setup the factory
    let factory = NeoMediaFactory::new_with_callback(move |element, audio_format| {
        let (reply, new_element) = tokio::sync::oneshot::channel();
        client_tx.blocking_send(ClientMsg::NewClient {
            element,
            audio_format,
            reply,
        })?;

        let element = new_element.blocking_recv()?;
        Ok(Some(element))
    })
    .await?;
    Ok((factory, thread))
}

fn send_to_sources(
    data: BcMedia,
    vid_src: &Option<AppSrc>,
    aud_src: &Option<AppSrc>,
    tracker: &mut TimestampTracker,
    fps_limiter: Option<&mut GopLimiter>,
) -> AnyResult<()> {
    match data {
        BcMedia::Aac(aac) => {
            let duration = aac.duration().expect("Could not calculate AAC duration");
            let ts_us = tracker.next_audio_us(duration);
            if let Some(aud_src) = aud_src.as_ref() {
                log::debug!("Sending AAC: {:?}", Duration::from_micros(ts_us));
                send_to_appsrc(aud_src, aac.data, Duration::from_micros(ts_us), false)?;
            }
        }
        BcMedia::Adpcm(adpcm) => {
            let duration = adpcm
                .duration()
                .expect("Could not calculate ADPCM duration");
            let ts_us = tracker.next_audio_us(duration);
            if let Some(aud_src) = aud_src.as_ref() {
                log::trace!("Sending ADPCM: {:?}", Duration::from_micros(ts_us));
                send_to_appsrc(aud_src, adpcm.data, Duration::from_micros(ts_us), false)?;
            }
        }
        BcMedia::Iframe(BcMediaIframe {
            data,
            microseconds,
            time,
            ..
        }) => {
            // The tracker is advanced for every frame, including dropped
            // ones, so that its camera-clock baseline stays correct and the
            // surviving frames keep their true capture times.
            let ts_us = tracker.next_video_us(microseconds);
            let send = fps_limiter.is_none_or(|l| l.admit(ts_us, true));
            if let Some(posix) = time {
                log::trace!(
                    "IFrame: pts={:?} camera_us={} posix={} send={}",
                    Duration::from_micros(ts_us),
                    microseconds,
                    posix,
                    send
                );
            } else {
                log::trace!(
                    "IFrame: pts={:?} camera_us={} send={}",
                    Duration::from_micros(ts_us),
                    microseconds,
                    send
                );
            }
            if send {
                if let Some(vid_src) = vid_src.as_ref() {
                    send_to_appsrc(vid_src, data, Duration::from_micros(ts_us), false)?;
                }
            }
        }
        BcMedia::Pframe(BcMediaPframe {
            data, microseconds, ..
        }) => {
            let ts_us = tracker.next_video_us(microseconds);
            let send = fps_limiter.is_none_or(|l| l.admit(ts_us, false));
            log::trace!(
                "PFrame: pts={:?} camera_us={} send={}",
                Duration::from_micros(ts_us),
                microseconds,
                send
            );
            if send {
                if let Some(vid_src) = vid_src.as_ref() {
                    send_to_appsrc(vid_src, data, Duration::from_micros(ts_us), true)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Upper bound on the send allowance [`GopLimiter`] will bank, in
/// microseconds.
///
/// Allowance accrues while the tail of a GOP is being dropped and is spent
/// on the head of the next one, so this has to cover a whole GOP interval
/// for the limiter to reach its target rate — Reolink keyframe intervals
/// are typically 1-4 s. The cap is what stops a long stall (camera asleep,
/// motion-gated stream) from banking minutes of allowance and then
/// releasing it as one enormous burst.
const BURST_CAP_US: i64 = 5_000_000;

/// Per-client video decimator backing the `max_fps` camera option.
///
/// Constructed only when the option is set — see [`GopLimiter::for_config`].
/// One instance lives in each client's blocking frame-pump thread, so two
/// clients on the same camera decimate independently. Audio is never
/// throttled and never reaches this type.
///
/// # Why this is GOP-aligned
///
/// H.264/H.265 P-frames are predicted from the frames before them, so a
/// decimator that drops frames from the *middle* of a group of pictures
/// leaves the survivors referencing frames that never arrived, and the
/// decoder shows artefacts until the next keyframe. This limiter therefore
/// only ever drops a *suffix* of a GOP: once a frame is withheld, every
/// remaining frame is withheld until the next keyframe restarts the
/// prediction chain. Everything that reaches the client is decodable.
///
/// Two consequences fall out of that, both intended:
///
/// * **Output is bursty.** Frames arrive as a run at the camera's native
///   rate followed by a gap, rather than smoothly spaced. Averaged over a
///   GOP the rate is `max_fps`; instantaneously it is not.
/// * **Keyframes set a floor.** They are never dropped, so a stream whose
///   keyframe rate alone already exceeds `max_fps` is passed through at
///   that keyframe rate. Dropping keyframes would blank the stream rather
///   than thin it.
///
/// Smoothly spaced output at an arbitrary rate would require re-encoding,
/// which is far more expensive than the CPU this option is meant to save.
struct GopLimiter {
    /// Allowance consumed by one forwarded frame, in microseconds.
    interval_us: i64,
    /// PTS of the previous video frame, used to accrue allowance.
    last_pts_us: Option<u64>,
    /// Allowance banked so far, in microseconds. Capped at [`BURST_CAP_US`].
    credit_us: i64,
    /// Cleared once a frame has been withheld in the current GOP; the next
    /// keyframe sets it again.
    chain_intact: bool,
}

impl GopLimiter {
    /// Build a limiter for a camera's `max_fps` setting, or `None` when the
    /// option is not in use.
    ///
    /// An unset option and an explicit `0` both mean "no limit", and both
    /// return `None` so that the frame path stays exactly as it is for
    /// everyone who has not opted in.
    fn for_config(max_fps: Option<u32>) -> Option<Self> {
        let max_fps = max_fps.filter(|fps| *fps > 0)?;
        Some(Self {
            // max_fps is non-zero, so this cannot divide by zero, and the
            // result fits an i64 comfortably (1 fps → 1_000_000).
            interval_us: (1_000_000 / max_fps as u64) as i64,
            last_pts_us: None,
            credit_us: 0,
            chain_intact: false,
        })
    }

    /// Account for one video frame, returning whether to forward it.
    ///
    /// Must be called exactly once per video frame — including frames that
    /// end up dropped — since the elapsed time between frames is what funds
    /// the allowance. `pts_us` is the monotonic presentation timestamp from
    /// [`TimestampTracker`], not the raw camera value, so it is already
    /// free of wrap and restart artefacts.
    fn admit(&mut self, pts_us: u64, is_keyframe: bool) -> bool {
        if let Some(last) = self.last_pts_us {
            let elapsed = pts_us.saturating_sub(last).min(i64::MAX as u64) as i64;
            self.credit_us = self.credit_us.saturating_add(elapsed).min(BURST_CAP_US);
        }
        self.last_pts_us = Some(pts_us);

        if is_keyframe {
            // Always forwarded: a keyframe is what makes the frames after it
            // decodable. Clamping the charge at zero rather than letting it
            // go negative means a stream whose keyframe rate already exceeds
            // max_fps settles at that rate instead of running up a debt it
            // would repay by starving the P-frames of later GOPs.
            self.chain_intact = true;
            self.credit_us = (self.credit_us - self.interval_us).max(0);
            return true;
        }

        if !self.chain_intact {
            return false;
        }

        if self.credit_us >= self.interval_us {
            self.credit_us -= self.interval_us;
            true
        } else {
            // Withholding this frame orphans every later frame in the GOP,
            // so stop forwarding until the next keyframe.
            self.chain_intact = false;
            false
        }
    }
}

#[cfg(test)]
mod fps_limit_tests {
    use super::*;

    /// Feed `gops` groups of `gop_len` frames at `camera_fps`, returning one
    /// entry per frame recording whether it was forwarded.
    fn run(limiter: &mut GopLimiter, camera_fps: u64, gop_len: usize, gops: usize) -> Vec<bool> {
        let step = 1_000_000 / camera_fps;
        (0..gops * gop_len)
            .map(|i| limiter.admit(i as u64 * step, i % gop_len == 0))
            .collect()
    }

    #[test]
    fn unset_option_builds_no_limiter() {
        assert!(GopLimiter::for_config(None).is_none());
    }

    #[test]
    fn zero_means_no_limiter() {
        assert!(GopLimiter::for_config(Some(0)).is_none());
    }

    #[test]
    fn set_option_builds_a_limiter() {
        assert!(GopLimiter::for_config(Some(5)).is_some());
    }

    #[test]
    fn keyframes_are_never_dropped() {
        // 1 fps against a 15 fps camera with a 1 s GOP: the keyframe rate
        // alone is above the limit, so every keyframe still goes out.
        let mut limiter = GopLimiter::for_config(Some(1)).unwrap();
        let sent = run(&mut limiter, 15, 15, 6);
        for (i, sent) in sent.iter().enumerate() {
            if i % 15 == 0 {
                assert!(sent, "keyframe {} was dropped", i);
            }
        }
    }

    #[test]
    fn drops_only_gop_suffixes() {
        // The invariant that makes the output decodable: within any GOP,
        // forwarded frames form a prefix — no frame is forwarded after one
        // has been withheld.
        let mut limiter = GopLimiter::for_config(Some(5)).unwrap();
        let sent = run(&mut limiter, 15, 30, 8);
        for gop in sent.chunks(30) {
            let dropped_at = gop.iter().position(|s| !s);
            if let Some(first_drop) = dropped_at {
                assert!(
                    gop[first_drop..].iter().all(|s| !s),
                    "frame forwarded after a drop within the same GOP: {:?}",
                    gop
                );
            }
        }
    }

    #[test]
    fn converges_on_the_requested_rate() {
        // 15 fps camera, 2 s GOP, limited to 5 fps. Measured over the
        // steady-state GOPs (the first is a warm-up, since no allowance has
        // accrued yet) the output should sit at ~5 fps.
        let mut limiter = GopLimiter::for_config(Some(5)).unwrap();
        let sent = run(&mut limiter, 15, 30, 10);
        let steady: usize = sent[30..].iter().filter(|s| **s).count();
        let seconds = 9.0 * 30.0 / 15.0;
        let rate = steady as f64 / seconds;
        assert!(
            (4.0..=6.0).contains(&rate),
            "expected ~5 fps, measured {:.2} fps",
            rate
        );
    }

    #[test]
    fn limit_at_camera_rate_drops_nothing() {
        let mut limiter = GopLimiter::for_config(Some(15)).unwrap();
        let sent = run(&mut limiter, 15, 30, 5);
        assert!(sent.iter().all(|s| *s), "frames dropped at the camera rate");
    }

    #[test]
    fn limit_above_camera_rate_drops_nothing() {
        let mut limiter = GopLimiter::for_config(Some(30)).unwrap();
        let sent = run(&mut limiter, 15, 30, 5);
        assert!(sent.iter().all(|s| *s), "frames dropped below the limit");
    }

    #[test]
    fn stall_does_not_bank_an_unbounded_burst() {
        // A camera that goes quiet must not bank allowance for the whole
        // silence and then release it: the catch-up burst has to be bounded
        // by BURST_CAP_US, not by how long the stream was idle.
        let burst_after = |stall_us: u64| {
            let mut limiter = GopLimiter::for_config(Some(5)).unwrap();
            limiter.admit(0, true);
            (0..600)
                .filter(|i| limiter.admit(stall_us + i * 66_666, *i == 0))
                .count()
        };

        let after_a_minute = burst_after(60_000_000);
        let after_a_day = burst_after(86_400_000_000);
        assert_eq!(
            after_a_minute, after_a_day,
            "burst length scaled with the stall: {} vs {}",
            after_a_minute, after_a_day
        );
        assert!(
            after_a_minute < 600,
            "the whole run was forwarded, so nothing was actually capped"
        );
    }

    #[test]
    fn non_monotonic_pts_does_not_panic() {
        // TimestampTracker guarantees monotonic PTS, but the limiter must
        // not panic on overflow if that ever changes.
        let mut limiter = GopLimiter::for_config(Some(5)).unwrap();
        limiter.admit(u64::MAX, true);
        limiter.admit(0, false);
        limiter.admit(u64::MAX, false);
    }
}

fn send_to_appsrc(
    appsrc: &AppSrc,
    data: Vec<u8>,
    mut ts: Duration,
    is_delta: bool,
) -> AnyResult<()> {
    check_live(appsrc)?; // Stop if appsrc is dropped

    // In live mode we follow the advice in
    // https://gstreamer.freedesktop.org/documentation/additional/design/element-source.html?gi-language=c#live-sources
    // Only push buffers when in play state and have a clock
    // we also timestamp at the current time
    if appsrc.is_live() {
        if let Some(time) = appsrc
            .current_clock_time()
            .and_then(|t| appsrc.base_time().map(|bt| t - bt))
        {
            if matches!(appsrc.current_state(), gstreamer::State::Playing) {
                ts = Duration::from_micros(time.useconds());
            } else {
                // Not playing
                return Ok(());
            }
        } else {
            // Clock not up yet
            return Ok(());
        }
    }

    // Wrap the frame bytes directly into a gstreamer buffer (zero-copy).
    //
    // This previously used a `HashMap<usize, BufferPool>` keyed by the frame
    // length. Compressed video frame sizes vary continuously, so that map
    // accumulated a separate, permanently-active `BufferPool` (each one
    // preallocating several buffers of that exact size) for every unique
    // frame size ever observed. Over a long-running stream that grows without
    // bound and is the source of the reported memory leak. `from_mut_slice`
    // hands the `Vec` straight to gstreamer with no pool and no extra copy.
    let mut buf = gstreamer::Buffer::from_mut_slice(data);
    {
        let buf_mut = buf
            .get_mut()
            .expect("freshly created buffer is uniquely owned");
        let time = ClockTime::from_useconds(ts.as_micros() as u64);
        buf_mut.set_dts(time);
        buf_mut.set_pts(time);
        // A buffer without DELTA_UNIT is a sync point. Every buffer we
        // pushed used to look like a keyframe, so downstream could not
        // tell an I-frame from a P-frame and had to treat mid-GOP data as
        // a valid place to start a client.
        if is_delta {
            buf_mut.set_flags(gstreamer::BufferFlags::DELTA_UNIT);
        }
    }

    // Push buffer into the appsrc. Back-pressure is handled by the appsrc
    // itself: it is configured with `leaky-type=downstream` and a bounded
    // `max-bytes` (see the appsrc setup in the `pipe_*` builders), so when a
    // slow client cannot keep up the oldest queued frames are dropped rather
    // than the queue (and our memory) growing without bound.
    match appsrc.push_buffer(buf) {
        Ok(_) => Ok(()),
        Err(FlowError::Flushing) => {
            // The pad is flushing (the media is being reconfigured or torn
            // down); drop this frame.
            log::debug!("{}: dropping frame while appsrc is flushing", appsrc.name());
            Ok(())
        }
        Err(e) => Err(anyhow!("Error in streaming: {e:?}")),
    }
}
fn check_live(app: &AppSrc) -> Result<()> {
    app.bus().ok_or(anyhow!("App source is closed"))?;
    app.pads()
        .iter()
        .all(|pad| pad.is_linked())
        .then_some(())
        .ok_or(anyhow!("App source is not linked"))
}

fn clear_bin(bin: &Element) -> Result<()> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    // Clear the autogenerated ones
    for element in bin.iterate_elements().into_iter().flatten() {
        bin.remove(&element)?;
    }

    Ok(())
}

fn build_unknown(bin: &Element, pattern: &str) -> Result<()> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Unknown Pipeline");
    let source = make_element("videotestsrc", "testvidsrc")?;
    source.set_property_from_str("pattern", pattern);
    source.set_property("num-buffers", 500i32); // Send buffers then EOS
    let queue = make_queue("queue0", 1024 * 1024 * 4, Duration::from_secs(1))?;

    let overlay = make_element("textoverlay", "overlay")?;
    overlay.set_property("text", "Stream not Ready");
    overlay.set_property_from_str("valignment", "top");
    overlay.set_property_from_str("halignment", "left");
    overlay.set_property("font-desc", "Sans, 16");
    let encoder = make_element("jpegenc", "encoder")?;
    let payload = make_element("rtpjpegpay", "pay0")?;

    bin.add_many([&source, &queue, &overlay, &encoder, &payload])?;
    source.link_filtered(
        &queue,
        &Caps::builder("video/x-raw")
            .field("format", "YUY2")
            .field("width", 896i32)
            .field("height", 512i32)
            .field("framerate", gstreamer::Fraction::new(25, 1))
            .build(),
    )?;
    Element::link_many([&queue, &overlay, &encoder, &payload])?;

    Ok(())
}

struct Linked {
    appsrc: AppSrc,
    output: Element,
}

/// Create an `appsrc` configured the way every neolink source wants it.
fn make_appsrc(name: &str, buffer_size: u32) -> Result<AppSrc> {
    let source = make_element("appsrc", name)?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;

    source.set_is_live(false);
    source.set_block(false);
    // Report no source latency. `min-latency` is in nanoseconds and was
    // previously set to `1000 / fps` — 40ns for a 25fps stream — which was
    // an accidental no-op rather than the one frame it reads as. Zero is
    // both what that actually did and what we want: whatever we declare
    // here is added to the delay before a client starts playing, and this
    // appsrc introduces no latency of its own. Jitter is absorbed by the
    // downstream queue instead.
    source.set_min_latency(0);
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(false);
    source.set_stream_type(AppStreamType::Stream);
    // Bound memory use under a slow/stalled client: once `max-bytes` is
    // reached the appsrc drops the oldest queued frames instead of letting
    // its internal queue grow without limit.
    source.set_leaky_type(AppLeakyType::Downstream);

    Ok(source)
}

/// Apply the low latency settings shared by the H264/H265 RTP payloaders.
///
/// * `config-interval=-1` multiplexes the parameter sets (SPS/PPS, plus VPS
///   for H265) into the stream with every IDR frame. Without it they are
///   only advertised in the SDP, so a client that joins between parameter
///   set updates — or any client that ignores the SDP `sprop-` fields — has
///   to wait for the camera to volunteer them again before it can decode.
/// * `aggregate-mode=zero-latency` bundles the NAL units that make up one
///   access unit into STAP-A packets but still forwards them as soon as the
///   VCL unit arrives, so it saves packets without holding a frame back
///   (unlike `max-stap`, which costs a full frame of latency).
///
/// `aggregate-mode` only exists from GStreamer 1.18, so it is set
/// defensively; setting a property an element does not have would panic.
fn tune_video_payloader(payload: &Element) {
    payload.set_property("config-interval", -1i32);
    if payload.has_property("aggregate-mode", None) {
        payload.set_property_from_str("aggregate-mode", "zero-latency");
    } else {
        log::debug!(
            "{}: no `aggregate-mode`, leaving NAL aggregation at the default",
            payload.name()
        );
    }
}

fn pipe_h264(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = buffer_size(stream_config.bitrate);
    log::debug!(
        "buffer_size: {buffer_size}, bitrate: {}",
        stream_config.bitrate
    );
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building H264 Pipeline");
    let source = make_appsrc("vidsrc", buffer_size)?
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;
    let queue = make_queue("source_queue", buffer_size, stream_config.queue_time)?;
    let parser = make_element("h264parse", "parser")?;
    // let stamper = make_element("h264timestamper", "stamper")?;

    bin.add_many([&source, &queue, &parser])?;
    Element::link_many([&source, &queue, &parser])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: parser,
    })
}

fn build_h264(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_h264(bin, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtph264pay", "pay0")?;
    tune_video_payloader(&payload);
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

fn pipe_h265(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = buffer_size(stream_config.bitrate);
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building H265 Pipeline");
    let source = make_appsrc("vidsrc", buffer_size)?
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;
    let queue = make_queue("source_queue", buffer_size, stream_config.queue_time)?;
    let parser = make_element("h265parse", "parser")?;
    // let stamper = make_element("h265timestamper", "stamper")?;

    bin.add_many([&source, &queue, &parser])?;
    Element::link_many([&source, &queue, &parser])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: parser,
    })
}

fn build_h265(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_h265(bin, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtph265pay", "pay0")?;
    tune_video_payloader(&payload);
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

/// Audio buffer budget. Audio seems to run at about 800kbs.
const AUD_BUFFER_SIZE: u32 = 512 * 1416;

/// The part of the audio pipeline every payload format shares:
///
/// ```text
/// appsrc ! queue ! aacparse
/// ```
///
/// The camera hands us AAC in ADTS framing; `aacparse` agrees on that
/// framing and derives the `AudioSpecificConfig` that both passthrough
/// payloaders advertise in the SDP as their `config=` parameter.
fn pipe_aac_head(
    bin: &Element,
    framing: AacFraming,
    stream_config: &StreamConfig,
) -> Result<Linked> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let source = make_appsrc("audsrc", AUD_BUFFER_SIZE)?;
    set_aac_caps(&source, framing);
    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let queue = make_queue("audqueue", AUD_BUFFER_SIZE, stream_config.queue_time)?;
    let parser = make_element("aacparse", "audparser")?;

    bin.add_many([&source, &queue, &parser])?;
    Element::link_many([&source, &queue, &parser])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: parser,
    })
}

/// Convert parsed AAC to the `raw` framing both passthrough payloaders
/// need, returning the element the payloader should be linked to.
///
/// `payload` only names the branch, so that `all` can build one of these
/// per passthrough track without two elements colliding on a name.
fn pipe_aac_raw_tail(bin: &Element, input: &Element, payload: AacPayload) -> Result<Element> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    // Neither payloader accepts framed AAC, so ask `aacparse` to convert
    // the camera's ADTS framing to `raw` rather than passing it through.
    let raw_aac = make_element("capsfilter", &format!("audrawcaps_{}", payload.slug()))?;
    raw_aac.set_property(
        "caps",
        Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("stream-format", "raw")
            .build(),
    );
    bin.add_many([&raw_aac])?;
    Element::link_many([input, &raw_aac])?;
    Ok(raw_aac)
}

/// Decode parsed AAC to raw samples for `rtpL16pay`, returning the element
/// the payloader should be linked to.
fn pipe_aac_pcm_tail(bin: &Element, input: &Element) -> Result<Element> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let decoder = match make_element("faad", "auddecoder_faad") {
        Ok(ele) => Ok(ele),
        Err(_) => make_element("avdec_aac", "auddecoder_avdec_aac"),
    }?;

    // The fallback
    let silence = make_element("audiotestsrc", "audsilence")?;
    silence.set_property_from_str("wave", "silence");
    let fallback_switch = make_element("fallbackswitch", "audfallbackswitch");
    if let Ok(fallback_switch) = fallback_switch.as_ref() {
        fallback_switch.set_property("timeout", 3u64 * 1_000_000_000u64);
        fallback_switch.set_property("immediate-fallback", true);
    }

    let encoder = make_element("audioconvert", "audencoder")?;

    bin.add_many([&decoder, &encoder])?;
    if let Ok(fallback_switch) = fallback_switch.as_ref() {
        bin.add_many([&silence, fallback_switch])?;
        Element::link_many([input, &decoder, fallback_switch, &encoder])?;
        Element::link_many([&silence, fallback_switch])?;
    } else {
        Element::link_many([input, &decoder, &encoder])?;
    }

    Ok(encoder)
}

/// The whole LATM chain, head and tail.
///
/// Used by the negotiation probe, which needs a standalone copy of exactly
/// what the serving pipeline would build.
fn pipe_aac_passthrough(
    bin: &Element,
    framing: AacFraming,
    payload: AacPayload,
    stream_config: &StreamConfig,
) -> Result<Linked> {
    log::debug!("Building Aac {} passthrough pipeline", payload.slug());
    let head = pipe_aac_head(bin, framing, stream_config)?;
    let output = pipe_aac_raw_tail(bin, &head.output, payload)?;
    Ok(Linked {
        appsrc: head.appsrc,
        output,
    })
}

/// Tell the audio appsrc what it is producing.
///
/// Only done when we actually saw an ADTS syncword on the wire; otherwise
/// the caps are left unset and `aacparse` typefinds the framing as before,
/// so a camera with some other AAC framing is no worse off than it was.
fn set_aac_caps(source: &AppSrc, framing: AacFraming) {
    if let Some(caps) = framing.caps() {
        source.set_caps(Some(&caps));
    }
}

/// Whether a passthrough payloader is present in this GStreamer install.
///
/// Both live in the same `rtp` plugin as the `rtpL16pay` we already
/// require, so this should always be true; we check anyway so a
/// stripped-down install degrades to L16 instead of failing to serve
/// audio at all.
fn has_payloader(payload: AacPayload) -> bool {
    ElementFactory::find(payload.element()).is_some()
}

/// How long a passthrough negotiation probe may take before we give up on
/// that format and serve L16.
const LATM_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Run the camera's own AAC frames through a throwaway copy of one
/// passthrough chain and report whether the payloader came out with usable
/// RTP caps.
///
/// This is the safety net behind the cheap
/// [`AacFraming::passthrough_blocker`] checks. An element that refuses to
/// negotiate does not just mute the audio: it errors out the media,
/// `gst_rtsp_media_prepare` fails, and gst-rtsp-server answers DESCRIBE
/// with `503 Service Unavailable` — so the *video* disappears too. There
/// is no way to recover from that once the media has been handed over, and
/// no realistic way to enumerate every AAC variant a camera might emit, so
/// we find out on a pipeline nobody is watching and keep the L16 path in
/// reserve.
fn passthrough_negotiates(
    samples: &[Vec<u8>],
    framing: AacFraming,
    payload: AacPayload,
    stream_config: &StreamConfig,
) -> Result<()> {
    let pipeline = Pipeline::with_name(&format!("{}probe", payload.slug()));
    let bin = pipeline.clone().upcast::<Element>();

    let linked = pipe_aac_passthrough(&bin, framing, payload, stream_config)?;
    let payload = make_element(payload.element(), "probepay")?;
    let sink = make_element("fakesink", "probesink")?;
    sink.set_property("sync", false);
    pipeline.add_many([&payload, &sink])?;
    Element::link_many([&linked.output, &payload, &sink])?;

    let verdict = probe_run(&pipeline, &linked.appsrc, &payload, samples);
    // Always tear the probe down, however it went; leaving it in PLAYING
    // would leak a pipeline (and its thread) per client connection.
    let _ = pipeline.set_state(State::Null);
    verdict
}

/// Body of [`passthrough_negotiates`], split out so its caller can stop
/// the pipeline on every path.
fn probe_run(
    pipeline: &Pipeline,
    appsrc: &AppSrc,
    payload: &Element,
    samples: &[Vec<u8>],
) -> Result<()> {
    pipeline.set_state(State::Playing)?;

    for (i, sample) in samples.iter().enumerate() {
        let mut buf = gstreamer::Buffer::from_mut_slice(sample.clone());
        if let Some(buf_mut) = buf.get_mut() {
            // The probe only cares about caps, so any monotonic stamp will
            // do; spacing them keeps `aacparse` from complaining.
            let ts = ClockTime::from_mseconds(i as u64 * 64);
            buf_mut.set_pts(ts);
            buf_mut.set_dts(ts);
        }
        appsrc.push_buffer(buf)?;
    }
    appsrc.end_of_stream()?;

    let msg = pipeline.bus().and_then(|bus| {
        bus.timed_pop_filtered(
            ClockTime::from_nseconds(LATM_PROBE_TIMEOUT.as_nanos() as u64),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        )
    });
    if let Some(msg) = msg.as_ref() {
        if let gstreamer::MessageView::Error(err) = msg.view() {
            return Err(anyhow!(
                "{}: {}",
                err.error(),
                err.debug().unwrap_or_else(|| "no detail".into())
            ));
        }
    }

    // Caps rather than EOS are the real question: the SDP is built from
    // them, and gst-rtsp-server answers 503 if a stream has none.
    let caps = payload
        .static_pad("src")
        .and_then(|pad| pad.current_caps())
        .ok_or_else(|| {
            anyhow!("the payloader produced no RTP caps within {LATM_PROBE_TIMEOUT:?}")
        })?;
    let structure = caps
        .structure(0)
        .ok_or_else(|| anyhow!("the payloader produced empty RTP caps"))?;
    if structure.get::<String>("config").is_err() {
        // Without the AudioSpecificConfig a client has nothing to
        // configure its decoder with, so the SDP would be unusable.
        return Err(anyhow!(
            "the payloader produced no `config` for the SDP (caps: {caps})"
        ));
    }
    Ok(())
}

/// One RTP payload format offered for this camera's audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioTrack {
    /// AAC forwarded untouched under the given payload format.
    Passthrough(AacPayload),
    /// AAC decoded to raw samples and sent as `L16`.
    Pcm,
}

/// The audio tracks the SDP offers, in the order they take `pay1`, `pay2`,
/// …
///
/// Never empty: L16 always works, so it is what everything falls back to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AudioTracks(Vec<AudioTrack>);

impl AudioTracks {
    /// The single-L16-track offering everything falls back to.
    fn pcm_only() -> Self {
        Self(vec![AudioTrack::Pcm])
    }

    fn iter(&self) -> impl Iterator<Item = AudioTrack> + '_ {
        self.0.iter().copied()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    /// What to log about what we settled on.
    fn describe(&self) -> String {
        self.iter()
            .map(|track| match track {
                AudioTrack::Passthrough(payload) => payload.encoding_name(),
                AudioTrack::Pcm => "L16",
            })
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

/// Work out what to offer, given the config and what this camera's audio
/// can actually be payloaded as.
fn decide_audio_tracks(
    samples: &[Vec<u8>],
    framing: AacFraming,
    stream_config: &StreamConfig,
) -> AudioTracks {
    let tracks = match stream_config.audio_format {
        AudioFormat::Pcm => AudioTracks::pcm_only(),
        AudioFormat::Mpeg4Generic => {
            single_passthrough(AacPayload::Mpeg4Generic, samples, framing, stream_config)
        }
        AudioFormat::Latm => single_passthrough(AacPayload::Latm, samples, framing, stream_config),
        AudioFormat::All => {
            // Every passthrough this camera can actually do, then L16 for
            // the clients that can take none of them — notably go2rtc's
            // WebRTC output, which cannot decode AAC in any framing.
            let mut tracks = AacPayload::ALL
                .iter()
                .copied()
                .filter(|payload| can_pass_through(*payload, samples, framing, stream_config))
                .map(AudioTrack::Passthrough)
                .collect::<Vec<_>>();
            tracks.push(AudioTrack::Pcm);
            AudioTracks(tracks)
        }
    };
    log::info!("Offering the audio as {}", tracks.describe());
    tracks
}

/// One passthrough track if this camera can do it, L16 if it cannot.
fn single_passthrough(
    payload: AacPayload,
    samples: &[Vec<u8>],
    framing: AacFraming,
    stream_config: &StreamConfig,
) -> AudioTracks {
    if can_pass_through(payload, samples, framing, stream_config) {
        AudioTracks(vec![AudioTrack::Passthrough(payload)])
    } else {
        AudioTracks::pcm_only()
    }
}

/// Whether the AAC frames from this camera can be passed through under
/// `payload`.
///
/// Errs towards L16: a wrong "yes" costs the whole stream, a wrong "no"
/// costs only the decode we were trying to avoid.
fn can_pass_through(
    payload: AacPayload,
    samples: &[Vec<u8>],
    framing: AacFraming,
    stream_config: &StreamConfig,
) -> bool {
    if matches!(stream_config.audio_format, AudioFormat::Pcm) {
        return false;
    }
    let name = payload.encoding_name();
    if !has_payloader(payload) {
        log::warn!(
            "audio_format is \"{}\" but the `{}` element is missing \
             (install the rtp plugin from gst-plugins-good); \
             not offering a {name} track",
            stream_config.audio_format,
            payload.element()
        );
        return false;
    }
    if let Some(blocker) = framing.passthrough_blocker() {
        log::warn!("Not passing the audio through as {name}: {blocker}");
        return false;
    }
    if samples.is_empty() {
        // Nothing to probe with. The framing checks above already passed,
        // so go ahead rather than pessimising a stream we have no evidence
        // against.
        return true;
    }
    match passthrough_negotiates(samples, framing, payload, stream_config) {
        Ok(()) => true,
        Err(e) => {
            log::warn!(
                "The {name} audio pipeline would not negotiate with this camera's AAC \
                 ({e:#}); not offering a {name} track. If no passthrough format works \
                 for this camera, `audio_format` can be left at its default (\"pcm\") \
                 for it and this check skipped"
            );
            false
        }
    }
}

/// [`decide_audio_tracks`] off the async runtime.
///
/// The probe runs a real pipeline and waits on its bus, so it must not sit
/// on a runtime worker thread while it does.
async fn decide_audio_tracks_off_thread(
    samples: Vec<Vec<u8>>,
    framing: AacFraming,
    stream_config: StreamConfig,
) -> AudioTracks {
    tokio::task::spawn_blocking(move || decide_audio_tracks(&samples, framing, &stream_config))
        .await
        .unwrap_or_else(|e| {
            log::warn!("Could not check whether the audio can be passed through ({e:?}); decoding it to L16 instead");
            AudioTracks::pcm_only()
        })
}

/// Add a payloader to the bin and link it to the end of a branch.
fn attach_payloader(bin: &Element, output: &Element, kind: &str, name: &str) -> Result<()> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    let payload = make_element(kind, name)?;
    bin.add_many([&payload])?;
    Element::link_many([output, &payload])?;
    Ok(())
}

/// Build the audio half of the pipeline.
///
/// `tracks` is decided by [`decide_audio_tracks`] before we get here: a
/// half-built pipeline cannot be unwound cleanly, so the choice must be
/// settled before the bin is touched.
///
/// The payloaders must be named `pay0`, `pay1`, ... with no gaps —
/// gst-rtsp-server stops collecting at the first index it cannot find — so
/// the audio always starts at `pay1` (video is `pay0`) and the tracks are
/// numbered by position, whichever ones survived the checks.
fn build_aac(
    bin: &Element,
    framing: AacFraming,
    tracks: AudioTracks,
    stream_config: &StreamConfig,
) -> Result<AppSrc> {
    let head = pipe_aac_head(bin, framing, stream_config)?;
    log::debug!("Offering the audio as {}", tracks.describe());

    // With one track the parser feeds its branch directly; with more, a
    // tee and a queue per branch, so that the slowest branch (the decode)
    // cannot stall the tee and with it the passthroughs.
    let tee = if tracks.len() > 1 {
        let bin_as_bin = bin
            .clone()
            .dynamic_cast::<Bin>()
            .map_err(|_| anyhow!("Media source's element should be a bin"))?;
        let tee = make_element("tee", "audtee")?;
        bin_as_bin.add_many([&tee])?;
        Element::link_many([&head.output, &tee])?;
        Some((bin_as_bin, tee))
    } else {
        None
    };

    for (index, track) in tracks.iter().enumerate() {
        let name = format!("pay{}", index + 1);
        let input = match tee.as_ref() {
            Some((bin_as_bin, tee)) => {
                let queue = make_queue(
                    &format!("audbranch{index}"),
                    AUD_BUFFER_SIZE,
                    stream_config.queue_time,
                )?;
                bin_as_bin.add_many([&queue])?;
                Element::link_many([tee, &queue])?;
                queue
            }
            None => head.output.clone(),
        };

        match track {
            AudioTrack::Passthrough(payload) => {
                let out = pipe_aac_raw_tail(bin, &input, payload)?;
                attach_payloader(bin, &out, payload.element(), &name)?;
            }
            AudioTrack::Pcm => {
                let out = pipe_aac_pcm_tail(bin, &input)?;
                attach_payloader(bin, &out, "rtpL16pay", &name)?;
            }
        }
    }

    Ok(head.appsrc)
}

fn pipe_adpcm(bin: &Element, block_size: u32, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = AUD_BUFFER_SIZE;
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Adpcm pipeline");
    // Original command line
    // caps=audio/x-adpcm,layout=dvi,block_align={},channels=1,rate=8000
    // ! queue silent=true max-size-bytes=10485760 min-threshold-bytes=1024
    // ! adpcmdec
    // ! audioconvert
    // ! rtpL16pay name=pay1

    let source = make_appsrc("audsrc", buffer_size)?;

    source.set_caps(Some(
        &Caps::builder("audio/x-adpcm")
            .field("layout", "div")
            .field("block_align", block_size as i32)
            .field("channels", 1i32)
            .field("rate", 8000i32)
            .build(),
    ));

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let queue = make_queue("audqueue", buffer_size, stream_config.queue_time)?;
    let decoder = make_element("decodebin", "auddecoder")?;
    let encoder = make_element("audioconvert", "audencoder")?;
    let encoder_out = encoder.clone();

    bin.add_many([&source, &queue, &decoder, &encoder])?;
    Element::link_many([&source, &queue, &decoder])?;
    decoder.connect_pad_added(move |_element, pad| {
        let sink_pad = encoder
            .static_pad("sink")
            .expect("Encoder is missing its pad");
        pad.link(&sink_pad)
            .expect("Failed to link ADPCM decoder to encoder");
    });

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: encoder_out,
    })
}

fn build_adpcm(bin: &Element, block_size: u32, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_adpcm(bin, block_size, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtpL16pay", "pay1")?;
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

#[allow(dead_code)]
fn pipe_silence(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = AUD_BUFFER_SIZE;
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Silence pipeline");
    let source = make_appsrc("audsrc", buffer_size)?
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let sink_queue = make_queue("audsinkqueue", buffer_size, stream_config.queue_time)?;
    let sink = make_element("fakesink", "silence_sink")?;

    let silence = make_element("audiotestsrc", "audsilence")?;
    silence.set_property_from_str("wave", "silence");
    let src_queue = make_queue("audsrcqueue", buffer_size, stream_config.queue_time)?;
    let encoder = make_element("audioconvert", "audencoder")?;

    bin.add_many([&source, &sink_queue, &sink, &silence, &src_queue, &encoder])?;

    Element::link_many([&source, &sink_queue, &sink])?;

    Element::link_many([&silence, &src_queue, &encoder])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: encoder,
    })
}

#[allow(dead_code)]
struct AppSrcPair {
    vid: AppSrc,
    aud: Option<AppSrc>,
}

// #[allow(dead_code)]
// /// Experimental build a stream of MPEGTS
// fn build_mpegts(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrcPair> {
//     let buffer_size = buffer_size(stream_config.bitrate);
//     log::debug!(
//         "buffer_size: {buffer_size}, bitrate: {}",
//         stream_config.bitrate
//     );

//     // VID
//     let vid_link = match stream_config.vid_format {
//         VidFormat::H264 => pipe_h264(bin, stream_config)?,
//         VidFormat::H265 => pipe_h265(bin, stream_config)?,
//         VidFormat::None => unreachable!(),
//     };

//     // AUD
//     let aud_link = match stream_config.aud_format {
//         AudFormat::Aac => pipe_aac_latm(bin, adts, stream_config)?,
//         AudFormat::Adpcm(block) => pipe_adpcm(bin, block, stream_config)?,
//         AudFormat::None => pipe_silence(bin, stream_config)?,
//     };

//     let bin = bin
//         .clone()
//         .dynamic_cast::<Bin>()
//         .map_err(|_| anyhow!("Media source's element should be a bin"))?;

//     // MUX
//     let muxer = make_element("mpegtsmux", "mpeg_muxer")?;
//     let rtp = make_element("rtpmp2tpay", "pay0")?;

//     bin.add_many([&muxer, &rtp])?;
//     Element::link_many([&vid_link.output, &muxer, &rtp])?;
//     Element::link_many([&aud_link.output, &muxer])?;

//     Ok(AppSrcPair {
//         vid: vid_link.appsrc,
//         aud: Some(aud_link.appsrc),
//     })
// }

// Convenice funcion to make an element or provide a message
// about what plugin is missing
fn make_element(kind: &str, name: &str) -> AnyResult<Element> {
    ElementFactory::make_with_name(kind, Some(name)).with_context(|| {
        let plugin = match kind {
            "appsrc" => "app (gst-plugins-base)",
            "audioconvert" => "audioconvert (gst-plugins-base)",
            "adpcmdec" => "Required for audio",
            "h264parse" => "videoparsersbad (gst-plugins-bad)",
            "h265parse" => "videoparsersbad (gst-plugins-bad)",
            "rtph264pay" => "rtp (gst-plugins-good)",
            "rtph265pay" => "rtp (gst-plugins-good)",
            "rtpjitterbuffer" => "rtp (gst-plugins-good)",
            "aacparse" => "audioparsers (gst-plugins-good)",
            "rtpL16pay" => "rtp (gst-plugins-good)",
            "rtpmp4apay" => "rtp (gst-plugins-good)",
            "rtpmp4gpay" => "rtp (gst-plugins-good)",
            "tee" => "coreelements (gstreamer)",
            "capsfilter" => "coreelements (gstreamer)",
            "x264enc" => "x264 (gst-plugins-ugly)",
            "x265enc" => "x265 (gst-plugins-bad)",
            "avdec_h264" => "libav (gst-libav)",
            "avdec_h265" => "libav (gst-libav)",
            "videotestsrc" => "videotestsrc (gst-plugins-base)",
            "imagefreeze" => "imagefreeze (gst-plugins-good)",
            "audiotestsrc" => "audiotestsrc (gst-plugins-base)",
            "decodebin" => "playback (gst-plugins-good)",
            _ => "Unknown",
        };
        format!(
            "Missing required gstreamer plugin `{}` for `{}` element",
            plugin, kind
        )
    })
}

#[allow(dead_code)]
fn make_dbl_queue(name: &str, buffer_size: u32) -> AnyResult<Element> {
    let queue = make_element("queue", &format!("queue1_{}", name))?;
    queue.set_property("max-size-bytes", buffer_size);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", 0u64);
    // queue.set_property(
    //     "max-size-time",
    //     std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
    //         .unwrap_or(0),
    // );

    let queue2 = make_element("queue2", &format!("queue2_{}", name))?;
    queue2.set_property("max-size-bytes", buffer_size * 2u32 / 3u32);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue2.set_property(
        "max-size-time",
        std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
            .unwrap_or(0),
    );
    queue2.set_property("use-buffering", false);

    let bin = gstreamer::Bin::builder().name(name).build();
    bin.add_many([&queue, &queue2])?;
    Element::link_many([&queue, &queue2])?;

    let pad = queue
        .static_pad("sink")
        .expect("Failed to get a static pad from queue.");
    let ghost_pad = GhostPad::builder_with_target(&pad).unwrap().build();
    ghost_pad.set_active(true)?;
    bin.add_pad(&ghost_pad)?;

    let pad = queue2
        .static_pad("src")
        .expect("Failed to get a static pad from queue2.");
    let ghost_pad = GhostPad::builder_with_target(&pad).unwrap().build();
    ghost_pad.set_active(true)?;
    bin.add_pad(&ghost_pad)?;

    let bin = bin
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot convert bin"))?;
    Ok(bin)
}

/// Build a `queue` bounded by both bytes and time.
///
/// `max_time` is the ceiling on how much media the queue may hold, and so
/// on how much latency it can add once the client stops keeping up. It
/// comes from the camera's `buffer_duration` option; it used to be
/// hard-coded to 5 seconds, which meant a stalling client could push
/// neolink's own contribution to the glass-to-glass delay up to that.
fn make_queue(name: &str, buffer_size: u32, max_time: Duration) -> AnyResult<Element> {
    let queue = make_element("queue", &format!("queue1_{}", name))?;
    queue.set_property("max-size-bytes", buffer_size);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", max_time.as_nanos() as u64);
    Ok(queue)
}

fn buffer_size(bitrate: u32) -> u32 {
    // 0.1 seconds (according to bitrate) or 4kb what ever is larger
    std::cmp::max(bitrate * 2 / 8u32, 4u32 * 1024u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MPEG-4 ADTS, the framing `rtpmp4apay` can payload.
    const MP4_FRAMING: AacFraming = AacFraming {
        adts: true,
        mpegversion: Some(4),
    };

    fn test_stream_config(audio_format: AudioFormat) -> StreamConfig {
        StreamConfig {
            resolution: [1920, 1080],
            bitrate: 2048 * 1024,
            fps: 25,
            bitrate_table: vec![],
            fps_table: vec![],
            vid_type: None,
            aud_type: None,
            queue_time: Duration::from_millis(500),
            audio_format,
        }
    }

    /// Build one ADTS-framed AAC-LC frame: 16kHz, mono, 1024 samples.
    ///
    /// The payload is filler. `aacparse` derives the `AudioSpecificConfig`
    /// that `rtpmp4apay` advertises purely from the ADTS header fields, so
    /// framing and payloading can be exercised without a real encoder.
    fn adts_frame(payload_len: usize) -> Vec<u8> {
        adts_frame_versioned(payload_len, 4)
    }

    /// As [`adts_frame`], but for either MPEG version. The `ID` bit in byte
    /// 1 is what tells them apart: clear for MPEG-4, set for MPEG-2.
    fn adts_frame_versioned(payload_len: usize, mpegversion: i32) -> Vec<u8> {
        const HEADER_LEN: usize = 7;
        const PROFILE_AAC_LC: u8 = 1;
        const FREQ_IDX_16K: u8 = 8;
        const CHANNELS_MONO: u8 = 1;

        let len = HEADER_LEN + payload_len;
        let mut frame = vec![
            0xFF,
            // Layer 00, no CRC, plus the MPEG version.
            if mpegversion == 2 { 0xF9 } else { 0xF1 },
            (PROFILE_AAC_LC << 6) | (FREQ_IDX_16K << 2) | (CHANNELS_MONO >> 2),
            ((CHANNELS_MONO & 0b11) << 6) | ((len >> 11) & 0b11) as u8,
            ((len >> 3) & 0xFF) as u8,
            // Low 3 bits of the length, then buffer fullness = VBR.
            (((len & 0b111) << 5) | 0b1_1111) as u8,
            // Rest of buffer fullness, and one raw data block.
            0xFC,
        ];
        frame.resize(len, 0xAA);
        frame
    }

    /// The elements a test needs, or `None` if this GStreamer install is
    /// missing one (CI installs the dev headers but not every plugin).
    fn require(elements: &[&str]) -> bool {
        gstreamer::init().expect("gstreamer should initialise");
        for element in elements {
            if ElementFactory::find(element).is_none() {
                eprintln!("skipping: `{element}` is not available");
                return false;
            }
        }
        true
    }

    /// Anything we declare here is added to the delay before a client
    /// starts playing, and the appsrc adds no latency of its own.
    #[test]
    fn appsrc_declares_no_latency() {
        if !require(&["appsrc"]) {
            return;
        }
        let source = make_appsrc("testsrc", 4096).unwrap();
        assert_eq!(source.property::<i64>("min-latency"), 0);
    }

    #[test]
    fn queue_time_is_clamped_to_something_usable() {
        // `buffer_duration` validates down to 1ms; a queue that small
        // cannot hold a single frame and would just stall the pipeline.
        assert_eq!(clamp_queue_time(Duration::from_millis(1)), MIN_QUEUE_TIME);
        assert_eq!(
            clamp_queue_time(Duration::from_millis(3000)),
            Duration::from_millis(3000)
        );
    }

    /// Push synthetic ADTS through the real `build_aac` pipeline and check
    /// the RTP caps the client would be offered in the SDP.
    ///
    /// Run for both passthrough formats: they share every element except
    /// the payloader, and the `encoding-name` they end up advertising is
    /// the whole reason to have both — go2rtc recognises AAC only as
    /// `MPEG4-GENERIC`.
    #[test]
    fn each_passthrough_format_negotiates_its_own_encoding_name() {
        for (format, payload) in [
            (AudioFormat::Mpeg4Generic, AacPayload::Mpeg4Generic),
            (AudioFormat::Latm, AacPayload::Latm),
        ] {
            if !require(&[
                "appsrc",
                "queue",
                "aacparse",
                "capsfilter",
                payload.element(),
            ]) {
                continue;
            }

            let pipeline = Pipeline::new();
            let bin = pipeline.clone().upcast::<Element>();
            let config = test_stream_config(format);

            let samples = vec![adts_frame(64); 4];
            let tracks = decide_audio_tracks(&samples, MP4_FRAMING, &config);
            assert_eq!(
                tracks,
                AudioTracks(vec![AudioTrack::Passthrough(payload)]),
                "MPEG-4 ADTS should be passed through as {}",
                payload.encoding_name()
            );
            let appsrc = build_aac(&bin, MP4_FRAMING, tracks, &config)
                .expect("passthrough pipeline should build");

            let payloader = pipeline.by_name("pay1").expect("pay1 should exist");
            assert_eq!(
                payloader.factory().map(|f| f.name().to_string()).as_deref(),
                Some(payload.element()),
            );

            let sink = ElementFactory::make_with_name("fakesink", Some("testsink")).unwrap();
            pipeline.add(&sink).unwrap();
            payloader.link(&sink).unwrap();

            pipeline.set_state(State::Playing).unwrap();

            for i in 0..20u64 {
                let mut buf = gstreamer::Buffer::from_mut_slice(adts_frame(64));
                let time = ClockTime::from_mseconds(i * 64);
                let buf_mut = buf.get_mut().unwrap();
                buf_mut.set_pts(time);
                buf_mut.set_dts(time);
                appsrc.push_buffer(buf).expect("appsrc should accept ADTS");
            }
            appsrc.end_of_stream().unwrap();

            let msg = pipeline
                .bus()
                .unwrap()
                .timed_pop_filtered(
                    ClockTime::from_seconds(10),
                    &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
                )
                .expect("pipeline should reach EOS");
            if let gstreamer::MessageView::Error(err) = msg.view() {
                pipeline.set_state(State::Null).unwrap();
                panic!("{} pipeline errored: {:?}", payload.slug(), err.error());
            }

            let caps = payloader
                .static_pad("src")
                .unwrap()
                .current_caps()
                .expect("payloader should have negotiated caps");
            let s = caps.structure(0).unwrap();

            assert_eq!(s.name(), "application/x-rtp");
            assert_eq!(s.get::<String>("media").unwrap(), "audio");
            assert_eq!(
                s.get::<String>("encoding-name").unwrap(),
                payload.encoding_name(),
            );
            // The clock rate and `config` are what let a client decode the
            // passed-through frames; without them the SDP is unusable.
            assert_eq!(s.get::<i32>("clock-rate").unwrap(), 16_000);
            assert!(
                !s.get::<String>("config").unwrap().is_empty(),
                "SDP needs the AudioSpecificConfig"
            );

            pipeline.set_state(State::Null).unwrap();
        }
    }

    /// The framing has to be read off the wire, not assumed: declaring the
    /// wrong MPEG version to `aacparse` is what makes it refuse to unpack
    /// the ADTS at all.
    #[test]
    fn framing_is_learned_from_the_adts_header() {
        let mp4 = AacFraming::from_frame(&BcMediaAac {
            data: adts_frame_versioned(64, 4),
        });
        assert_eq!(mp4, MP4_FRAMING);

        let mp2 = AacFraming::from_frame(&BcMediaAac {
            data: adts_frame_versioned(64, 2),
        });
        assert_eq!(
            mp2,
            AacFraming {
                adts: true,
                mpegversion: Some(2)
            }
        );

        // No syncword: we know nothing, and must not claim otherwise.
        let raw = AacFraming::from_frame(&BcMediaAac {
            data: vec![0x00; 64],
        });
        assert_eq!(
            raw,
            AacFraming {
                adts: false,
                mpegversion: None
            }
        );
        assert!(raw.caps().is_none());
    }

    /// A camera sending MPEG-2 AAC must not be put on the LATM path.
    ///
    /// `rtpmp4apay` only payloads MPEG-4 AAC, and a payloader that cannot
    /// negotiate errors out the whole media — which costs the *video* too,
    /// as a `503 Service Unavailable` on DESCRIBE.
    #[test]
    fn mpeg2_aac_falls_back_to_l16() {
        if !require(&[
            "appsrc",
            "queue",
            "aacparse",
            "capsfilter",
            "rtpmp4apay",
            "rtpL16pay",
            "audioconvert",
        ]) {
            return;
        }
        if ElementFactory::find("faad").is_none() && ElementFactory::find("avdec_aac").is_none() {
            eprintln!("skipping: no AAC decoder available");
            return;
        }

        let pipeline = Pipeline::new();
        let bin = pipeline.clone().upcast::<Element>();
        let config = test_stream_config(AudioFormat::Latm);
        let framing = AacFraming {
            adts: true,
            mpegversion: Some(2),
        };

        let samples = vec![adts_frame_versioned(64, 2)];
        let tracks = decide_audio_tracks(&samples, framing, &config);
        assert_eq!(
            tracks,
            AudioTracks::pcm_only(),
            "MPEG-2 AAC cannot be passed through as LATM"
        );

        build_aac(&bin, framing, tracks, &config)
            .expect("MPEG-2 AAC should still build a pipeline");

        let payloader = pipeline.by_name("pay1").expect("pay1 should exist");
        assert_eq!(
            payloader.factory().map(|f| f.name().to_string()).as_deref(),
            Some("rtpL16pay"),
            "MPEG-2 AAC cannot be payloaded as LATM, so it must be decoded"
        );

        pipeline.set_state(State::Null).unwrap();
    }

    /// `audio_format = "all"` offers the client a choice: every
    /// passthrough format the camera can do, then L16, so a client that
    /// negotiates can set up only the one it wants.
    ///
    /// `MPEG4-GENERIC` leads. It is the format the most clients can take,
    /// and the only one go2rtc recognises as AAC — while its WebRTC output
    /// cannot take AAC at all and needs the L16 track, which is why both
    /// have to be on offer at once.
    #[test]
    fn all_offers_every_passthrough_then_l16() {
        if !require(&[
            "appsrc",
            "queue",
            "tee",
            "aacparse",
            "capsfilter",
            "rtpmp4gpay",
            "rtpmp4apay",
            "rtpL16pay",
            "audioconvert",
        ]) {
            return;
        }
        if ElementFactory::find("faad").is_none() && ElementFactory::find("avdec_aac").is_none() {
            eprintln!("skipping: no AAC decoder available");
            return;
        }

        let pipeline = Pipeline::new();
        let bin = pipeline.clone().upcast::<Element>();
        let config = test_stream_config(AudioFormat::All);

        let samples = vec![adts_frame(64); 4];
        let tracks = decide_audio_tracks(&samples, MP4_FRAMING, &config);
        assert_eq!(
            tracks,
            AudioTracks(vec![
                AudioTrack::Passthrough(AacPayload::Mpeg4Generic),
                AudioTrack::Passthrough(AacPayload::Latm),
                AudioTrack::Pcm,
            ])
        );

        build_aac(&bin, MP4_FRAMING, tracks, &config).expect("dual pipeline should build");

        let named = |name: &str| {
            pipeline
                .by_name(name)
                .and_then(|e| e.factory().map(|f| f.name().to_string()))
        };
        assert_eq!(named("pay1").as_deref(), Some("rtpmp4gpay"));
        assert_eq!(named("pay2").as_deref(), Some("rtpmp4apay"));
        assert_eq!(named("pay3").as_deref(), Some("rtpL16pay"));
        // One parse, split after it: the passthrough must not be paying for
        // the decode branch's work twice over.
        assert!(pipeline.by_name("audtee").is_some(), "branches share a tee");
        assert_eq!(
            pipeline
                .iterate_elements()
                .into_iter()
                .flatten()
                .filter(|e| { e.factory().map(|f| f.name() == "aacparse").unwrap_or(false) })
                .count(),
            1,
            "the two branches should share one parser"
        );

        pipeline.set_state(State::Null).unwrap();
    }

    /// When the camera can do no passthrough at all, `all` has to
    /// collapse to a single track — and it must still be `pay1`.
    /// gst-rtsp-server stops collecting payloaders at the first missing
    /// index, so a gap would lose every track after it.
    #[test]
    fn all_collapses_to_one_track_without_leaving_a_gap() {
        if !require(&["appsrc", "queue", "aacparse", "rtpL16pay", "audioconvert"]) {
            return;
        }
        if ElementFactory::find("faad").is_none() && ElementFactory::find("avdec_aac").is_none() {
            eprintln!("skipping: no AAC decoder available");
            return;
        }

        let pipeline = Pipeline::new();
        let bin = pipeline.clone().upcast::<Element>();
        let config = test_stream_config(AudioFormat::All);
        let framing = AacFraming {
            adts: true,
            mpegversion: Some(2),
        };

        let tracks = decide_audio_tracks(&[adts_frame_versioned(64, 2)], framing, &config);
        assert_eq!(
            tracks,
            AudioTracks::pcm_only(),
            "MPEG-2 AAC has no passthrough to offer"
        );

        build_aac(&bin, framing, tracks, &config).expect("pipeline should still build");
        assert_eq!(
            pipeline
                .by_name("pay1")
                .and_then(|e| e.factory().map(|f| f.name().to_string()))
                .as_deref(),
            Some("rtpL16pay"),
            "the surviving track must be pay1, not pay2"
        );
        assert!(
            pipeline.by_name("pay2").is_none(),
            "no second track was built, so pay2 must not exist"
        );

        pipeline.set_state(State::Null).unwrap();
    }

    /// The probe is the catch-all for framings we did not anticipate: it
    /// has to say no to anything that will not reach the payloader, and
    /// yes to what the camera actually sends.
    #[test]
    fn the_probe_matches_what_the_pipeline_can_do() {
        for payload in AacPayload::ALL {
            if !require(&[
                "appsrc",
                "queue",
                "aacparse",
                "capsfilter",
                payload.element(),
            ]) {
                continue;
            }
            let config = test_stream_config(AudioFormat::All);

            assert!(
                passthrough_negotiates(&vec![adts_frame(64); 4], MP4_FRAMING, payload, &config)
                    .is_ok(),
                "MPEG-4 ADTS is exactly what the {} path is for",
                payload.encoding_name()
            );

            // Garbage that is claimed to be ADTS: `aacparse` cannot make raw
            // AAC of it, so the payloader is never reached.
            let err =
                passthrough_negotiates(&vec![vec![0x00; 64]; 4], MP4_FRAMING, payload, &config)
                    .expect_err("non-AAC payload should not negotiate");
            log::debug!("probe rejected the payload: {err:#}");
        }
    }

    /// `audio_format = "pcm"` must still produce the decode-to-L16 shape.
    #[test]
    fn aac_pcm_pipeline_uses_the_l16_payloader() {
        if !require(&["appsrc", "queue", "aacparse", "audioconvert", "rtpL16pay"]) {
            return;
        }
        if ElementFactory::find("faad").is_none() && ElementFactory::find("avdec_aac").is_none() {
            eprintln!("skipping: no AAC decoder available");
            return;
        }

        let pipeline = Pipeline::new();
        let bin = pipeline.clone().upcast::<Element>();
        let config = test_stream_config(AudioFormat::Pcm);

        assert_eq!(
            decide_audio_tracks(&[], MP4_FRAMING, &config),
            AudioTracks::pcm_only(),
            "`audio_format = \"pcm\"` should never take a passthrough path"
        );
        build_aac(&bin, MP4_FRAMING, AudioTracks::pcm_only(), &config)
            .expect("PCM pipeline should build");

        let payloader = pipeline.by_name("pay1").expect("pay1 should exist");
        assert_eq!(
            payloader.factory().map(|f| f.name().to_string()).as_deref(),
            Some("rtpL16pay"),
        );
        // The decoder is the thing LATM exists to avoid; assert the PCM
        // path really does still have one.
        assert!(
            pipeline.by_name("auddecoder_faad").is_some()
                || pipeline.by_name("auddecoder_avdec_aac").is_some(),
            "PCM path should decode the AAC"
        );

        pipeline.set_state(State::Null).unwrap();
    }

    /// The H264/H265 payloaders must carry parameter sets in-band and must
    /// not hold a frame back to aggregate NAL units.
    #[test]
    fn video_payloaders_are_tuned_for_low_latency() {
        for (parser, payloader_name, build) in [
            (
                "h264parse",
                "rtph264pay",
                build_h264 as fn(&Element, &StreamConfig) -> Result<AppSrc>,
            ),
            ("h265parse", "rtph265pay", build_h265),
        ] {
            if !require(&["appsrc", "queue", parser, payloader_name]) {
                continue;
            }

            let pipeline = Pipeline::new();
            let bin = pipeline.clone().upcast::<Element>();
            let config = test_stream_config(AudioFormat::Latm);

            build(&bin, &config).expect("video pipeline should build");

            let payloader = pipeline.by_name("pay0").expect("pay0 should exist");
            assert_eq!(
                payloader.property::<i32>("config-interval"),
                -1,
                "{payloader_name}: parameter sets should ride with every IDR"
            );
            assert_eq!(
                payloader
                    .property_value("aggregate-mode")
                    .serialize()
                    .unwrap()
                    .as_str(),
                "zero-latency",
                "{payloader_name}: NAL aggregation must not delay packets"
            );

            pipeline.set_state(State::Null).unwrap();
        }
    }

    /// The queues must honour `buffer_duration` rather than the old
    /// hard-coded 5s ceiling.
    #[test]
    fn queues_use_the_configured_buffer_duration() {
        if !require(&["appsrc", "queue", "h264parse", "rtph264pay"]) {
            return;
        }

        let pipeline = Pipeline::new();
        let bin = pipeline.clone().upcast::<Element>();
        let mut config = test_stream_config(AudioFormat::Latm);
        config.queue_time = Duration::from_millis(250);

        build_h264(&bin, &config).expect("H264 pipeline should build");

        let queue = pipeline
            .by_name("queue1_source_queue")
            .expect("source queue should exist");
        assert_eq!(
            queue.property::<u64>("max-size-time"),
            Duration::from_millis(250).as_nanos() as u64
        );

        pipeline.set_state(State::Null).unwrap();
    }
}
