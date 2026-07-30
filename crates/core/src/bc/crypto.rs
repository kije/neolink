use aes::{
    cipher::{AsyncStreamCipher, KeyIvInit},
    Aes128,
};
use cfb_mode::{Decryptor, Encryptor};

type Aes128CfbEnc = Encryptor<Aes128>;
type Aes128CfbDec = Decryptor<Aes128>;

const XML_KEY: [u8; 8] = [0x1F, 0x2D, 0x3C, 0x4B, 0x5A, 0x69, 0x78, 0xFF];
const IV: &[u8] = b"0123456789abcdef";

/// These are the encyption modes supported by the camera
///
/// The mode is negotiated during login
#[derive(Debug, Clone)]
pub enum EncryptionProtocol {
    /// Older camera use no encryption
    Unencrypted,
    /// Camera/Firmwares before 2021 use BCEncrypt which is a simple XOr
    BCEncrypt,
    /// Latest cameras/firmwares use Aes with the key derived from
    /// the camera's password and the negotiated NONCE
    Aes {
        /// The encryptor
        enc: Aes128CfbEnc,
        /// The decryptor
        dec: Aes128CfbDec,
    },
    /// Same as Aes but the media stream is also encrypted and not just
    /// the control commands
    FullAes {
        /// The encryptor
        enc: Aes128CfbEnc,
        /// The decryptor
        dec: Aes128CfbDec,
    },
}

impl EncryptionProtocol {
    /// Helper to make unencrypted
    pub fn unencrypted() -> Self {
        EncryptionProtocol::Unencrypted
    }
    /// Helper to make bcencrypted
    pub fn bcencrypt() -> Self {
        EncryptionProtocol::BCEncrypt
    }
    /// Helper to make aes
    pub fn aes(key: [u8; 16]) -> Self {
        EncryptionProtocol::Aes {
            enc: Aes128CfbEnc::new(key.as_slice().into(), IV.into()),
            dec: Aes128CfbDec::new(key.as_slice().into(), IV.into()),
        }
    }
    /// Helper to make full aes
    pub fn full_aes(key: [u8; 16]) -> Self {
        EncryptionProtocol::FullAes {
            enc: Aes128CfbEnc::new(key.as_slice().into(), IV.into()),
            dec: Aes128CfbDec::new(key.as_slice().into(), IV.into()),
        }
    }

    /// Decrypt the data, offset comes from the header of the packet
    pub fn decrypt(&self, offset: u32, buf: &[u8]) -> Vec<u8> {
        match self {
            EncryptionProtocol::Unencrypted => buf.to_vec(),
            EncryptionProtocol::BCEncrypt => {
                let key_iter = XML_KEY.iter().cycle().skip(offset as usize % 8);
                key_iter
                    .zip(buf)
                    .map(|(key, i)| *i ^ key ^ (offset as u8))
                    .collect()
            }
            EncryptionProtocol::Aes { dec, .. } | EncryptionProtocol::FullAes { dec, .. } => {
                // AES decryption

                let mut decrypted = buf.to_vec();
                dec.clone().decrypt(&mut decrypted);
                decrypted
            }
        }
    }

    /// Encrypt the data, offset comes from the header of the packet
    pub fn encrypt(&self, offset: u32, buf: &[u8]) -> Vec<u8> {
        match self {
            EncryptionProtocol::Unencrypted => {
                // Encrypt is the same as decrypt
                self.decrypt(offset, buf)
            }
            EncryptionProtocol::BCEncrypt => {
                // Encrypt is the same as decrypt
                self.decrypt(offset, buf)
            }
            EncryptionProtocol::Aes { enc, .. } | EncryptionProtocol::FullAes { enc, .. } => {
                // AES encryption
                let mut encrypted = buf.to_vec();
                enc.clone().encrypt(&mut encrypted);
                encrypted
            }
        }
    }
}

#[test]
fn test_xml_crypto() {
    let sample = include_bytes!("samples/xml_crypto_sample1.bin");
    let should_be = include_bytes!("samples/xml_crypto_sample1_plaintext.bin");

    let decrypted = EncryptionProtocol::BCEncrypt.decrypt(0, &sample[..]);
    assert_eq!(decrypted, &should_be[..]);
}

#[test]
fn test_xml_crypto_roundtrip() {
    let zeros: [u8; 256] = [0; 256];

    let decrypted = EncryptionProtocol::BCEncrypt.encrypt(0, &zeros[..]);
    let encrypted = EncryptionProtocol::BCEncrypt.decrypt(0, &decrypted[..]);
    assert_eq!(encrypted, &zeros[..]);
}

#[cfg(test)]
mod aes_tests {
    use super::*;

