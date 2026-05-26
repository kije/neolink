//! Camera-to-RTSP timestamp forwarding.
//!
//! The Baichuan media protocol carries a 32-bit microsecond capture
//! timestamp on every I- and P-frame (`BcMediaIframe::microseconds` /
//! `BcMediaPframe::microseconds`). [`TimestampTracker`] converts that
//! 32-bit value into a monotonic 64-bit microsecond PTS suitable for use
//! on the GStreamer buffers fed into the RTSP `appsrc`.
//!
//! Forwarding the camera's capture clock (instead of synthesising PTS from
//! the configured FPS) gives consumers a drift-free media timeline that is
//! immune to network jitter between the camera and neolink, which in turn
//! makes the RTCP Sender Reports emitted by `rtspserver`/`rtpbin` map
//! accurate RTP↔NTP pairings — the standard mechanism RTSP clients use to
//! align multiple streams onto a shared wall-clock timeline.
//!
//! Audio frames in the BC stream do not carry their own timestamps. We
//! anchor the first audio frame's PTS to the current video PTS so that
//! audio and video from one camera share a clock domain, then advance the
//! audio clock by the per-frame duration (parsed from AAC ADTS / ADPCM
//! block size by `BcMediaAac::duration` / `BcMediaAdpcm::duration`).

/// Forward direction wrapping deltas larger than this threshold are
/// treated as a backward jump (out-of-order arrival). 2^31 μs is roughly
/// 35.79 minutes — far larger than any reorder we would actually see, and
/// half the u32 wrap distance so that natural wraps land on the forward
/// side.
const BACKWARD_THRESHOLD: u32 = 0x8000_0000;

/// Forward delta larger than this is treated as a suspected camera
/// restart / huge stall and clamped. Chosen well above any realistic
/// frame gap (a 1 fps low-bitrate stream still has 1 s gaps) but below
/// timescales where a real gap would matter for sync.
const MAX_FORWARD_JUMP_US: u32 = 5_000_000;

/// PTS advance applied when we clamp a suspected discontinuity. Keeps the
/// stream monotonic without skipping the player forward by minutes.
const RESTART_ADVANCE_US: u64 = 100_000;

/// PTS advance used for duplicate / reordered frames so the timeline
/// stays strictly monotonic (GStreamer dislikes equal/decreasing PTS).
const TIE_BREAKER_US: u64 = 1;

/// Per-stream timestamp state. One tracker per client/stream pipeline.
pub(super) struct TimestampTracker {
    last_camera_us_32: Option<u32>,
    last_video_us: u64,
    last_audio_us: u64,
    audio_anchored: bool,
}

impl TimestampTracker {
    pub(super) fn new() -> Self {
        Self {
            last_camera_us_32: None,
            last_video_us: 0,
            last_audio_us: 0,
            audio_anchored: false,
        }
    }

    /// Translate a 32-bit camera μs value into our monotonic 64-bit PTS.
    ///
    /// Normalises the first observed value to PTS 0 and handles:
    /// * natural u32 wrap every ~71.58 min (small forward wrapping delta),
    /// * out-of-order or duplicate frames (wrapping delta near or past
    ///   `BACKWARD_THRESHOLD` — advances by 1 μs to stay monotonic),
    /// * suspected camera restart / huge gap (wrapping delta exceeds
    ///   `MAX_FORWARD_JUMP_US` — clamped to `RESTART_ADVANCE_US`).
    pub(super) fn next_video_us(&mut self, camera_us_32: u32) -> u64 {
        let advance = match self.last_camera_us_32 {
            None => {
                self.last_camera_us_32 = Some(camera_us_32);
                return 0;
            }
            Some(last_32) => {
                let fwd = camera_us_32.wrapping_sub(last_32);
                if fwd == 0 {
                    TIE_BREAKER_US
                } else if fwd >= BACKWARD_THRESHOLD {
                    log::trace!(
                        "Camera μs went backward (last={:#x} cur={:#x}); treating as reorder",
                        last_32,
                        camera_us_32
                    );
                    TIE_BREAKER_US
                } else if fwd <= MAX_FORWARD_JUMP_US {
                    fwd as u64
                } else {
                    log::warn!(
                        "Large camera μs jump (last={:#x} cur={:#x} fwd={}μs); \
                         clamping advance to {}μs (suspected restart)",
                        last_32,
                        camera_us_32,
                        fwd,
                        RESTART_ADVANCE_US
                    );
                    RESTART_ADVANCE_US
                }
            }
        };

        self.last_camera_us_32 = Some(camera_us_32);
        self.last_video_us = self.last_video_us.saturating_add(advance);
        self.last_video_us
    }

