//! FLAC Vorbis comment metadata.
//!
//! # Why the tone plan lives here
//!
//! The plan cannot be carried *in* the modulated audio: you need it in order to
//! demodulate anything, including a header that would describe it. That was the
//! format's sharpest edge — encode with non-default flags, forget them, and the
//! carrier becomes undecodable with only "bad magic" to go on.
//!
//! A FLAC metadata block resolves it exactly, because it sits *outside* the
//! audio stream and is readable as plain bytes without demodulating anything.
//! The decoder reads the plan from the file and configures itself.
//!
//! This also makes the carrier a properly tagged audio file rather than an
//! untitled blob, so players and library scanners show something meaningful.
//!
//! # What this deliberately does not do
//!
//! Metadata is *not* authenticated. Anyone can rewrite these tags, and a
//! tampered plan simply produces a demodulation that fails the header CRC or
//! magic check. Nothing security-relevant is stored here — no filename (that
//! goes inside the encryption), no key material, no lengths that anything
//! trusts. The tags are a convenience, and the format still works if they are
//! stripped entirely, provided the reader supplies the plan by hand.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

/// FLAC metadata block type for Vorbis comments.
pub const VORBIS_COMMENT_BLOCK: u8 = 4;

/// Tag holding the machine-readable tone plan.
pub const PLAN_TAG: &str = "AUDIOMODEM_PLAN";
/// Tag holding the profile name, when a preset was used.
pub const PROFILE_TAG: &str = "AUDIOMODEM_PROFILE";

const VENDOR: &str = concat!("stego-flac ", env!("CARGO_PKG_VERSION"));

/// Build a `VORBIS_COMMENT` block payload from `KEY=value` pairs.
///
/// Note the endianness trap: FLAC is a big-endian format, but the Vorbis
/// comment block embedded in it keeps Vorbis's little-endian length fields.
/// Everything inside this function is little-endian; the block *header* around
/// it is big-endian.
pub fn build_block(tags: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();

    out.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    out.extend_from_slice(VENDOR.as_bytes());
    out.extend_from_slice(&(tags.len() as u32).to_le_bytes());

    for (key, value) in tags {
        let entry = format!("{key}={value}");
        out.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        out.extend_from_slice(entry.as_bytes());
    }

    out
}

/// Read every Vorbis comment from a FLAC file's metadata.
///
/// Walks the metadata block chain directly rather than going through a decoder.
/// That keeps this readable in about thirty lines, and means `info` and
/// `decode` can learn the tone plan before constructing a modem — which is the
/// whole point, since the modem cannot be built without it.
///
/// Keys are upper-cased, matching the Vorbis convention of case-insensitive
/// field names.
pub fn read_tags(flac: &[u8]) -> Result<BTreeMap<String, String>> {
    if flac.len() < 4 || &flac[0..4] != b"fLaC" {
        bail!("not a FLAC stream (missing the fLaC marker)");
    }

    let mut tags = BTreeMap::new();
    let mut pos = 4usize;

    loop {
        if pos + 4 > flac.len() {
            break;
        }
        let is_last = flac[pos] & 0x80 != 0;
        let block_type = flac[pos] & 0x7f;
        let len = u32::from_be_bytes([0, flac[pos + 1], flac[pos + 2], flac[pos + 3]]) as usize;
        pos += 4;

        if pos + len > flac.len() {
            break;
        }

        if block_type == VORBIS_COMMENT_BLOCK {
            parse_comments(&flac[pos..pos + len], &mut tags);
        }

        pos += len;
        if is_last {
            break;
        }
    }

    Ok(tags)
}

/// Parse the little-endian Vorbis comment payload.
///
/// Malformed blocks are skipped rather than raised: a carrier with damaged tags
/// is still perfectly decodable when the reader supplies the plan explicitly,
/// so refusing to open it would be worse than losing the convenience.
fn parse_comments(block: &[u8], tags: &mut BTreeMap<String, String>) {
    let read_u32 = |data: &[u8], at: usize| -> Option<usize> {
        data.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };

    let Some(vendor_len) = read_u32(block, 0) else {
        return;
    };
    let mut pos = 4 + vendor_len;

    let Some(count) = read_u32(block, pos) else {
        return;
    };
    pos += 4;

    for _ in 0..count {
        let Some(len) = read_u32(block, pos) else {
            return;
        };
        pos += 4;

        let Some(entry) = block.get(pos..pos + len) else {
            return;
        };
        pos += len;

        if let Ok(text) = std::str::from_utf8(entry) {
            if let Some((key, value)) = text.split_once('=') {
                tags.insert(key.to_ascii_uppercase(), value.to_string());
            }
        }
    }
}