    /// Arbitrary but fixed, so the known-answer test below is reproducible.
    const TEST_KEY: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
        0xFF,
    ];

    /// 54 bytes: deliberately *not* a multiple of the 16 byte AES block size, so
    /// the trailing partial-block path of CFB is exercised.
    const TEST_PLAINTEXT: &[u8] = b"<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body></body>\n";

    /// What the implementation emits today for `TEST_KEY` over `TEST_PLAINTEXT`,
    /// captured from the current `aes`/`cfb-mode` 0.8 code.
    ///
    /// This is here so a refactor is proven byte-identical rather than merely
    /// self-consistent: a round-trip test still passes if encrypt and decrypt
    /// change in step, but the camera on the other end did not change with us.
    const TEST_CIPHERTEXT: [u8; 54] = [
        0x92, 0x82, 0xAB, 0xF6, 0x78, 0x60, 0xFF, 0xB0, 0x91, 0x6F, 0xC5, 0xBC, 0x35, 0x47, 0x33,
        0x0B, 0xB8, 0x5E, 0x48, 0xAE, 0x97, 0xA3, 0x51, 0x33, 0x81, 0x10, 0x5B, 0xC2, 0xCF, 0xFF,
        0xCA, 0xE3, 0x78, 0xDD, 0x2D, 0x94, 0x94, 0x52, 0x3A, 0x8C, 0x99, 0x87, 0x22, 0x17, 0xF5,
        0xFB, 0x49, 0x25, 0xFF, 0x85, 0x51, 0xF2, 0x69, 0x6C,
    ];

    #[test]
    fn test_aes_roundtrip() {
        let protocol = EncryptionProtocol::aes(TEST_KEY);
        let encrypted = protocol.encrypt(0, TEST_PLAINTEXT);
        assert_ne!(
            encrypted.as_slice(),
            TEST_PLAINTEXT,
            "ciphertext must not equal plaintext"
        );
        assert_eq!(protocol.decrypt(0, &encrypted), TEST_PLAINTEXT);
    }

    #[test]
    fn test_full_aes_roundtrip() {
        let protocol = EncryptionProtocol::full_aes(TEST_KEY);
        let encrypted = protocol.encrypt(0, TEST_PLAINTEXT);
        assert_ne!(
            encrypted.as_slice(),
            TEST_PLAINTEXT,
            "ciphertext must not equal plaintext"
        );
        assert_eq!(protocol.decrypt(0, &encrypted), TEST_PLAINTEXT);
    }

    /// Every packet must restart from the IV.
    ///
    /// The camera treats each packet independently, so encrypting the same bytes
    /// twice through the same `EncryptionProtocol` has to yield the same
    /// ciphertext twice. Today that holds because `encrypt`/`decrypt` clone a
    /// never-mutated cipher per call.
    ///
    /// This is the assertion that matters, and it is the reason it is spelled out
    /// rather than folded into the round-trip tests above: if a refactor ever lets
    /// the CFB keystream advance across packets, the round-trip tests still pass
    /// (we would decrypt our own output happily) while every camera running
    /// post-2021 firmware breaks — silently, with no error raised.
    #[test]
    fn test_aes_packets_are_independent() {
        for protocol in [
            EncryptionProtocol::aes(TEST_KEY),
            EncryptionProtocol::full_aes(TEST_KEY),
        ] {
            let first = protocol.encrypt(0, TEST_PLAINTEXT);
            let second = protocol.encrypt(0, TEST_PLAINTEXT);
            assert_eq!(
                first, second,
                "encrypting the same packet twice must produce identical \
                 ciphertext -- the keystream must not carry across packets"
            );

            // And the same on the decrypt side, which uses a separate cipher.
            assert_eq!(protocol.decrypt(0, &first), protocol.decrypt(0, &second));
            assert_eq!(protocol.decrypt(0, &second), TEST_PLAINTEXT);
        }
    }

    /// `offset` is meaningful for `BCEncrypt` but must be ignored by AES: the
    /// camera does not fold the packet offset into the AES keystream.
    #[test]
    fn test_aes_ignores_offset() {
        let protocol = EncryptionProtocol::aes(TEST_KEY);
        assert_eq!(
            protocol.encrypt(0, TEST_PLAINTEXT),
            protocol.encrypt(0xDEAD, TEST_PLAINTEXT)
        );
    }

    /// Known-answer test. See `TEST_CIPHERTEXT`.
    #[test]
    fn test_aes_known_answer() {
        assert_eq!(
            EncryptionProtocol::aes(TEST_KEY).encrypt(0, TEST_PLAINTEXT),
            TEST_CIPHERTEXT
        );
        assert_eq!(
            EncryptionProtocol::aes(TEST_KEY).decrypt(0, &TEST_CIPHERTEXT),
            TEST_PLAINTEXT
        );
    }

    /// `FullAes` differs from `Aes` only in *what* gets encrypted (media as well
    /// as control commands), not in *how*. Pinning that here means a refactor
    /// that accidentally gives them different keystreams is caught.
    #[test]
    fn test_full_aes_matches_aes_bytes() {
        assert_eq!(
            EncryptionProtocol::full_aes(TEST_KEY).encrypt(0, TEST_PLAINTEXT),
            TEST_CIPHERTEXT
        );
    }

    /// The empty packet is a real case on the wire and a plausible panic site for
    /// a partial-block refactor.
    #[test]
    fn test_aes_empty_payload() {
        let protocol = EncryptionProtocol::aes(TEST_KEY);
        assert!(protocol.encrypt(0, &[]).is_empty());
        assert!(protocol.decrypt(0, &[]).is_empty());
    }
}
