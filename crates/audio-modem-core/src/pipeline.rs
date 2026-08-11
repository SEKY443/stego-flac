//! Payload pipeline: plaintext to frame bytes and back.
//!
//! ```text
//!   encode:  plaintext -> zstd -> AES-256-GCM -> RaptorQ -> header || packets
//!   decode:  header || packets -> RaptorQ -> AES-256-GCM -> zstd -> plaintext
//! ```
//!
//! The layer order is forced, not chosen:
//!
//! * **Compress before encrypt**, because ciphertext is incompressible.
//! * **Encrypt before FEC**, because FEC is transport, not security. Coding
//!   first would mean the repair symbols are computed over plaintext, which
//!   would leak structure; and a receiver would have to decode FEC before it
//!   could authenticate anything, so a forged packet stream would be processed
//!   before the tag was ever checked. This way the GCM tag covers exactly what
//!   RaptorQ reconstructed.
//! * **FEC innermost to the channel**, because erasures happen in the channel.
//!
//! The frame produced here is what the modem modulates; nothing below this
//! module knows about audio.

use crate::codec::compress;
use crate::codec::crypto::{self, KdfParams, NONCE_LEN, SALT_LEN, TAG_LEN};
use crate::codec::fec::{self, FecParams};
use crate::error::{CompressError, CoreError, CryptoError, FecError, FrameError};
use crate::format::{self, FileFormat};
use crate::frame::header::{
    Header, FLAG_COMPRESSED, FLAG_ENCRYPTED, FLAG_FEC, FLAG_FORMAT, FLAG_NAMED, FLAG_TIME,
    HEADER_LEN, VERSION,
};

/// Current Unix time, or `None` if the clock is somehow before the epoch.
fn unix_now() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// Refuse to honour a declared plaintext length larger than this.
///
/// A header whose CRC passes is not necessarily a header we wrote — the CRC is
/// an integrity check, not an authentication tag, and the length fields are
/// read *before* anything can be authenticated. This cap is what stops a
/// crafted or badly-corrupted header from turning into a multi-gigabyte
/// allocation during decompression. 256 MiB is already absurd for this format:
/// at the default 250 B/s it would be twelve days of audio.
pub const MAX_PAYLOAD_LEN: u64 = 256 * 1024 * 1024;

/// Longest filename that may be stored in a payload envelope.
///
/// Comfortably above every mainstream filesystem's 255-byte component limit,
/// while still bounding what a malformed envelope can make the decoder read.
pub const MAX_NAME_LEN: usize = 1024;

/// Longest format identifier an envelope may declare.
const MAX_FORMAT_LEN: usize = 32;

/// A payload recovered by [`decode_frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPayload {
    /// The original filename, if the encoder stored one.
    pub name: Option<String>,
    /// The format detected when the payload was encoded, if recognised.
    pub format: Option<FileFormat>,
    /// Unix seconds at which the payload was encoded, if recorded.
    pub encoded_at: Option<u64>,
    /// The file contents.
    pub data: Vec<u8>,
}

impl DecodedPayload {
    /// The filename to write.
    ///
    /// The stored name is returned verbatim — see [`format::default_name`] for
    /// why it is never "corrected" against the detected format. `None` means
    /// the payload carried no name, and the caller must be told where to put
    /// it rather than have a path invented for it.
    pub fn suggested_name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Prefix `data` with the metadata blocks the flags advertise.
///
/// Layout, each block present only when its flag is set:
///
/// ```text
///   [u16 name_len][name]      FLAG_NAMED
///   [u8  fmt_len ][format]    FLAG_FORMAT
///   [u64 unix_seconds]        FLAG_TIME
///   [data]
/// ```
///
/// The envelope sits inside compression and encryption, so both the name and
/// the format are compressed with the payload and covered by the AEAD tag.
/// That placement is the point: "this carrier holds a PDF called
/// tax-return.pdf" is exactly what the encryption exists to hide.
fn wrap_envelope(
    name: Option<&str>,
    format: Option<FileFormat>,
    encoded_at: Option<u64>,
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 64);

