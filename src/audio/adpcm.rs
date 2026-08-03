//! A Rust implementation of OKI and DVI/IMA ADPCM.
//!
//! Reolink cameras that predate AAC send audio as IMA ADPCM, which
//! nothing downstream of neolink wants in that form. Decoding it here
//! costs almost nothing — it is a table lookup and an add per sample —
//! and leaves [`crate::audio::alaw`] a plain PCM stream to work from.

use anyhow::{bail, Result};

struct AdpcmSetup {
    max_step_index: u32,
    steps: &'static [u32],
    max_sample_size: i32,
    changes: &'static [i32],
}

impl AdpcmSetup {
    // Unused, originally we thought BC might be using OKI but it is actually DVI4
    #[allow(dead_code)]
    fn new_oki() -> Self {
        Self {
            max_step_index: 48,
            steps: &[
                16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73, 80, 88, 97,
                107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
                494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552,
            ],
            changes: &[-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8],
            max_sample_size: 2048,
        }
    }

    // This is IMA format, but it is the same as DVI4 format except in the block header
    fn new_ima() -> Self {
        Self {
            max_step_index: 88,
            steps: &[
                7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50,
                55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279,
                307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282,
                1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871,
                5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899, 15289, 16818,
                18500, 20350, 22385, 24623, 27086, 29794, 32767,
            ],
            changes: &[-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8],
            max_sample_size: 32768,
        }
    }
}

struct Nibble {
    // A nibble is a 4bit int
    data: u8, // This is the raw data for the nibble
}

impl Nibble {
    // Use u/i32 throughout to ensure that we always have enough
    // Headroom to do the math without needing `as` casting everywhere
    fn unsigned(&self) -> u32 {
        (self.data & 0b00001111) as u32 // Mask first 4 bits it just to be sure its in nibble range
    }

    #[allow(dead_code)]
    fn signed_magnitude(&self) -> u32 {
        (self.data & 0b00000111) as u32 // Mask of first 3 bits which are the magnitiude bits in signed int
    }

    #[allow(dead_code)]
    fn signed(&self) -> i32 {
        match self.data & 0b00001000 {
            // Sign bit is at the 4th bit
            0b00001000 => -(self.signed_magnitude() as i32),
            _ => self.signed_magnitude() as i32,
        }
    }

    fn from_byte(byte: &u8) -> [Self; 2] {
        // Two nibbles per byte
        [
            Self {
                data: (byte & 0b11110000) >> 4,
            },
            Self {
                data: byte & 0b00001111,
            },
        ]
    }
}

/// Bytes of predictor state at the head of every block.
///
/// ADPCM is not really a streamable format: each sample is expressed as a
/// delta from the one before, so a decoder joining mid-stream has nothing
/// to start from. Reolink solves that by caching the encoder's
/// intermediate state — the last output sample and the step index — in
/// front of every block, which is what these four bytes are.
const BLOCK_HEADER_SIZE: usize = 4;

