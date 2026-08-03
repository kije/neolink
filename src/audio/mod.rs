//! Audio codec helpers that do not need GStreamer.
//!
//! Reolink cameras send one of two things: AAC in ADTS framing, which is
//! already what a consumer wants and travels untouched, or IMA ADPCM,
//! which is not. [`adpcm`] turns the latter into PCM and [`alaw`] turns
//! PCM into G.711, the pair being what lets [`crate::stream`] offer real
//! audio without linking a decoder.

pub(crate) mod adpcm;
pub(crate) mod alaw;

/// Sample rate of the ADPCM audio Reolink cameras send, in Hz.
///
/// Fixed by the camera rather than negotiated — `BcMediaAdpcm::duration`
/// hard-codes the same value to work out how long a block lasts.
pub(crate) const ADPCM_SAMPLE_RATE: u32 = 8_000;
