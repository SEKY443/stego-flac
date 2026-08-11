//! Container-independent DSP and coding for `stego-flac`.
//!
//! This crate knows nothing about files, CLIs, or FLAC. It maps `&[u8]` to
//! normalised `f32` PCM and back. The binary crate owns all I/O.
//!
//! # Phase 1 scope
//!
//! Only the physical layer exists: bin-aligned M-FSK over a lossless channel.
//! Compression, encryption, FEC, and framing are later layers; the pipeline
//! seams are `&[u8] -> &[u8]` so each can be built and tested in isolation.
//!
//! ```
//! use audio_modem_core::{FskModem, ModemConfig};
//!
//! let modem = FskModem::new(ModemConfig::default())?;
//! let samples = modem.modulate(b"hello");
//! assert_eq!(modem.demodulate(&samples)?, b"hello");
//! # Ok::<(), audio_modem_core::CoreError>(())
//! ```
//!
//! # Why every tone sits on a DFT bin
//!
//! Tone frequencies are not chosen in Hz. They are pinned to exact bins of the
//! symbol-length DFT, `f_i = m_i · fs / N` for integer `m_i`. Three properties
//! follow, and the implementation leans on all three:
//!
//! 1. **Exact non-coherent orthogonality.** Tone spacing becomes
//!    `Δf = k · fs/N = k/T`, which is precisely the condition for orthogonality
//!    under magnitude detection. It holds in integer arithmetic, so there is no
//!    residual correlation to budget for.
//!
//! 2. **Zero spectral leakage.** An integer number of cycles per symbol means
//!    the rectangular analysis window is coherent with the tone: all energy
//!    lands in one bin and adjacent-tone crosstalk is zero to floating-point
//!    precision. No analysis window function is needed, and none is applied —
//!    a Hann window here would *widen* the response and create the leakage it
//!    is normally used to suppress.
//!
//! 3. **Automatic phase continuity.** Every symbol both starts and ends at a
//!    zero crossing with zero phase, so symbols concatenate without
//!    discontinuity regardless of order. There is no phase accumulator, and
//!    therefore the entire modulator reduces to a lookup table of `M`
//!    precomputed waveforms — see [`FskModem`]'s `symbol_waveforms`.
//!
//! # Throughput, honestly
//!
//! M-FSK trades bandwidth for power efficiency, which is the wrong trade on a
//! lossless channel where SNR is effectively infinite. With `M` tones at
//! minimum spacing inside a usable band `B`, the constraint is
//! `M ≤ B·N/fs` and the yield is `R = (fs/N)·log₂(M)` — a logarithmic return
//! on an exponential tone count. The default plan plateaus around 2 kbit/s,
//! so a 2 MB payload runs well over two hours of audio.
//!
//! That ceiling is inherent to FSK, not to this implementation, and no amount
//! of tuning moves it far. Breaking it needs coherent detection carrying
//! multiple bits per tone *per symbol* — the QAM/FDM layer. Phase 1 exists to
//! establish a correct, testable reference against which that layer can be
//! validated.

#![forbid(unsafe_code)]

pub mod bits;
pub mod codec;
pub mod error;
pub mod format;
pub mod frame;
pub mod modem;
pub mod pipeline;

pub use codec::{FecParams, KdfParams};
pub use error::{
    CompressError, ConfigError, CoreError, CryptoError, DemodError, FecError, FrameError,
};
pub use format::FileFormat;
pub use frame::{Header, HEADER_LEN};
pub use modem::{from_i16, to_i16, FskModem, ModemConfig, SymbolDetector};
pub use modem::{Carrier, OfdmConfig, OfdmModem, Plan, PlanParseError, Profile};
pub use pipeline::{
    decode_frame, encode_frame, inspect, DecodedPayload, EncodeParams, EncodeReport,
};
