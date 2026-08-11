//! Typed error hierarchy for the core crate.
//!
//! The core crate never uses `anyhow`; every failure is a named variant so the
//! CLI layer can decide how to present it. `anyhow` context is added at the
//! binary boundary only.
//!
//! One deliberate exception to the "be specific" rule: [`CryptoError::Decrypt`]
//! carries no detail. AEAD failures must not distinguish a wrong passphrase
//! from a tampered ciphertext from a truncated tag, because doing so hands an
//! attacker an oracle. See its docs.

use thiserror::Error;

/// A [`ModemConfig`](crate::ModemConfig) that cannot describe a physically
/// realisable modulation scheme.
///
/// All of these are caught once, in [`FskModem::new`](crate::FskModem::new),
/// so that modulation itself is infallible.
// No `Eq`: `InvalidAmplitude` carries an `f32`.
#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum ConfigError {
    #[error("sample_rate must be non-zero")]
    ZeroSampleRate,

    /// Fewer than two samples per symbol leaves no representable bin above DC.
    #[error("samples_per_symbol must be at least 2, got {got}")]
    SymbolTooShort { got: usize },

    /// Phase 1 has no framing layer, so the symbol stream must map onto whole
    /// bytes without padding. That restriction lifts once frame headers carry
    /// an explicit payload length.
    #[error(
        "bits_per_symbol must divide 8 evenly (1, 2, or 4 in phase 1), got {got}; \
         non-divisors need the framing layer to disambiguate trailing padding"
    )]
    BitsPerSymbolNotByteAligned { got: u32 },

    #[error("bin_spacing must be at least 1 for orthogonal tones, got 0")]
    ZeroBinSpacing,

    /// Bin 0 is DC and carries no usable phase; it is also the first thing a
    /// real acoustic channel destroys.
    #[error("base_bin must be at least 1 to avoid DC")]
    DcBaseBin,

    /// The highest tone must sit strictly below the Nyquist bin `N/2`,
    /// otherwise it aliases onto a lower tone and detection is ambiguous.
    #[error(
        "tone plan exceeds Nyquist: highest bin {highest} must be < N/2 = {nyquist_bin} \
         (tones={tones}, base_bin={base_bin}, bin_spacing={bin_spacing})"
    )]
    ExceedsNyquist {
        highest: usize,
        nyquist_bin: usize,
        tones: usize,
        base_bin: usize,
        bin_spacing: usize,
    },

    #[error("amplitude must be finite and within (0.0, 1.0], got {got}")]
    InvalidAmplitude { got: f32 },

    #[error("fft_size must be even and at least 8, got {got}")]
    BadFftSize { got: usize },

    /// Square QAM needs an even bit count so the two axes are independent.
    #[error("bits_per_bin must be even and within 2..=20, got {got}")]
    BadQamOrder { got: u32 },

    #[error("subcarrier band is empty: base_bin {base_bin} is above top_bin {top_bin}")]
    EmptyBand { base_bin: usize, top_bin: usize },
}

/// A sample buffer that the demodulator cannot interpret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DemodError {
    /// Phase 1 assumes a lossless, sample-aligned channel: the buffer is a
    /// whole number of symbols starting at sample 0. An acoustic front end
    /// will instead supply an explicit offset and trim the tail itself.
    #[error(
        "sample buffer length {len} is not a whole number of symbols \
         (samples_per_symbol={samples_per_symbol}, remainder={remainder})"
    )]
    RaggedSymbolBoundary {
        len: usize,
        samples_per_symbol: usize,
        remainder: usize,
    },

    /// A whole number of symbols, but not a whole number of bytes.
    #[error(
        "symbol count {symbols} does not fill whole bytes \
         (symbols_per_byte={symbols_per_byte}, remainder={remainder})"
    )]
    RaggedByteBoundary {
        symbols: usize,
        symbols_per_byte: usize,
        remainder: usize,
    },

    /// The band carries no measurable energy, so no gain reference exists.
    /// Usually silence, or audio that is not a carrier at all.
    #[error("no signal energy in the subcarrier band; this audio carries no OFDM carrier")]
    NoSignal,
}