    if let Some(name) = name {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
    }
    if let Some(format) = format {
        out.push(format.id.len() as u8);
        out.extend_from_slice(format.id.as_bytes());
    }
    if let Some(seconds) = encoded_at {
        out.extend_from_slice(&seconds.to_le_bytes());
    }

    out.extend_from_slice(data);
    out
}

/// Split an envelope produced by [`wrap_envelope`].
fn unwrap_envelope(
    body: Vec<u8>,
    named: bool,
    formatted: bool,
    timed: bool,
) -> Result<DecodedPayload, FrameError> {
    let mut cursor = 0usize;

    let name = if named {
        if body.len() < 2 {
            return Err(FrameError::MalformedEnvelope {
                reason: "payload is too short to hold a name length",
            });
        }
        let len = u16::from_le_bytes([body[0], body[1]]) as usize;
        if len > MAX_NAME_LEN || 2 + len > body.len() {
            return Err(FrameError::MalformedEnvelope {
                reason: "stored name length runs past the end of the payload",
            });
        }
        cursor = 2 + len;
        Some(String::from_utf8(body[2..2 + len].to_vec()).map_err(|_| {
            FrameError::MalformedEnvelope {
                reason: "stored name is not valid UTF-8",
            }
        })?)
    } else {
        None
    };

    let format = if formatted {
        let len = *body.get(cursor).ok_or(FrameError::MalformedEnvelope {
            reason: "payload is too short to hold a format length",
        })? as usize;
        cursor += 1;
        if len > MAX_FORMAT_LEN || cursor + len > body.len() {
            return Err(FrameError::MalformedEnvelope {
                reason: "stored format length runs past the end of the payload",
            });
        }
        let id = std::str::from_utf8(&body[cursor..cursor + len]).map_err(|_| {
            FrameError::MalformedEnvelope {
                reason: "stored format is not valid UTF-8",
            }
        })?;
        cursor += len;
        // An unknown identifier is not an error: a newer encoder may know
        // formats this build does not, and the payload is still perfectly
        // recoverable without a name for its type.
        format::by_id(id)
    } else {
        None
    };

    let encoded_at = if timed {
        let bytes = body
            .get(cursor..cursor + 8)
            .ok_or(FrameError::MalformedEnvelope {
                reason: "payload is too short to hold a timestamp",
            })?;
        cursor += 8;
        Some(u64::from_le_bytes(bytes.try_into().expect("8 bytes")))
    } else {
        None
    };

    Ok(DecodedPayload {
        name,
        format,
        encoded_at,
        data: body[cursor..].to_vec(),
    })
}

/// Options for [`encode_frame`].
#[derive(Debug, Clone, Copy)]
pub struct EncodeParams<'a> {
    /// Zstandard level.
    pub compression_level: i32,
    /// Passphrase; `None` writes an unencrypted frame.
    pub passphrase: Option<&'a [u8]>,
    /// Argon2id cost parameters.
    pub kdf: KdfParams,
    /// RaptorQ parameters.
    pub fec: FecParams,
    /// Record the encode time, as Unix seconds, in the envelope.
    ///
    /// Encrypted along with the rest of the envelope. It is deliberately not in
    /// the frame header: a plaintext timestamp would date every carrier for
    /// anyone who picked one up.
    pub store_timestamp: bool,
    /// Record the payload's detected format in the envelope.
    ///
    /// On by default: it costs a handful of bytes and is what lets `decode`
    /// restore an extension the stored name never had. Turn it off alongside
    /// the name when even the file's *type* should stay private.
    pub store_format: bool,
}

impl Default for EncodeParams<'_> {
    fn default() -> Self {
        Self {
            compression_level: compress::DEFAULT_LEVEL,
            passphrase: None,
            kdf: KdfParams::default(),
            fec: FecParams::default(),
            store_format: true,
            store_timestamp: true,
        }
    }
}

