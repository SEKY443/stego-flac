//! The fixed 92-byte frame header.
//!
//! # Layout (all integers little-endian)
//!
//! ```text
//!  off  len  field
//!    0    4  magic "AMDM"
//!    4    1  version
//!    5    1  flags (bit0 compressed, bit1 encrypted, bit2 fec)
//!    6    2  reserved
//!    8    8  original_len      plaintext bytes
//!   16    8  ciphertext_len    bytes handed to the FEC layer
//!   24    8  fec_payload_len   bytes of packets following this header
//!   32   16  argon2id salt
//!   48   12  AES-GCM nonce
//!   60   12  RaptorQ OTI
//!   72    4  argon2id m_cost (KiB)
//!   76    4  argon2id t_cost
//!   80    4  argon2id p_cost
//!   84    2  fec symbol size
//!   86    2  reserved
//!   88    4  CRC-32 over bytes 0..88
//! ```
//!
//! # Why the header is not itself encrypted
//!
//! It cannot be: every field in it is needed *to be able to decrypt*. The salt
//! and KDF costs are inputs to key derivation, and the nonce and lengths are
//! inputs to AEAD verification. A decoder that had to decrypt the header first
//! would have a circular dependency.
//!
//! Instead the header is **authenticated but not confidential**. The
//! security-relevant fields are fed to AES-GCM as additional authenticated data
//! (see [`Header::aad`]), so tampering with a declared length, a KDF cost, or
//! the encrypted flag invalidates the tag and fails the decrypt. The CRC-32 is
//! *not* a security control — it is an integrity check that gives a clear error
//! when the demodulator produced garbage, instead of letting nonsense lengths
//! propagate into allocations. Anyone who can rewrite the payload can recompute
//! a CRC; only the GCM tag resists that.
//!
//! What the header therefore leaks to anyone holding the file: that it is an
//! stego-flac carrier at all, the plaintext size, and the KDF cost parameters.
//! Concealing the first of those is a steganography problem, which this format
//! does not attempt.

use crate::codec::crypto::{KdfParams, NONCE_LEN, SALT_LEN};
use crate::codec::fec::OTI_LEN;
use crate::error::FrameError;

/// Container magic.
pub const MAGIC: [u8; 4] = *b"AMDM";
/// Container format version this build reads and writes.
pub const VERSION: u8 = 1;
/// Serialised header length in bytes.
pub const HEADER_LEN: usize = 92;
/// Byte range covered by the CRC.
const CRC_COVERAGE: usize = 88;

/// Payload was zstd-compressed.
pub const FLAG_COMPRESSED: u8 = 0x01;
/// Payload was AES-256-GCM encrypted.
pub const FLAG_ENCRYPTED: u8 = 0x02;
/// Payload was RaptorQ encoded.
pub const FLAG_FEC: u8 = 0x04;
/// Payload begins with a length-prefixed original filename.
///
/// The name lives *inside* the compressed and encrypted payload, not in this
/// header, so it is protected exactly as well as the file contents are. Putting
/// it in the header would have leaked it, and filenames are frequently more
/// revealing than the bytes they label.
pub const FLAG_NAMED: u8 = 0x08;
/// Payload carries a detected format identifier after the name.
///
/// Separate from [`FLAG_NAMED`] so the two are independent: a piped payload has
/// a format but no name, and `--no-store-name` suppresses both. Keeping it a
/// distinct flag also means carriers written before format detection existed
/// still parse — their envelope simply ends after the name.
pub const FLAG_FORMAT: u8 = 0x10;

/// Payload carries the Unix time at which it was encoded.
///
/// Inside the envelope with the name and the format, not in this header, and
/// for the same reason: when a file was made is frequently as telling as what
/// it contains. A plaintext timestamp would date every carrier to anyone
/// holding it.
pub const FLAG_TIME: u8 = 0x20;

const KNOWN_FLAGS: u8 =
    FLAG_COMPRESSED | FLAG_ENCRYPTED | FLAG_FEC | FLAG_NAMED | FLAG_FORMAT | FLAG_TIME;

/// Parsed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub flags: u8,
    /// Plaintext length before compression.
    pub original_len: u64,
    /// Length of the buffer handed to the FEC layer, i.e. ciphertext plus tag.
    pub ciphertext_len: u64,
    /// Length of the packet region following the header.
    pub fec_payload_len: u64,
    pub salt: [u8; SALT_LEN],
    pub nonce: [u8; NONCE_LEN],
    pub oti: [u8; OTI_LEN],
    pub kdf: KdfParams,
    pub fec_symbol_size: u16,
}

impl Header {
    /// Whether the payload is compressed.
    #[inline]
    pub fn is_compressed(&self) -> bool {
        self.flags & FLAG_COMPRESSED != 0
    }

    /// Whether the payload is encrypted.
    #[inline]
    pub fn is_encrypted(&self) -> bool {
        self.flags & FLAG_ENCRYPTED != 0
    }

    /// Whether the payload is FEC-coded.
    #[inline]
    pub fn is_fec(&self) -> bool {
        self.flags & FLAG_FEC != 0
    }

    /// Whether the payload carries the original filename.
    #[inline]
    pub fn is_named(&self) -> bool {
        self.flags & FLAG_NAMED != 0
    }