/// Zstandard compression failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompressError {
    #[error("zstd compression failed: {0}")]
    Compress(String),

    #[error("zstd decompression failed: {0}")]
    Decompress(String),

    /// The decompressed size disagrees with the length recorded in the frame
    /// header. The header is authenticated, so this means the compressed
    /// stream itself is inconsistent with it.
    #[error("decompressed length {got} does not match header length {expected}")]
    LengthMismatch { expected: u64, got: usize },
}

/// Key-derivation and AEAD failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CryptoError {
    #[error("argon2id key derivation failed: {0}")]
    KeyDerivation(String),

    #[error("could not read from the system random source: {0}")]
    Rng(String),

    #[error("AES-256-GCM encryption failed")]
    Encrypt,

    /// Authentication failed.
    ///
    /// Deliberately carries no detail. A wrong passphrase, a flipped
    /// ciphertext bit, a truncated tag, and a header field tampered with under
    /// AAD are all indistinguishable here, and must stay that way: any
    /// discrimination between them is an oracle an attacker can query. The CLI
    /// turns this into a single "wrong passphrase or corrupt payload" message
    /// for the same reason.
    #[error("decryption failed: wrong passphrase, or the payload has been altered")]
    Decrypt,

    /// This build was handed a payload whose header says it is encrypted, but
    /// no passphrase was supplied (or vice versa).
    #[error("payload is encrypted but no passphrase was supplied")]
    PassphraseRequired,

    #[error("payload is not encrypted but a passphrase was supplied")]
    UnexpectedPassphrase,
}

/// RaptorQ forward-error-correction failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FecError {
    /// The packet region is not a whole number of fixed-size packets.
    #[error(
        "FEC region length {len} is not a multiple of the packet size {packet_size} \
         (remainder {remainder})"
    )]
    RaggedPacketRegion {
        len: usize,
        packet_size: usize,
        remainder: usize,
    },

    /// Not enough surviving symbols to invert the code. On a lossless channel
    /// this means the audio was truncated or corrupted beyond what the repair
    /// symbols cover.
    #[error(
        "RaptorQ could not reconstruct the payload from {packets} packets; \
         too many symbols were lost"
    )]
    Unrecoverable { packets: usize },

    #[error("RaptorQ reconstructed {got} bytes but the header expects {expected}")]
    LengthMismatch { expected: u64, got: usize },

    #[error("fec symbol_size must be non-zero")]
    ZeroSymbolSize,
}

/// Frame header parsing failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FrameError {
    #[error("frame is {len} bytes, shorter than the {needed}-byte header")]
    TooShort { len: usize, needed: usize },

    /// The first four bytes are not `AMDM`. Almost always means the audio is
    /// not a stego-flac carrier, or the tone plan used to demodulate it does
    /// not match the one used to write it.
    #[error(
        "bad magic {got:02x?}: this audio was not produced by stego-flac, or the tone plan differs"
    )]
    BadMagic { got: [u8; 4] },

    #[error("unsupported container version {got}, this build understands {supported}")]
    UnsupportedVersion { got: u8, supported: u8 },

    /// The header did not survive demodulation intact.
    #[error("header checksum mismatch (stored {stored:#010x}, computed {computed:#010x})")]
    HeaderCrcMismatch { stored: u32, computed: u32 },

    #[error(
        "frame declares a {declared}-byte FEC region but only {available} bytes follow the header"
    )]
    PayloadTruncated { declared: u64, available: usize },

    #[error("header sets unknown flag bits {bits:#04x}")]
    UnknownFlags { bits: u8 },

    /// The payload claims to carry a filename but the envelope is inconsistent.
    /// Only reachable after the AEAD tag has already verified, so this means a
    /// genuine encoder bug rather than tampering.
    #[error("payload name envelope is malformed: {reason}")]
    MalformedEnvelope { reason: &'static str },
}

/// Aggregate error surfaced at the crate boundary.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum CoreError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Demod(#[from] DemodError),

    #[error(transparent)]
    Compress(#[from] CompressError),

    #[error(transparent)]
    Crypto(#[from] CryptoError),

    #[error(transparent)]
    Fec(#[from] FecError),

    #[error(transparent)]
    Frame(#[from] FrameError),
}
