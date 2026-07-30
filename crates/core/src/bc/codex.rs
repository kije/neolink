//! Create a tokio encoder/decoder for turning a AsyncRead/Write stream into
//! a Bc packet
//!
//! BcCodex is used with a `[tokio_util::codec::Framed]` to form complete packets
//!
use crate::bc::model::*;
use crate::bc::xml::*;
use crate::{Credentials, Error, Result};
use bytes::BytesMut;
use nom::AsBytes;
use tokio_util::codec::{Decoder, Encoder};

pub(crate) struct BcCodex {
    context: BcContext,
}

impl BcCodex {
    pub(crate) fn new_with_debug(credentials: Credentials) -> Self {
        let mut context = BcContext::new(credentials);

        context.debug_on();
        Self { context }
    }
    pub(crate) fn new(credentials: Credentials) -> Self {
        Self {
            context: BcContext::new(credentials),
        }
    }
}

impl Encoder<Bc> for BcCodex {
    type Error = Error;

    fn encode(&mut self, item: Bc, dst: &mut BytesMut) -> Result<()> {
        // let context = self.context.read().unwrap();
        const BC_ENCRYPTED: EncryptionProtocol = EncryptionProtocol::BCEncrypt;
        let buf: Vec<u8> = Default::default();
        let enc_protocol: &EncryptionProtocol = match self.context.get_encrypted() {
            EncryptionProtocol::Aes { .. } | EncryptionProtocol::FullAes { .. }
                if item.meta.msg_id == 1 =>
            {
                // During login the encyption protocol cannot go higher than BCEncrypt
                // even if we support AES. (BUt it can go lower i.e. None)
                &BC_ENCRYPTED
            }
            n => n,
        };
        let buf = item.serialize(buf, enc_protocol)?;
        dst.extend_from_slice(buf.as_slice());
        Ok(())
    }
}