    /// Get the PTS for the next audio frame and advance the audio clock.
    ///
    /// The first call anchors the audio clock to the current video PTS so
    /// A/V share a clock domain. If audio arrives before any video has
    /// been seen, both start at 0 and meet at the first video frame.
    pub(super) fn next_audio_us(&mut self, duration_us: u32) -> u64 {
        if !self.audio_anchored {
            self.last_audio_us = self.last_video_us;
            self.audio_anchored = true;
        }
        let ts = self.last_audio_us;
        self.last_audio_us = self.last_audio_us.saturating_add(duration_us as u64);
        ts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_video_frame_is_zero() {
        let mut t = TimestampTracker::new();
        assert_eq!(t.next_video_us(123_456_789), 0);
    }

    #[test]
    fn sequential_frames_advance_by_camera_delta() {
        let mut t = TimestampTracker::new();
        assert_eq!(t.next_video_us(1_000_000), 0);
        assert_eq!(t.next_video_us(1_033_333), 33_333);
        assert_eq!(t.next_video_us(1_066_666), 66_666);
        assert_eq!(t.next_video_us(1_100_000), 100_000);
    }

    #[test]
    fn natural_u32_wrap_is_handled() {
        let mut t = TimestampTracker::new();
        // Anchor 256 μs before the u32 wrap point.
        let near_wrap = u32::MAX - 255;
        assert_eq!(t.next_video_us(near_wrap), 0);
        // Next frame 512 μs later straddles the wrap: u32 value is small.
        let after_wrap = 256u32; // wraps around: near_wrap + 512 == 256
        let pts = t.next_video_us(after_wrap);
        assert_eq!(pts, 512, "expected smooth 512μs advance across u32 wrap");
    }

    #[test]
    fn many_wraps_in_a_row_stay_monotonic() {
        let mut t = TimestampTracker::new();
        let step = 33_333u32; // 30 fps
        let mut camera_us = 0u32;
        let mut last_pts = t.next_video_us(camera_us);
        // Run ~10 full wraps worth of frames.
        for _ in 0..((u32::MAX / step) as u64 * 10) {
            camera_us = camera_us.wrapping_add(step);
            let pts = t.next_video_us(camera_us);
            assert!(
                pts > last_pts,
                "PTS not monotonic: prev={} next={}",
                last_pts,
                pts
            );
            assert_eq!(pts - last_pts, step as u64);
            last_pts = pts;
        }
    }

    #[test]
    fn duplicate_microseconds_advance_by_one() {
        let mut t = TimestampTracker::new();
        assert_eq!(t.next_video_us(5_000), 0);
        assert_eq!(t.next_video_us(5_000), TIE_BREAKER_US);
        assert_eq!(t.next_video_us(5_000), 2 * TIE_BREAKER_US);
    }

    #[test]
    fn small_backward_jump_advances_by_one() {
        // Out-of-order delivery (would not happen over TCP, but defensive).
        let mut t = TimestampTracker::new();
        assert_eq!(t.next_video_us(1_000_000), 0);
        assert_eq!(t.next_video_us(1_033_333), 33_333);
        // Frame arrives "late" with smaller μs than the previous frame.
        let pts = t.next_video_us(1_020_000);
        assert_eq!(pts, 33_333 + TIE_BREAKER_US);
        // Subsequent in-order frame resumes forward progress relative to
        // the most recently seen camera μs.
        let pts2 = t.next_video_us(1_066_666);
        // fwd = 1_066_666 - 1_020_000 = 46_666 (advance from prior PTS)
        assert_eq!(pts2, pts + 46_666);
    }

    #[test]
    fn large_forward_jump_is_clamped() {
        let mut t = TimestampTracker::new();
        assert_eq!(t.next_video_us(1_000_000), 0);
        // 10s forward jump - exceeds MAX_FORWARD_JUMP_US.
        let pts = t.next_video_us(11_000_000);
        assert_eq!(pts, RESTART_ADVANCE_US);
        // Subsequent normal frame continues from the clamped baseline.
        let pts2 = t.next_video_us(11_033_333);
        assert_eq!(pts2, RESTART_ADVANCE_US + 33_333);
    }

    #[test]
    fn suspected_restart_to_zero_does_not_skip_71_minutes() {
        // Stream has been running far past the wrap region, then camera
        // restarts and reports μs near 0 — the wrapping delta would put
        // PTS forward by ~71 min if we naively treated it as a wrap.
        let mut t = TimestampTracker::new();
        assert_eq!(t.next_video_us(0x5000_0000), 0);
        // Sanity: take a normal step forward first.
        let _ = t.next_video_us(0x5000_8235); // +33333 μs ≈ 30 fps
        let baseline = t.last_video_us;
        // "Restart": camera μs jumps back to a small value. The wrapping
        // forward delta from 0x5000_8235 to 0x100 is huge (≈2.95 billion)
        // which is well above BACKWARD_THRESHOLD — we treat it as reorder
        // and only advance by TIE_BREAKER_US.
        let pts = t.next_video_us(0x100);
        assert_eq!(pts, baseline + TIE_BREAKER_US);
        // Subsequent frames after restart at small μs values look like
        // big forward jumps from the previous *recorded* camera μs (0x100),
        // and continue advancing normally.
        let pts2 = t.next_video_us(0x100 + 33_333);
        assert_eq!(pts2, pts + 33_333);
    }

    #[test]
    fn audio_anchors_to_video_clock() {
        let mut t = TimestampTracker::new();
        let _ = t.next_video_us(1_000_000); // PTS 0
        let _ = t.next_video_us(1_033_333); // PTS 33_333
        // First audio frame anchors to current video PTS.
        assert_eq!(t.next_audio_us(20_000), 33_333);
        assert_eq!(t.next_audio_us(20_000), 53_333);
        // Video continues independently in the same domain.
        assert_eq!(t.next_video_us(1_066_666), 66_666);
    }

    #[test]
    fn audio_before_video_starts_at_zero() {
        let mut t = TimestampTracker::new();
        // No video yet; audio anchors to last_video_us == 0.
        assert_eq!(t.next_audio_us(20_000), 0);
        assert_eq!(t.next_audio_us(20_000), 20_000);
        // First video frame still starts at 0; both clocks meet at the
        // start of the stream.
        assert_eq!(t.next_video_us(1_000_000), 0);
    }

    #[test]
    fn audio_does_not_re_anchor_after_first_frame() {
        let mut t = TimestampTracker::new();
        // First audio frame at 0.
        assert_eq!(t.next_audio_us(20_000), 0);
        // Video advances later — audio must keep its own clock, not jump.
        let _ = t.next_video_us(1_000_000);
        let _ = t.next_video_us(2_000_000); // PTS = 1_000_000
        assert_eq!(t.next_audio_us(20_000), 20_000);
    }
}
