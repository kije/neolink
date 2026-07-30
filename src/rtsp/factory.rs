use gstreamer::ClockTime;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use gstreamer::{prelude::*, Bin, Caps, Element, ElementFactory, FlowError, GhostPad};
use gstreamer_app::{AppLeakyType, AppSrc, AppSrcCallbacks, AppStreamType};
use neolink_core::{
    bc_protocol::StreamKind,
    bcmedia::model::{
        BcMedia, BcMediaIframe, BcMediaInfoV1, BcMediaInfoV2, BcMediaPframe, VideoType,
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
    /// AAC. `adts` records whether the frames the camera sent carry ADTS
    /// framing, which every camera seen so far does. When it holds we can
    /// tell the appsrc exactly what it is producing instead of making
    /// `aacparse` typefind it, which matters for the LATM path: the
    /// payloader can only be reached once the parser has agreed on an
    /// input format.
    Aac {
        adts: bool,
    },
    Adpcm(u32),
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
                // `duration()` parses the ADTS header and returns `None`
                // when the syncword is not there, so it doubles as an
                // ADTS probe.
                self.aud_type = Some(AudioType::Aac {
                    adts: aac.duration().is_some(),
                });
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
    NeoMediaFactory::new_with_callback(move |element| {
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
                ClientMsg::NewClient { element, reply } => {
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

                        let mut stream_config = StreamConfig::new(
                            &camera,
                            stream,
                            Duration::from_millis(config.buffer_duration),
                            config.audio_format,
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
                            Some(AudioType::Aac { adts }) => {
                                let src = build_aac(&element, *adts, &stream_config)?;
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
                        let mut fps_limiter = FpsLimiter::new(stream_config.fps, config.max_fps);
                        std::thread::spawn(move || {
                            let mut tracker = TimestampTracker::new();

                            log::trace!("{name}::{stream}: Sending buffered frames");
                            for buffered in buffer.drain(..) {
                                send_to_sources(
                                    buffered,
                                    &vid_src,
                                    &aud_src,
                                    &mut tracker,
                                    &mut fps_limiter,
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
                                            &mut fps_limiter,
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
    let factory = NeoMediaFactory::new_with_callback(move |element| {
        let (reply, new_element) = tokio::sync::oneshot::channel();
        client_tx.blocking_send(ClientMsg::NewClient { element, reply })?;

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
    fps_limiter: &mut FpsLimiter,
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
            let send = fps_limiter.take();
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
            let send = fps_limiter.take();
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

/// Per-client video frame decimator backing the `max_fps` camera option.
///
/// One instance lives in each client's blocking frame-pump thread, so two
/// clients on the same camera decimate independently. Audio is never
/// throttled and never reaches this type.
struct FpsLimiter {
    camera_fps: u32,
    max_fps: Option<u32>,
    frame_count: u64,
}

impl FpsLimiter {
    fn new(camera_fps: u32, max_fps: Option<u32>) -> Self {
        Self {
            camera_fps,
            max_fps,
            frame_count: 0,
        }
    }

    /// Account for one video frame, returning whether it should be forwarded.
    ///
    /// Must be called exactly once per video frame — including frames that
    /// end up dropped — or the decimation ratio drifts.
    fn take(&mut self) -> bool {
        let send = should_send_frame(self.frame_count, self.camera_fps, self.max_fps);
        self.frame_count = self.frame_count.wrapping_add(1);
        send
    }
}

/// Returns `true` if this video frame should be forwarded to the GStreamer pipeline.
///
/// The timestamp tracker is advanced for dropped frames too; only the payload
/// is withheld. `limit = 0` or `limit >= camera_fps` means no frames are dropped.
fn should_send_frame(vid_frame_count: u64, camera_fps: u32, max_fps: Option<u32>) -> bool {
    match max_fps {
        Some(limit) if limit > 0 && camera_fps > limit => {
            // Ceiling-integer skip factor: 15 fps / 5 limit → skip 3
            let frame_skip = (camera_fps as u64).div_ceil(limit as u64);
            vid_frame_count.is_multiple_of(frame_skip)
        }
        _ => true,
    }
}

#[cfg(test)]
mod fps_limit_tests {
    use super::*;

    #[test]
    fn fps_15_to_5() {
        // skip=3 → frames 0,3,6,9,12 pass out of 15
        let sent: Vec<u64> = (0..15)
            .filter(|&i| should_send_frame(i, 15, Some(5)))
            .collect();
        assert_eq!(sent, vec![0, 3, 6, 9, 12]);
    }

    #[test]
    fn fps_no_limit() {
        for i in 0..100 {
            assert!(should_send_frame(i, 15, None));
        }
    }

    #[test]
    fn fps_limit_equals_camera() {
        for i in 0..15 {
            assert!(should_send_frame(i, 15, Some(15)));
        }
    }

    #[test]
    fn fps_camera_zero_no_panic() {
        // camera_fps=0 means the condition `camera_fps > limit` is never true
        for i in 0..10 {
            assert!(should_send_frame(i, 0, Some(5)));
        }
    }

    #[test]
    fn fps_limit_one() {
        // 30fps → skip=30 → only frame 0 (and 30, 60, …) pass
        assert!(should_send_frame(0, 30, Some(1)));
        for i in 1..30 {
            assert!(!should_send_frame(i, 30, Some(1)));
        }
        assert!(should_send_frame(30, 30, Some(1)));
    }

    #[test]
    fn fps_limit_zero_means_no_limit() {
        for i in 0..15 {
            assert!(should_send_frame(i, 15, Some(0)));
        }
    }

    #[test]
    fn fps_near_u64_max_no_panic() {
        let near_max = u64::MAX - 1;
        let _ = should_send_frame(near_max, 15, Some(5));
    }

    #[test]
    fn limiter_decimates_and_advances() {
        let mut limiter = FpsLimiter::new(15, Some(5));
        let sent: Vec<bool> = (0..15).map(|_| limiter.take()).collect();
        assert_eq!(sent.iter().filter(|s| **s).count(), 5);
        assert!(sent[0] && sent[3] && sent[6] && sent[9] && sent[12]);
        assert_eq!(limiter.frame_count, 15);
    }

    #[test]
    fn limiter_without_limit_sends_everything() {
        let mut limiter = FpsLimiter::new(15, None);
        assert!((0..50).all(|_| limiter.take()));
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
    if payload.has_property("aggregate-mode") {
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

/// Build the AAC passthrough (LATM) pipeline.
///
/// ```text
/// appsrc ! queue ! aacparse ! audio/mpeg,stream-format=raw ! rtpmp4apay name=pay1
/// ```
///
/// The camera hands us AAC in ADTS framing. `aacparse` strips the ADTS
/// headers and derives the `AudioSpecificConfig`, which `rtpmp4apay` then
/// advertises in the SDP as the `config=` parameter of an `MP4A-LATM`
/// (RFC 6416) media description. The compressed frames themselves are
/// forwarded to the client bit for bit.
///
/// Compared to the L16 path this removes an AAC decode, a format
/// conversion and a ~10x bitrate increase from the serving path, and it
/// drops the `fallbackswitch` (which cannot sit in a compressed stream)
/// along with the buffering it needs to do its silence substitution.
fn pipe_aac_latm(bin: &Element, adts: bool, stream_config: &StreamConfig) -> Result<Linked> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Aac LATM passthrough pipeline");

    let source = make_appsrc("audsrc", AUD_BUFFER_SIZE)?;
    set_adts_caps(&source, adts);
    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let queue = make_queue("audqueue", AUD_BUFFER_SIZE, stream_config.queue_time)?;
    let parser = make_element("aacparse", "audparser")?;
    // `rtpmp4apay` only accepts unframed AAC, so ask `aacparse` to convert
    // the camera's ADTS framing to `raw` rather than passing it through.
    let raw_aac = make_element("capsfilter", "audrawcaps")?;
    raw_aac.set_property(
        "caps",
        Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("stream-format", "raw")
            .build(),
    );

    bin.add_many([&source, &queue, &parser, &raw_aac])?;
    Element::link_many([&source, &queue, &parser, &raw_aac])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: raw_aac,
    })
}

/// Build the AAC -> raw -> L16 pipeline.
///
/// Kept for `audio_format = "pcm"`, and used automatically when the
/// `rtpmp4apay` payloader is not available in the local GStreamer install.
fn pipe_aac_pcm(bin: &Element, adts: bool, stream_config: &StreamConfig) -> Result<Linked> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Aac pipeline");
    let source = make_appsrc("audsrc", AUD_BUFFER_SIZE)?;
    set_adts_caps(&source, adts);
    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let queue = make_queue("audqueue", AUD_BUFFER_SIZE, stream_config.queue_time)?;
    let parser = make_element("aacparse", "audparser")?;
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

    bin.add_many([&source, &queue, &parser, &decoder, &encoder])?;
    if let Ok(fallback_switch) = fallback_switch.as_ref() {
        bin.add_many([&silence, fallback_switch])?;
        Element::link_many([
            &source,
            &queue,
            &parser,
            &decoder,
            fallback_switch,
            &encoder,
        ])?;
        Element::link_many([&silence, fallback_switch])?;
    } else {
        Element::link_many([&source, &queue, &parser, &decoder, &encoder])?;
    }

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: encoder,
    })
}

/// Tell the audio appsrc it is producing ADTS-framed AAC.
///
/// Only done when we actually saw an ADTS syncword on the wire; otherwise
/// the caps are left unset and `aacparse` typefinds the framing as before,
/// so a camera with some other AAC framing is no worse off than it was.
fn set_adts_caps(source: &AppSrc, adts: bool) {
    if adts {
        source.set_caps(Some(
            &Caps::builder("audio/mpeg")
                .field("mpegversion", 4i32)
                .field("stream-format", "adts")
                .build(),
        ));
    }
}

/// Whether the LATM payloader is present in this GStreamer install.
///
/// `rtpmp4apay` lives in the same `rtp` plugin as the `rtpL16pay` we
/// already require, so this should always be true; we check anyway so a
/// stripped-down install degrades to L16 instead of failing to serve
/// audio at all.
fn has_latm_payloader() -> bool {
    ElementFactory::find("rtpmp4apay").is_some()
}

fn build_aac(bin: &Element, adts: bool, stream_config: &StreamConfig) -> Result<AppSrc> {
    // Decide before touching the bin: a half-built pipeline cannot be
    // unwound cleanly, so we must not start on LATM and then discover the
    // payloader is missing.
    let use_latm = match stream_config.audio_format {
        AudioFormat::Latm if has_latm_payloader() => true,
        AudioFormat::Latm => {
            log::warn!(
                "audio_format is \"latm\" but the `rtpmp4apay` element is missing \
                 (install the rtp plugin from gst-plugins-good); \
                 falling back to decoding the audio to L16"
            );
            false
        }
        AudioFormat::Pcm => false,
    };

    let (linked, payload) = if use_latm {
        (
            pipe_aac_latm(bin, adts, stream_config)?,
            make_element("rtpmp4apay", "pay1")?,
        )
    } else {
        (
            pipe_aac_pcm(bin, adts, stream_config)?,
            make_element("rtpL16pay", "pay1")?,
        )
    };

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
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
    use gstreamer::{Pipeline, State};

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
        const HEADER_LEN: usize = 7;
        const PROFILE_AAC_LC: u8 = 1;
        const FREQ_IDX_16K: u8 = 8;
        const CHANNELS_MONO: u8 = 1;

        let len = HEADER_LEN + payload_len;
        let mut frame = vec![
            0xFF,
            // MPEG-4, layer 00, no CRC.
            0xF1,
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
    #[test]
    fn aac_latm_pipeline_negotiates_mp4a_latm() {
        if !require(&["appsrc", "queue", "aacparse", "capsfilter", "rtpmp4apay"]) {
            return;
        }

        let pipeline = Pipeline::new();
        let bin = pipeline.clone().upcast::<Element>();
        let config = test_stream_config(AudioFormat::Latm);

        let appsrc = build_aac(&bin, true, &config).expect("LATM pipeline should build");

        let payloader = pipeline.by_name("pay1").expect("pay1 should exist");
        assert_eq!(
            payloader.factory().map(|f| f.name().to_string()).as_deref(),
            Some("rtpmp4apay"),
            "LATM should be payloaded by rtpmp4apay"
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
            panic!("LATM pipeline errored: {:?}", err.error());
        }

        let caps = payloader
            .static_pad("src")
            .unwrap()
            .current_caps()
            .expect("payloader should have negotiated caps");
        let s = caps.structure(0).unwrap();

        assert_eq!(s.name(), "application/x-rtp");
        assert_eq!(s.get::<String>("media").unwrap(), "audio");
        assert_eq!(s.get::<String>("encoding-name").unwrap(), "MP4A-LATM");
        // The clock rate and `config` are what let a client decode the
        // passed-through frames; without them the SDP is unusable.
        assert_eq!(s.get::<i32>("clock-rate").unwrap(), 16_000);
        assert!(
            !s.get::<String>("config").unwrap().is_empty(),
            "SDP needs the AudioSpecificConfig"
        );

        pipeline.set_state(State::Null).unwrap();
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

        build_aac(&bin, true, &config).expect("PCM pipeline should build");

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
