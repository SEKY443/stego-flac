//! Zstandard pre-compression.
//!
//! Compression runs *before* encryption, and that order is not negotiable:
//! ciphertext is indistinguishable from random, so compressing it afterwards
//! would achieve nothing.
//!
//! The usual objection to compress-then-encrypt is the CRIME/BREACH family of
//! attacks, where an attacker who can inject chosen plaintext into the same
//! stream as a secret learns about the secret from the compressed length. That
//! threat model does not apply here: a file is compressed once, in one shot,
//! with no attacker-controlled prefix and no repeated queries. What *does*
//! leak is the compressed size, and therefore a coarse hint about the payload's
//! entropy — an already-compressed archive stays large, a text file shrinks.
//! Nothing in this design hides that, and the audio duration advertises it to
//! anyone holding the file.
//!
//! The level is high (19 by default) because the scarce resource here is audio
//! *time*, not CPU. At 250 B/s, every byte compression removes is 32 ms of
//! carrier that does not have to be generated, stored, or played back.

use crate::error::CompressError;

/// Default zstd level.
///
/// Level 19 rather than the usual 3: encoding a payload is a one-shot,
/// human-initiated operation whose cost is dominated by the resulting audio
/// duration, so trading seconds of CPU for a smaller carrier is nearly always
/// the right call at this scale.
pub const DEFAULT_LEVEL: i32 = 19;

/// Level used to test whether a payload is worth compressing properly.
///
/// Fast enough to be free (~3 ms on 20 MB) because zstd's low levels abandon
/// incompressible input almost immediately, which is exactly the signal wanted.
const PROBE_LEVEL: i32 = 1;

/// Skip the real pass if the probe saves less than this fraction.
///
/// Genuinely incompressible input probes at ~1.000; anything with real
/// structure probes well under 0.98, so the gap is wide and the test is not
/// delicate.
const WORTHWHILE_RATIO: f64 = 0.98;

/// Payloads below this size are compressed directly; probing is not worth an
/// extra pass when the real pass costs microseconds.
const PROBE_THRESHOLD: usize = 64 * 1024;

/// Compress `data` at `level`.
pub fn compress(data: &[u8], level: i32) -> Result<Vec<u8>, CompressError> {
    zstd::bulk::compress(data, level).map_err(|error| CompressError::Compress(error.to_string()))
}

/// Compress only if a cheap probe suggests it will help.
///
/// # Why this exists
///
/// The default level is 19, which is deliberate: audio *time* is the scarce
/// resource, so trading CPU for a smaller carrier is normally right. But level
/// 19 has no early exit for incompressible input — measured on 20 MB of random
/// bytes it grinds for **2.163 s** and returns something 469 bytes *larger*
/// than the input, which is then discarded. That was 77% of the entire encode.
///
/// A level-1 pass reaches the same verdict in 3 ms, so the expensive attempt is
/// now only made when there is something to find. Already-compressed payloads —
/// archives, photos, video, anything encrypted — are the common case for this
/// tool and they now skip compression almost for free.
///
/// # The case this gets wrong
///
/// Level 1 uses a smaller match window than level 19. A payload whose only
/// redundancy is very long-range — say two identical 10 MB halves — can probe
/// as incompressible and be stored raw even though level 19 would have found
/// it. The threshold is set at 0.98 rather than something tighter so that any
/// hint of structure still triggers the real pass, but the blind spot is real.
///
/// Returns `None` when compression was skipped or did not help.
pub fn compress_if_worthwhile(data: &[u8], level: i32) -> Result<Option<Vec<u8>>, CompressError> {
    if data.len() >= PROBE_THRESHOLD {
        let probe = compress(data, PROBE_LEVEL)?;
        if probe.len() as f64 >= data.len() as f64 * WORTHWHILE_RATIO {
            return Ok(None);
        }
    }

    let compressed = compress(data, level)?;
    Ok((compressed.len() < data.len()).then_some(compressed))
}

/// Decompress `data`, refusing to allocate more than `expected_len` bytes.
///
/// The bound is load-bearing. `expected_len` comes from the frame header, which
/// is covered by the AEAD tag, so a forged decompression bomb cannot get this
/// far without also forging a valid GCM tag. The cap is enforced anyway —
/// defence in depth costs one argument here, and this is the one place in the
/// pipeline where a small input can demand an unbounded allocation.
pub fn decompress(data: &[u8], expected_len: u64) -> Result<Vec<u8>, CompressError> {
    let capacity = usize::try_from(expected_len)
        .map_err(|_| CompressError::Decompress("declared length exceeds usize".into()))?;

    let out = zstd::bulk::decompress(data, capacity)
        .map_err(|error| CompressError::Decompress(error.to_string()))?;

    if out.len() as u64 != expected_len {
        return Err(CompressError::LengthMismatch {
            expected: expected_len,
            got: out.len(),
        });
    }

    Ok(out)
}