/// Decode one ADPCM block into 16-bit PCM samples.
///
/// `block` is the payload of a [`neolink_core::bcmedia::model::BcMediaAdpcm`]
/// exactly as the deserialiser hands it over: two bytes of last output,
/// two bytes of step index, then the ADPCM nibbles. The frame magic and
/// the half-block-size field that precede it on the wire have already
/// been stripped by then, so they are not expected here.
pub(crate) fn decode(block: &[u8]) -> Result<Vec<i16>> {
    let context = AdpcmSetup::new_ima();

    if block.len() <= BLOCK_HEADER_SIZE {
        bail!(
            "ADPCM block of {} bytes is too short to hold its own predictor state",
            block.len()
        );
    }

    // Get predictor state from the block header, in DVI4 layout.
    let mut last_output = i16::from_le_bytes([block[0], block[1]]) as i32;
    let mut step_index = u16::from_le_bytes([block[2], block[3]]) as i32;

    // Use u/i32 throughout so the arithmetic has headroom without a cast
    // on every line; ADPCM clamps the values well inside the range.
    let mut step: u32;

    let data = &block[BLOCK_HEADER_SIZE..];
    let mut result: Vec<i16> = Vec::with_capacity(data.len() * 2);

    for byte in data {
        let nibbles: [Nibble; 2] = Nibble::from_byte(byte);
        for nibble in &nibbles {
            let unibble = nibble.unsigned();

            // Specifications say: clamp to 0..context.max_step_index
            step_index = match step_index {
                n if n < 0 => 0,
                n if n > context.max_step_index as i32 => context.max_step_index as i32,
                n => n,
            };

            // This is just Euler's approximation with a variable step size
            // **Adaptive** Differential PCM
            // Adaptive: because the step size is variable
            step = context.steps[step_index as usize];

            /* == Non approximate version ===
            // This is the full maths version
            // We don't use this one as we need to match the way the encoder
            // works if we want to use the state stored in the header.
            // I have left it here as it is easier to understand than the bit shift version below
            let inibble = nibble.signed();

            // Calculate the delta (which is really what adpcm is all about)
            // Adaptive **Differential** PCM
            // Differential: because it's all about the difference (gradient)
            let diff = (step as i32) * (inibble) / 2 + (step as i32) / 8;

            // Euler's approximation
            // Sample = Previous_Sample + difference*step_size
            let raw_sample = last_output + diff;
            */

            // === Approximate version ==
            // Approximate form uses bit shift operators.
            // This is a legacy of the days when mult/divides were expensive
            // It is also the format used on low end CPUs like cameras
            let mut diff = step >> 3;
            if (unibble & 0b0100) == 0b0100 {
                diff += step;
            }
            if (unibble & 0b0010) == 0b0010 {
                diff += step >> 1;
            }
            if (unibble & 0b0001) == 0b0001 {
                diff += step >> 2;
            }
            // Sign test
            let raw_sample = if (unibble & 0b1000) == 0b1000 {
                last_output - (diff as i32)
            } else {
                last_output + (diff as i32)
            };

            // Specifications say: clamp to
            // -context.max_sample_size..context.max_sample_size
            let sample = match raw_sample {
                value if value > context.max_sample_size - 1 => context.max_sample_size - 1,
                value if value < -context.max_sample_size => -context.max_sample_size,
                value => value,
            };

            // PCM is really in i16 range.
            // Some formats e.g. OKI are not in the full PCM range of values.
            // To convert we must scale it to the i16 range. For IMA the two
            // ranges already agree and this is an identity.
            let scaled_sample = (sample * (i16::MAX as i32) / (context.max_sample_size - 1)) as i16;

            result.push(scaled_sample);

            // Increment the step index
            step_index += context.changes[unibble as usize];

            // cache the last_output ready for next run
            last_output = sample;
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a block with the given predictor state and nibbles.
    fn block(last_output: i16, step_index: u16, nibbles: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&last_output.to_le_bytes());
        out.extend_from_slice(&step_index.to_le_bytes());
        out.extend_from_slice(nibbles);
        out
    }

    #[test]
    fn a_block_yields_two_samples_per_byte() {
        let decoded = decode(&block(0, 0, &[0x00; 122])).unwrap();
        assert_eq!(decoded.len(), 244);
    }

    #[test]
    fn a_block_without_a_full_header_is_rejected() {
        for short in 0..=BLOCK_HEADER_SIZE {
            assert!(
                decode(&vec![0u8; short]).is_err(),
                "a {}-byte block should not decode",
                short
            );
        }
    }

    #[test]
    fn silence_decodes_to_near_silence() {
        // Nibble 0 is the smallest positive delta, so from a zero
        // predictor the output should stay tiny rather than run away.
        let decoded = decode(&block(0, 0, &[0x00; 64])).unwrap();
        assert!(
            decoded.iter().all(|&s| s.abs() < 500),
            "expected near silence, got a peak of {}",
            decoded.iter().map(|s| s.abs()).max().unwrap()
        );
    }

    #[test]
    fn the_predictor_state_in_the_header_is_honoured() {
        // Decoding the same nibbles from a different starting sample must
        // move the output with it; this is the state that makes each
        // block independently decodable.
        let low = decode(&block(0, 10, &[0x11; 16])).unwrap();
        let high = decode(&block(8000, 10, &[0x11; 16])).unwrap();
        assert!(
            high[0] > low[0],
            "a higher last_output must raise the first sample: {} vs {}",
            high[0],
            low[0]
        );
    }

    #[test]
    fn the_step_index_is_clamped_to_the_table() {
        // A camera sending a bogus step index must not index out of
        // bounds; the clamp is what stops that being a panic.
        let decoded = decode(&block(0, 60_000, &[0xFF; 32])).unwrap();
        assert_eq!(decoded.len(), 64);
    }

    #[test]
    fn full_scale_nibbles_stay_inside_the_pcm_range() {
        // Nibble 0x7 is the largest positive delta. Repeated, it should
        // saturate at the clamp rather than wrap around to negative.
        let decoded = decode(&block(0, 88, &[0x77; 128])).unwrap();
        assert!(
            decoded.iter().all(|&s| s > 0),
            "a rising ramp must not wrap through zero"
        );
    }
}