/// What the encoder did, for reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeReport {
    pub plaintext_len: usize,
    /// Post-compression size, or the plaintext size if compression was skipped.
    pub compressed_len: usize,
    /// Size handed to the FEC layer.
    pub ciphertext_len: usize,
    pub fec_packets: usize,
    pub frame_len: usize,
    pub compressed: bool,
    pub encrypted: bool,
}

impl EncodeReport {
    /// Compression ratio as a fraction of the original size. `1.0` means
    /// compression was skipped or achieved nothing.
    pub fn compression_ratio(&self) -> f64 {
        if self.plaintext_len == 0 {
            return 1.0;
        }
        self.compressed_len as f64 / self.plaintext_len as f64
    }

    /// Total expansion from plaintext to frame, including all overheads.
    pub fn expansion_ratio(&self) -> f64 {
        if self.plaintext_len == 0 {
            return f64::INFINITY;
        }
        self.frame_len as f64 / self.plaintext_len as f64
    }
}

/// Run the encode pipeline, returning the frame bytes and a report.
///
/// `name`, when given, is stored inside the encrypted payload so `decode` can
/// restore the original filename without being told it.
pub fn encode_frame(
    plaintext: &[u8],
    name: Option<&str>,
    params: &EncodeParams<'_>,
) -> Result<(Vec<u8>, EncodeReport), CoreError> {
    let name = name.filter(|n| !n.is_empty() && n.len() <= MAX_NAME_LEN);

    // Detection happens here rather than in the caller so it cannot be
    // forgotten, and so it always sees the real bytes rather than a filename's
    // opinion of them.
    let format = params
        .store_format
        .then(|| format::detect(plaintext))
        .flatten();
    let encoded_at = params.store_timestamp.then(unix_now).flatten();

    let envelope = if name.is_some() || format.is_some() || encoded_at.is_some() {
        wrap_envelope(name, format, encoded_at, plaintext)
    } else {
        plaintext.to_vec()
    };

    // --- compress -------------------------------------------------------
    // Skipped when it does not help, and -- more importantly -- not even
    // attempted at full strength when a cheap probe says there is nothing to
    // find. See `compress_if_worthwhile`.
    let (body, compressed_flag) =
        match compress::compress_if_worthwhile(&envelope, params.compression_level)? {
            Some(compressed) => (compressed, true),
            None => (envelope.clone(), false),
        };

    let mut flags = FLAG_FEC;
    if compressed_flag {
        flags |= FLAG_COMPRESSED;
    }
    if name.is_some() {
        flags |= FLAG_NAMED;
    }
    if format.is_some() {
        flags |= FLAG_FORMAT;
    }
    if encoded_at.is_some() {
        flags |= FLAG_TIME;
    }
    if params.passphrase.is_some() {
        flags |= FLAG_ENCRYPTED;
    }

    // --- key material ---------------------------------------------------
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    if params.passphrase.is_some() {
        crypto::fill_random(&mut salt)?;
        crypto::fill_random(&mut nonce)?;
    }

    let ciphertext_len = if params.passphrase.is_some() {
        (body.len() + TAG_LEN) as u64
    } else {
        body.len() as u64
    };

    // The AAD is derived from a header that is complete except for the FEC
    // fields, which cannot exist yet. See `Header::aad` for why that is sound.
    let mut header = Header {
        version: VERSION,
        flags,
        // The envelope length, not the file length: this is what the
        // decompressor must reproduce and bound its allocation against.
        original_len: envelope.len() as u64,
        ciphertext_len,
        fec_payload_len: 0,
        salt,
        nonce,
        oti: [0u8; fec::OTI_LEN],
        kdf: params.kdf,
        fec_symbol_size: params.fec.symbol_size,
    };

    // --- encrypt --------------------------------------------------------
    let sealed = match params.passphrase {
        Some(passphrase) => {
            let key = crypto::derive_key(passphrase, &salt, params.kdf)?;
            crypto::encrypt(&key, &nonce, &header.aad(), &body)?
        }
        None => body,
    };

    debug_assert_eq!(sealed.len() as u64, ciphertext_len);

    // --- forward error correction ---------------------------------------
    let encoded = fec::encode(&sealed, params.fec)?;
    header.oti = encoded.oti;
    header.fec_payload_len = encoded.packets.len() as u64;

    // --- frame ----------------------------------------------------------
    let mut frame = Vec::with_capacity(HEADER_LEN + encoded.packets.len());
    frame.extend_from_slice(&header.to_bytes());
    frame.extend_from_slice(&encoded.packets);

    let report = EncodeReport {
        plaintext_len: plaintext.len(),
        compressed_len: if compressed_flag {
            sealed.len().saturating_sub(if flags & FLAG_ENCRYPTED != 0 {
                TAG_LEN
            } else {
                0
            })
        } else {
            envelope.len()
        },
        ciphertext_len: sealed.len(),
        fec_packets: encoded.packet_count,
        frame_len: frame.len(),
        compressed: compressed_flag,
        encrypted: params.passphrase.is_some(),
    };

    Ok((frame, report))
}

