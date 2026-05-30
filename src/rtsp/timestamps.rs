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

/// Number of consecutive backward (`fwd >= BACKWARD_THRESHOLD`) frames
/// at which we stop treating the situation as a transient reorder and
/// instead accept the new μs value as a fresh baseline (camera restart).
/// One backward frame is treated as a reorder blip; two in a row is
/// the camera firmly telling us its clock has reset.
const RESTART_DETECT_COUNT: u32 = 2;

/// Per-stream timestamp state. One tracker per client/stream pipeline.
pub(super) struct TimestampTracker {
    last_camera_us_32: Option<u32>,
    last_video_us: u64,
    last_audio_us: u64,
    audio_anchored: bool,
    consecutive_backward: u32,
}

impl TimestampTracker {
    pub(super) fn new() -> Self {
        Self {
            last_camera_us_32: None,
            last_video_us: 0,
            last_audio_us: 0,
            audio_anchored: false,
            consecutive_backward: 0,
        }
    }

    /// Translate a 32-bit camera μs value into our monotonic 64-bit PTS.
    ///
    /// Normalises the first observed value to PTS 0 and handles:
    /// * natural u32 wrap every ~71.58 min (small forward wrapping delta),
    /// * out-of-order or duplicate frames — advances by 1 μs without
    ///   updating the baseline μs, so a subsequent in-order frame's
    ///   delta is still computed against the last *good* timestamp,
    /// * suspected camera restart — `RESTART_DETECT_COUNT` consecutive
    ///   backward frames, or a single forward jump above
    ///   `MAX_FORWARD_JUMP_US`, accept the new μs as a fresh baseline
    ///   and clamp the PTS advance to `RESTART_ADVANCE_US`.
    pub(super) fn next_video_us(&mut self, camera_us_32: u32) -> u64 {
        let Some(last_32) = self.last_camera_us_32 else {
            self.last_camera_us_32 = Some(camera_us_32);
            return 0;
        };

        let fwd = camera_us_32.wrapping_sub(last_32);

        if fwd == 0 {
            // Duplicate timestamp: keep baseline intact so a real
            // forward step later computes the correct delta.
            self.last_video_us = self.last_video_us.saturating_add(TIE_BREAKER_US);
            return self.last_video_us;
        }

        if fwd >= BACKWARD_THRESHOLD {
            self.consecutive_backward += 1;
            if self.consecutive_backward >= RESTART_DETECT_COUNT {
                log::warn!(
                    "Camera timestamp restart detected (last={:#x} cur={:#x}); \
                     accepting new baseline and clamping PTS advance to {}μs",
                    last_32,
                    camera_us_32,
                    RESTART_ADVANCE_US
                );
                self.last_camera_us_32 = Some(camera_us_32);
                self.consecutive_backward = 0;
                self.last_video_us = self.last_video_us.saturating_add(RESTART_ADVANCE_US);
                return self.last_video_us;
            }
            log::trace!(
                "Camera μs went backward (last={:#x} cur={:#x}); treating as reorder",
                last_32,
                camera_us_32
            );
            self.last_video_us = self.last_video_us.saturating_add(TIE_BREAKER_US);
            return self.last_video_us;
        }

        // Forward delta within an acceptable range or a huge stall.
        self.consecutive_backward = 0;
        let advance = if fwd <= MAX_FORWARD_JUMP_US {
            fwd as u64
        } else {
            log::warn!(
                "Large camera μs forward jump (last={:#x} cur={:#x} fwd={}μs); \
                 clamping advance to {}μs (suspected stall/restart)",
                last_32,
                camera_us_32,
                fwd,
                RESTART_ADVANCE_US
            );
            RESTART_ADVANCE_US
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
    ///
    /// `duration_us` is clamped to at least 1 μs so a malformed audio
    /// frame reporting zero duration cannot produce equal back-to-back
    /// PTS values.
    pub(super) fn next_audio_us(&mut self, duration_us: u32) -> u64 {
        if !self.audio_anchored {
            self.last_audio_us = self.last_video_us;
            self.audio_anchored = true;
        }
        let ts = self.last_audio_us;
        let advance = (duration_us as u64).max(TIE_BREAKER_US);
        self.last_audio_us = self.last_audio_us.saturating_add(advance);
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
        // Coarse step (1s/frame) so we cross the wrap boundary multiple
        // times in ~13k iterations rather than 1.28M.
        let mut t = TimestampTracker::new();
        let step = 1_000_000u32;
        let mut camera_us = 0u32;
        let mut last_pts = t.next_video_us(camera_us);
        let iters = (u32::MAX / step) as u64 * 3;
        for _ in 0..iters {
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
    fn transient_backward_jump_does_not_poison_baseline() {
        // Out-of-order delivery (would not happen over TCP, but defensive).
        // The stale ts must not become the baseline, otherwise the next
        // in-order frame computes an inflated delta.
        let mut t = TimestampTracker::new();
        assert_eq!(t.next_video_us(1_000_000), 0);
        assert_eq!(t.next_video_us(1_033_333), 33_333);
        // Frame arrives "late" with smaller μs than the previous frame.
        let pts = t.next_video_us(1_020_000);
        assert_eq!(pts, 33_333 + TIE_BREAKER_US);
        // Next in-order frame's delta is computed from 1_033_333 (the
        // last *good* timestamp), not from the stale 1_020_000.
        let pts2 = t.next_video_us(1_066_666);
        assert_eq!(pts2, pts + 33_333);
    }

    #[test]
    fn two_consecutive_backward_frames_trigger_restart() {
        let mut t = TimestampTracker::new();
        assert_eq!(t.next_video_us(0x5000_0000), 0);
        let _ = t.next_video_us(0x5000_8235); // PTS 33_333
        // 1st backward: transient reorder, nudge by 1μs.
        let pts1 = t.next_video_us(0x100);
        assert_eq!(pts1, 33_333 + TIE_BREAKER_US);
        // 2nd consecutive backward: restart accepted, clamp advance.
        let pts2 = t.next_video_us(0x200);
        assert_eq!(pts2, pts1 + RESTART_ADVANCE_US);
        // After restart, the new μs is the baseline; normal step works.
        let pts3 = t.next_video_us(0x200 + 33_333);
        assert_eq!(pts3, pts2 + 33_333);
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
    fn restart_after_long_run_does_not_skip_71_minutes() {
        // Stream has been running far past the wrap region, then camera
        // restarts and reports μs near 0. Without restart detection the
        // wrapping delta would push PTS forward by ~71 min.
        let mut t = TimestampTracker::new();
        assert_eq!(t.next_video_us(0x5000_0000), 0);
        let _ = t.next_video_us(0x5000_8235); // +33333 μs (≈30 fps)
        // 1st post-restart frame looks like a reorder; 2nd confirms restart.
        let pts1 = t.next_video_us(0x100);
        let pts2 = t.next_video_us(0x200);
        let pts3 = t.next_video_us(0x200 + 33_333);
        // Total advance for the three post-restart frames is bounded by
        // (TIE_BREAKER + RESTART_ADVANCE + 33_333), nowhere near 71 min.
        assert_eq!(pts1, 33_333 + TIE_BREAKER_US);
        assert_eq!(pts2, pts1 + RESTART_ADVANCE_US);
        assert_eq!(pts3, pts2 + 33_333);
        assert!(
            pts3 < 33_333 + TIE_BREAKER_US + RESTART_ADVANCE_US + 1_000_000,
            "PTS jumped suspiciously far after restart: {}",
            pts3
        );
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

    #[test]
    fn audio_zero_duration_still_advances() {
        // A malformed audio frame reporting zero duration must not
        // produce equal back-to-back PTS values.
        let mut t = TimestampTracker::new();
        assert_eq!(t.next_audio_us(0), 0);
        assert_eq!(t.next_audio_us(0), TIE_BREAKER_US);
        assert_eq!(t.next_audio_us(20_000), 2 * TIE_BREAKER_US);
        assert_eq!(t.next_audio_us(0), 2 * TIE_BREAKER_US + 20_000);
    }
}
