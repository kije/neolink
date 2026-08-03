//! G.711 A-law encoding.
//!
//! A-law is the one audio codec that go2rtc can hand to a WebRTC consumer
//! without transcoding anything (`WithResampling`, `pkg/webrtc/helpers.go`
//! appends a wildcard `PCMA/0` so any clock rate matches). It is also
//! about as cheap as compression gets — a shift and a table lookup per
//! sample, 8 bits out for every 16 in — which is what makes it worth
//! doing here rather than leaving the audio as L16 and paying four times
//! the bandwidth for a stream that will be resampled at the far end
//! regardless.

/// Encode 16-bit PCM samples as G.711 A-law.
pub(crate) fn from_pcm(samples: &[i16]) -> Vec<u8> {
    samples.iter().copied().map(encode_sample).collect()
}

/// Upper bound of each A-law segment, on the 13-bit scale.
///
/// A-law is a piecewise-linear approximation of a logarithmic curve. The
/// segment a magnitude falls in becomes the three exponent bits, and its
/// position within that segment the four mantissa bits.
const SEGMENT_ENDS: [u16; 8] = [0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF];

/// Encode one sample, following G.711's `linear2alaw`.
///
/// The `^ mask` at the end does two jobs at once: it carries the sign
/// (the mask differs between the two cases) and it applies the
/// alternate-bit inversion the standard specifies, which keeps a long run
/// of silence from becoming a long run of identical bytes on the wire.
fn encode_sample(sample: i16) -> u8 {
    // A-law quantises 13 bits, so drop the bottom three.
    let mut pcm = sample >> 3;

    // Fold the negative half onto the positive one. This is `-pcm - 1`
    // rather than a negation so that the most negative input, which has
    // no positive counterpart, lands in range instead of overflowing.
    let mask = if pcm >= 0 {
        0xD5
    } else {
        pcm = -pcm - 1;
        0x55
    };
    let pcm = pcm as u16;

    let Some(segment) = SEGMENT_ENDS.iter().position(|&end| pcm <= end) else {
        // Out of range; the largest magnitude the format can express.
        return 0x7F ^ mask;
    };

    // The bottom two segments share a slope, so both shift by one.
    let shift = if segment < 2 { 1 } else { segment };
    let mantissa = (pcm >> shift) as u8 & 0x0F;

    ((segment as u8) << 4 | mantissa) ^ mask
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode A-law back to PCM, so the encoder can be checked against
    /// something other than itself. This is G.711's `alaw2linear`, which
    /// rebuilds each magnitude at the middle of its quantisation step.
    fn decode_sample(byte: u8) -> i16 {
        let byte = byte ^ 0x55;
        let mut value = ((byte & 0x0F) as i32) << 4;
        let segment = (byte & 0x70) >> 4;

        match segment {
            0 => value += 8,
            1 => value += 0x108,
            _ => {
                value += 0x108;
                value <<= segment - 1;
            }
        }

        if byte & 0x80 != 0 {
            value as i16
        } else {
            -value as i16
        }
    }

    #[test]
    fn one_byte_out_per_sample_in() {
        assert_eq!(from_pcm(&[0; 160]).len(), 160);
        assert_eq!(from_pcm(&[]).len(), 0);
    }

    #[test]
    fn silence_encodes_to_the_standard_idle_byte() {
        // Zero maps to 0x00, which the alternate-bit inversion turns into
        // 0xD5 — the byte a G.711 line idles on.
        assert_eq!(from_pcm(&[0]), vec![0xD5]);
    }

    #[test]
    fn the_sign_bit_tracks_the_sample_sign() {
        for &sample in &[1i16, 1000, 16000, i16::MAX] {
            let positive = from_pcm(&[sample])[0] ^ 0x55;
            let negative = from_pcm(&[-sample])[0] ^ 0x55;
            assert_eq!(positive & 0x80, 0x80, "{sample} should encode as positive");
            assert_eq!(negative & 0x80, 0x00, "{sample} should encode as negative");
            assert_eq!(
                positive & 0x7F,
                negative & 0x7F,
                "magnitude should not depend on sign for {sample}"
            );
        }
    }

    #[test]
    fn the_extremes_do_not_overflow() {
        // i16::MIN has no positive counterpart; folding with `-x - 1`
        // rather than a negation is what keeps this from overflowing.
        let encoded = from_pcm(&[i16::MIN, i16::MAX]);
        assert_eq!(encoded.len(), 2);
        assert_eq!(encoded[0] ^ 0x55, 0x7F, "i16::MIN should be full negative");
        assert_eq!(encoded[1] ^ 0x55, 0xFF, "i16::MAX should be full positive");
    }

    #[test]
    fn a_round_trip_stays_within_the_quantisation_step() {
        // A-law is lossy by design, but the error has to stay inside the
        // segment's step size — about 1/2048 of full scale at the bottom
        // and proportionally more at the top. Checking relative error
        // catches an exponent picked one segment out.
        for sample in (i16::MIN as i32..=i16::MAX as i32).step_by(7) {
            let sample = sample as i16;
            let decoded = decode_sample(from_pcm(&[sample])[0]) as i32;
            let error = (decoded - sample as i32).abs();
            let allowed = 16 + (sample as i32).abs() / 16;
            assert!(
                error <= allowed,
                "{} decoded to {}, off by {} (allowed {})",
                sample,
                decoded,
                error,
                allowed
            );
        }
    }

    #[test]
    fn the_encoding_is_monotonic() {
        // Louder in must never mean quieter out, which is the property a
        // mis-sized mantissa shift tends to break.
        let mut previous = i32::MIN;
        for sample in (0..=i16::MAX as i32).step_by(11) {
            let decoded = decode_sample(from_pcm(&[sample as i16])[0]) as i32;
            assert!(
                decoded >= previous,
                "output went backwards at {}: {} after {}",
                sample,
                decoded,
                previous
            );
            previous = decoded;
        }
    }
}