/// Parse a frame header without touching the payload.
pub fn inspect(frame: &[u8]) -> Result<Header, CoreError> {
    Ok(Header::parse(frame)?)
}

/// Run the decode pipeline.
///
/// Tolerates a truncated `frame`: the FEC region is trimmed to whole packets
/// and RaptorQ is asked to reconstruct from whatever survived. That is the
/// entire practical justification for carrying repair symbols on a lossless
/// channel — see [`crate::codec::fec`].
pub fn decode_frame(frame: &[u8], passphrase: Option<&[u8]>) -> Result<DecodedPayload, CoreError> {
    let header = Header::parse(frame)?;

    if header.original_len > MAX_PAYLOAD_LEN {
        return Err(CompressError::Decompress(format!(
            "header declares {} bytes, above the {MAX_PAYLOAD_LEN}-byte sanity limit",
            header.original_len
        ))
        .into());
    }

    // --- recover the packet region --------------------------------------
    let available = frame.len() - HEADER_LEN;
    let declared = usize::try_from(header.fec_payload_len).unwrap_or(usize::MAX);
    let region = &frame[HEADER_LEN..HEADER_LEN + available.min(declared)];

    let stride = fec::packet_size(header.fec_symbol_size);
    if stride == 0 {
        return Err(FecError::ZeroSymbolSize.into());
    }
    // Round down to whole packets rather than erroring: a clipped tail leaves a
    // partial packet, and discarding it is exactly what lets the rest decode.
    let whole = region.len() - region.len() % stride;
    if whole == 0 {
        return Err(FrameError::PayloadTruncated {
            declared: header.fec_payload_len,
            available,
        }
        .into());
    }

    let sealed = fec::decode(&header.oti, &region[..whole], header.ciphertext_len)?;

    // --- decrypt ---------------------------------------------------------
    let body = match (header.is_encrypted(), passphrase) {
        (true, Some(passphrase)) => {
            let key = crypto::derive_key(passphrase, &header.salt, header.kdf)?;
            crypto::decrypt(&key, &header.nonce, &header.aad(), &sealed)?
        }
        (true, None) => return Err(CryptoError::PassphraseRequired.into()),
        (false, Some(_)) => return Err(CryptoError::UnexpectedPassphrase.into()),
        (false, None) => sealed,
    };

    // --- decompress ------------------------------------------------------
    let envelope = if header.is_compressed() {
        compress::decompress(&body, header.original_len)?
    } else if body.len() as u64 != header.original_len {
        return Err(CompressError::LengthMismatch {
            expected: header.original_len,
            got: body.len(),
        }
        .into());
    } else {
        body
    };

    // --- unwrap the metadata envelope ------------------------------------
    if header.is_named() || header.has_format() || header.has_timestamp() {
        Ok(unwrap_envelope(
            envelope,
            header.is_named(),
            header.has_format(),
            header.has_timestamp(),
        )?)
    } else {
        Ok(DecodedPayload {
            name: None,
            format: None,
            encoded_at: None,
            data: envelope,
        })
    }
}
