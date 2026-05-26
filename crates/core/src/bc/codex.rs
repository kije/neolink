//! Create a tokio encoder/decoder for turning a AsyncRead/Write stream into
//! a Bc packet
//!
//! BcCodex is used with a `[tokio_util::codec::Framed]` to form complete packets
//!
use crate::bc::model::*;
use crate::bc::xml::*;
use crate::{Credentials, Error, Result};
use bytes::{Buf, BytesMut};
use nom::AsBytes;
use tokio_util::codec::{Decoder, Encoder};

/// On-wire byte pattern for the reversed magic (`MAGIC_HEADER_REV` in LE).
/// Used by the reverse-magic recovery path.
pub(crate) const MAGIC_HEADER_REV_BYTES: [u8; 4] = [0xa0, 0xcb, 0xed, 0x0f];
/// On-wire byte pattern for the forward magic (`MAGIC_HEADER` in LE).
pub(crate) const MAGIC_HEADER_BYTES: [u8; 4] = [0xf0, 0xde, 0xbc, 0x0a];

/// If the buffer starts with the reversed-magic byte sequence and a forward
/// magic appears later in the same buffer, returns the number of bytes that
/// must be discarded to land on that forward magic. Otherwise returns `None`.
pub(crate) fn find_next_forward_magic_after_reverse(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 || buf[0..4] != MAGIC_HEADER_REV_BYTES {
        return None;
    }
    // Scan starting from byte 1 — the camera occasionally emits two reversed
    // magic markers back-to-back, so we want to land on the *forward* one.
    buf.windows(4)
        .enumerate()
        .skip(1)
        .find(|(_, w)| *w == MAGIC_HEADER_BYTES)
        .map(|(i, _)| i)
}

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
            Err(e) => {
                // Reverse-magic recovery.
                //
                // Some firmwares emit a stray reversed-magic byte sequence
                // (`0xa0 0xcb 0xed 0x0f` on the wire — `MAGIC_HEADER_REV` in
                // LE) after large pushes. The bogus "header" that follows can
                // either parse loosely (resulting in a parse error later) or
                // claim a body_len bigger than the rest of the stream (which
                // surfaces as `NomIncomplete`). In both cases the safe thing
                // is to scan forward for the next forward magic and resume.
                //
                // Mirrors `nodelink-js`'s `BC_MAGIC_REV` handling in
                // `framing.ts`.
                if let Some(skip) = find_next_forward_magic_after_reverse(src) {
                    log::warn!(
                        "Reverse-magic desync detected; skipping {} bytes to next forward magic",
                        skip
                    );
                    src.advance(skip);
                    // Tail-call into ourselves for the next packet attempt.
                    return self.decode(src);
                }
                match e {
                    Error::NomIncomplete(_) => return Ok(None),
                    other => return Err(other),
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bc::de::needs_xor_fallback;

    #[test]
    fn xor_fallback_only_triggers_on_aes() {
        let garbage = b"\xff\xee\xdd\xcc some non-XML bytes";
        assert!(!needs_xor_fallback(
            &EncryptionProtocol::Unencrypted,
            garbage
        ));
        assert!(!needs_xor_fallback(&EncryptionProtocol::BCEncrypt, garbage));
        // AES with non-XML payload -> fallback wanted.
        let aes = EncryptionProtocol::aes([0u8; 16]);
        assert!(needs_xor_fallback(&aes, garbage));
        // AES with XML payload -> no fallback.
        assert!(!needs_xor_fallback(&aes, b"<?xml version=\"1.0\"?><Bc/>"));
        // AES with leading whitespace + XML -> no fallback.
        assert!(!needs_xor_fallback(&aes, b"  <Bc/>"));
        // AES with UTF-8 BOM + XML -> no fallback.
        assert!(!needs_xor_fallback(&aes, b"\xef\xbb\xbf<Bc/>"));
        // Empty AES payload — nothing to fall back to.
        assert!(!needs_xor_fallback(&aes, b""));
    }

    #[test]
    fn aes_fallback_decodes_xor_encoded_frame() {
        // Build a frame as the camera would emit it under BC-XOR for a
        // login-success (msg_id=1, response_code=200). Then feed it through
        // the codec while the codec believes AES is in effect — the
        // fallback path should still parse the XML.
        let xml_payload = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<DeviceInfo version=\"1.1\">\n<resolution>\n<resolutionName>2304*1296</resolutionName>\n<width>2304</width>\n<height>1296</height>\n</resolution>\n</DeviceInfo>\n</body>\n";
        let body = EncryptionProtocol::BCEncrypt.encrypt(0, xml_payload.as_bytes());
        let body_len = body.len() as u32;
        // Header (class 0x0000, has payload_offset = 0, no extension).
        let mut frame = Vec::new();
        frame.extend_from_slice(&MAGIC_HEADER_BYTES);
        frame.extend_from_slice(&1u32.to_le_bytes()); // msg_id
        frame.extend_from_slice(&body_len.to_le_bytes()); // body_len
        frame.push(0u8); // channel_id
        frame.push(0u8); // stream_type
        frame.extend_from_slice(&0u16.to_le_bytes()); // msg_num
        frame.extend_from_slice(&200u16.to_le_bytes()); // response_code
        frame.extend_from_slice(&0u16.to_le_bytes()); // class
        frame.extend_from_slice(&0u32.to_le_bytes()); // payload_offset
        frame.extend_from_slice(&body);

        // Codec primed with AES, but the body is BC-XOR. The fallback should
        // kick in and return a valid parsed Bc.
        let mut codex = BcCodex::new(Credentials::default());
        codex
            .context
            .set_encrypted(EncryptionProtocol::aes([0u8; 16]));
        let mut buf = BytesMut::from(frame.as_slice());
        let result = codex
            .decode(&mut buf)
            .expect("decode should succeed via XOR fallback")
            .expect("frame should be fully parsed");
        assert_eq!(result.meta.msg_id, 1);
        assert_eq!(result.meta.response_code, 200);
    }

    #[test]
    fn reverse_magic_recovery_skips_to_next_forward_magic() {
        // Build a "desync" stream: leading reversed-magic bytes, some
        // garbage, then a real forward-magic frame.
        //
        // We piggy-back on the well-known `modern_login_failed.bin` sample,
        // which is a header-only modern message (body_len == 0). That gives
        // us a real frame we can confidently round-trip.
        let real_frame = include_bytes!("samples/modern_login_failed.bin").to_vec();

        let mut stream = Vec::new();
        // Stray reverse-magic + 6 bytes of garbage, then the real frame.
        stream.extend_from_slice(&MAGIC_HEADER_REV_BYTES);
        stream.extend_from_slice(b"GARBAG");
        stream.extend_from_slice(&real_frame);

        let mut codex = BcCodex::new(Credentials::default());
        codex.context.set_encrypted(EncryptionProtocol::Unencrypted);
        let mut buf = BytesMut::from(stream.as_slice());
        let result = codex
            .decode(&mut buf)
            .expect("decode should succeed after skipping desync")
            .expect("frame should be fully parsed");
        assert_eq!(result.meta.msg_id, 1); // MSG_ID_LOGIN
        assert_eq!(result.meta.response_code, 400);
    }

    #[test]
    fn find_next_forward_magic_returns_none_when_no_reverse_prefix() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC_HEADER_BYTES);
        buf.extend_from_slice(b"junk");
        assert_eq!(find_next_forward_magic_after_reverse(&buf), None);
    }

    #[test]
    fn find_next_forward_magic_returns_offset() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC_HEADER_REV_BYTES);
        buf.extend_from_slice(b"AAA");
        buf.extend_from_slice(&MAGIC_HEADER_BYTES);
        assert_eq!(find_next_forward_magic_after_reverse(&buf), Some(7));
    }
}