impl Decoder for BcCodex {
    type Item = Bc;
    type Error = Error;

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>> {
        match self.decode(buf)? {
            Some(frame) => Ok(Some(frame)),
            None => {
                if buf.is_empty() {
                    Ok(None)
                } else {
                    log::debug!(
                        "bytes remaining on BC stream: {:X?}",
                        buf.as_bytes().chunks(25).next()
                    );
                    // Right after this we seem to get an issue with the camera dropping us
                    // Needs probing
                    // F0, DE, BC, A, 3, 0, 0, 0, 88, 6, 0, 0, 0, 1, 4, 0, C8, 0, 0, 0, 0, 0, 0, 0, 30, 31, 64, 63, 48,
                    // 32, 36, 34, 6A, 6, 0, 0, 0, 0, 0, 0, D8, F5, C7, 86, 56, 0, 0, 0, 0, 0, 0, 1, 21, 9A, FC, 22, 7F, 6, AE, F6, 15, FF, E5, 71, 4, 2F, 24, 61, 15, 96, F0, BF, 83, DE, 10, BE, B4, 2E, 3
                    // 9, 76, 56, 92, 7E, 48, 79, 20, 9A, DC, 1B, BB, AC, 22, 60, 5C, 72, B5, 3D, 8, E0, 34, 43, 3F, 2E, A7, 81, A8, 11, 75, 7F, 58, 3E, 8, 54, 91, 43, 21, EC, 6B, D6, 1A, D5, CB, D5, 6C,
                    // 8C, 2E, 6E, A3, 51, C3, A4, F0, CF, 2B, 61, 81, D0, 1C, A1, 76, EE, BF, 7A, D5, D8, D1, C4, D, B0, 45, EE, 3E, 93, 9A, CE, 5F, AB, 75, 55, AC, 9D, 66, DE, 23, 6D, 5F, 25, 57, DA, F5
                    //, E, 7F, 8D, 30, A7, 66, C4, 60, 76, 41, D0, 6A, 23, E, A9, C5, 51, EE, F6, DD, 19, E7, A8, 96, 9F, 2B, AF, 31, 90, 9D, FC, BE
                    Ok(None)
                }
            }
        }
        // match self.decode(buf)? {
        //     Some(frame) => Ok(Some(frame)),
        //     None => Ok(None),
        // }
    }

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        // trace!("Decoding: {:X?}", src);
        let bc = Bc::deserialize(&self.context, src);
        // trace!("As: {:?}", bc);
        let bc = match bc {
            Ok(bc) => bc,
            Err(Error::NomIncomplete(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        // Update context
        if let Bc {
            meta:
                BcMeta {
                    msg_id: 1,
                    response_code,
                    ..
                },
            body:
                BcBody::ModernMsg(ModernMsg {
                    payload:
                        Some(BcPayloads::BcXml(BcXml {
                            encryption: Some(Encryption { nonce, .. }),
                            ..
                        })),
                    ..
                }),
        } = &bc
        {
            if response_code >> 8 == 0xdd {
                // Login reply has the encryption info
                // Set that the encryption type now
                let encryption_protocol_byte = (response_code & 0xff) as usize;
                match encryption_protocol_byte {
                    0x00 => self.context.set_encrypted(EncryptionProtocol::Unencrypted),
                    0x01 => self.context.set_encrypted(EncryptionProtocol::BCEncrypt),
                    0x02 => self.context.set_encrypted(EncryptionProtocol::aes(
                        self.context.credentials.make_aeskey(nonce),
                    )),
                    0x12 => self.context.set_encrypted(EncryptionProtocol::full_aes(
                        self.context.credentials.make_aeskey(nonce),
                    )),
                    _ => {
                        return Err(Error::UnknownEncryption(encryption_protocol_byte));
                    }
                }
            }
        }

        if let BcBody::ModernMsg(ModernMsg {
            extension:
                Some(Extension {
                    binary_data: Some(on_off),
                    ..
                }),
            ..
        }) = bc.body
        {
            if on_off == 0 {
                self.context.binary_off(bc.meta.msg_num);
            } else {
                self.context.binary_on(bc.meta.msg_num);
            }
        }

        Ok(Some(bc))
    }
}

/// The `Err::Incomplete` boundary.
///
/// The nom parsers are well covered on the happy path -- 31 of this crate's
/// tests exercise them against captured fixtures -- but every one of those
/// concatenates its fixture into a single complete buffer before parsing. That
/// leaves the branch this codec is built around, `Err(Error::NomIncomplete(_))
/// => return Ok(None)` at `decode`, with no test at all.
///
/// It is not an edge case. It is the normal case: a BC frame arrives split
/// across however many TCP segments the network felt like, and `Framed` calls
/// `decode` after each one. If the parsers ever stopped reporting `Incomplete`
/// on a short buffer -- which is exactly what switching nom from its streaming
/// parsers to its complete ones would do -- a partial frame would surface as a
/// parse *error* instead, and the connection would drop rather than wait for the
/// rest of the packet.
///
/// So: feed a real frame one byte at a time and assert `Ok(None)` every time
/// until the last byte, which must yield the whole message.
#[cfg(test)]
mod incomplete_tests {
    use super::*;

    /// The captured login fixtures are BCEncrypt, which is what a camera uses
    /// before the login reply negotiates anything stronger. `BcCodex::new`
    /// starts at `Unencrypted`, so build the context directly -- this module is
    /// a child of `codex`, so the private field is reachable.
    fn test_codex() -> BcCodex {
        BcCodex {
            context: BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt),
        }
    }

    /// Decode `sample` one byte at a time.
    ///
    /// Returns the number of bytes that had to arrive before a frame came out.
    fn decode_byte_at_a_time(sample: &[u8]) -> usize {
        let mut codex = test_codex();
        let mut buf = BytesMut::new();

        for (i, byte) in sample.iter().enumerate() {
            buf.extend_from_slice(&[*byte]);
            match codex.decode(&mut buf) {
                Ok(None) => {
                    assert!(
                        i + 1 < sample.len(),
                        "the whole frame arrived and still no message came out"
                    );
                }
                Ok(Some(_)) => return i + 1,
                Err(e) => panic!(
                    "byte {} of {} produced a hard error instead of Incomplete: {e:?}",
                    i + 1,
                    sample.len()
                ),
            }
        }
        panic!("never produced a frame");
    }

    #[test]
    fn partial_frames_report_incomplete_not_error() {
        let sample = include_bytes!("samples/modern_login_success.bin");
        let consumed = decode_byte_at_a_time(&sample[..]);
        assert_eq!(
            consumed,
            sample.len(),
            "frame emitted before all its bytes had arrived"
        );
    }

    #[test]
    fn partial_frames_report_incomplete_not_error_failed_login() {
        let sample = include_bytes!("samples/modern_login_failed.bin");
        let consumed = decode_byte_at_a_time(&sample[..]);
        assert_eq!(consumed, sample.len());
    }

    /// An empty buffer is the very first thing `Framed` hands us, and it must be
    /// `Incomplete` rather than an error.
    #[test]
    fn empty_buffer_is_incomplete() {
        let mut codex = test_codex();
        let mut buf = BytesMut::new();
        assert!(matches!(codex.decode(&mut buf), Ok(None)));
    }

    /// A frame delivered whole must be consumed exactly -- no bytes left behind,
    /// which is what lets a second frame in the same read be decoded after it.
    #[test]
    fn complete_frame_consumes_exactly_its_own_bytes() {
        let sample = &include_bytes!("samples/modern_login_success.bin")[..];
        let mut codex = test_codex();

        // Two frames back to back, as a single TCP read would deliver them.
        let mut buf = BytesMut::new();
        buf.extend_from_slice(sample);
        buf.extend_from_slice(sample);

        assert!(matches!(codex.decode(&mut buf), Ok(Some(_))));
        assert_eq!(
            buf.len(),
            sample.len(),
            "decoding one frame did not consume exactly one frame"
        );
        assert!(matches!(codex.decode(&mut buf), Ok(Some(_))));
        assert!(buf.is_empty(), "trailing bytes left after the second frame");
    }
}