    /// Whether the payload carries a detected format identifier.
    #[inline]
    pub fn has_format(&self) -> bool {
        self.flags & FLAG_FORMAT != 0
    }

    /// Whether the payload records when it was encoded.
    #[inline]
    pub fn has_timestamp(&self) -> bool {
        self.flags & FLAG_TIME != 0
    }

    /// Total frame size including the header.
    #[inline]
    pub fn frame_len(&self) -> u64 {
        HEADER_LEN as u64 + self.fec_payload_len
    }

    /// Additional authenticated data for AES-GCM.
    ///
    /// Covers every field that influences key derivation or payload
    /// interpretation *and* is known before encryption runs. Deliberately
    /// excluded: `fec_payload_len`, `oti`, and `fec_symbol_size`, which are not
    /// determined until after the ciphertext exists — a chicken-and-egg the AAD
    /// cannot resolve. Those fields sit below the crypto layer, so corrupting
    /// them yields a FEC failure or a ciphertext that fails its tag anyway;
    /// they cannot be used to steer decryption.
    pub fn aad(&self) -> Vec<u8> {
        let mut aad = Vec::with_capacity(62);
        aad.extend_from_slice(&MAGIC);
        aad.push(self.version);
        aad.push(self.flags);
        aad.extend_from_slice(&self.original_len.to_le_bytes());
        aad.extend_from_slice(&self.ciphertext_len.to_le_bytes());
        aad.extend_from_slice(&self.salt);
        aad.extend_from_slice(&self.nonce);
        aad.extend_from_slice(&self.kdf.m_cost.to_le_bytes());
        aad.extend_from_slice(&self.kdf.t_cost.to_le_bytes());
        aad.extend_from_slice(&self.kdf.p_cost.to_le_bytes());
        aad
    }

    /// Serialise to exactly [`HEADER_LEN`] bytes, appending the CRC.
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];

        out[0..4].copy_from_slice(&MAGIC);
        out[4] = self.version;
        out[5] = self.flags;
        // 6..8 reserved
        out[8..16].copy_from_slice(&self.original_len.to_le_bytes());
        out[16..24].copy_from_slice(&self.ciphertext_len.to_le_bytes());
        out[24..32].copy_from_slice(&self.fec_payload_len.to_le_bytes());
        out[32..48].copy_from_slice(&self.salt);
        out[48..60].copy_from_slice(&self.nonce);
        out[60..72].copy_from_slice(&self.oti);
        out[72..76].copy_from_slice(&self.kdf.m_cost.to_le_bytes());
        out[76..80].copy_from_slice(&self.kdf.t_cost.to_le_bytes());
        out[80..84].copy_from_slice(&self.kdf.p_cost.to_le_bytes());
        out[84..86].copy_from_slice(&self.fec_symbol_size.to_le_bytes());
        // 86..88 reserved

        let crc = crc32fast::hash(&out[..CRC_COVERAGE]);
        out[88..92].copy_from_slice(&crc.to_le_bytes());

        out
    }

    /// Parse and validate a header from the front of `frame`.
    ///
    /// Checks are ordered cheapest-and-most-diagnostic first: length, then
    /// magic (which distinguishes "not our file" from "our file, damaged"),
    /// then version, then CRC. Only once the CRC passes are the length fields
    /// trusted enough to size anything with.
    pub fn parse(frame: &[u8]) -> Result<Self, FrameError> {
        if frame.len() < HEADER_LEN {
            return Err(FrameError::TooShort {
                len: frame.len(),
                needed: HEADER_LEN,
            });
        }

        let magic: [u8; 4] = frame[0..4].try_into().expect("slice is 4 bytes");
        if magic != MAGIC {
            return Err(FrameError::BadMagic { got: magic });
        }

        let version = frame[4];
        if version != VERSION {
            return Err(FrameError::UnsupportedVersion {
                got: version,
                supported: VERSION,
            });
        }

        let stored = u32::from_le_bytes(frame[88..92].try_into().expect("slice is 4 bytes"));
        let computed = crc32fast::hash(&frame[..CRC_COVERAGE]);
        if stored != computed {
            return Err(FrameError::HeaderCrcMismatch { stored, computed });
        }

        let flags = frame[5];
        if flags & !KNOWN_FLAGS != 0 {
            return Err(FrameError::UnknownFlags {
                bits: flags & !KNOWN_FLAGS,
            });
        }

        Ok(Self {
            version,
            flags,
            original_len: u64::from_le_bytes(frame[8..16].try_into().expect("8 bytes")),
            ciphertext_len: u64::from_le_bytes(frame[16..24].try_into().expect("8 bytes")),
            fec_payload_len: u64::from_le_bytes(frame[24..32].try_into().expect("8 bytes")),
            salt: frame[32..48].try_into().expect("16 bytes"),
            nonce: frame[48..60].try_into().expect("12 bytes"),
            oti: frame[60..72].try_into().expect("12 bytes"),
            kdf: KdfParams {
                m_cost: u32::from_le_bytes(frame[72..76].try_into().expect("4 bytes")),
                t_cost: u32::from_le_bytes(frame[76..80].try_into().expect("4 bytes")),
                p_cost: u32::from_le_bytes(frame[80..84].try_into().expect("4 bytes")),
            },
            fec_symbol_size: u16::from_le_bytes(frame[84..86].try_into().expect("2 bytes")),
        })
    }
}
