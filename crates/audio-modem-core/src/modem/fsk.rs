//! Bin-aligned M-FSK modulator and demodulator.

use rayon::prelude::*;
use std::f64::consts::TAU;

use crate::bits;
use crate::error::{ConfigError, DemodError};
use crate::modem::config::ModemConfig;
use crate::modem::goertzel::SymbolDetector;

/// Symbols below which `rayon`'s split overhead outweighs the work.
///
/// A symbol is ~48 multiply-accumulates per tone; a few hundred of them are
/// cheaper to run inline than to hand to a work-stealing scheduler.
const PARALLEL_SYMBOL_THRESHOLD: usize = 512;

/// A configured M-FSK modem.
///
/// Construction validates the tone plan and precomputes both directions, so
/// [`modulate`](Self::modulate) is infallible and [`demodulate`](Self::demodulate)
/// can only fail on buffer geometry.
#[derive(Debug, Clone)]
pub struct FskModem {
    config: ModemConfig,
    /// `M · N` samples: the complete waveform of every symbol, laid out
    /// contiguously with stride `N`.
    ///
    /// This table is only correct because of bin alignment. Each tone
    /// completes an integer number of cycles in `N` samples, so a symbol's
    /// waveform is independent of where it lands in the stream — there is no
    /// phase to carry forward. Modulation degenerates to `extend_from_slice`,
    /// with zero runtime trigonometry.
    ///
    /// At the default plan this is `16 × 48 × 4 B = 3 KiB`, comfortably
    /// L1-resident.
    symbol_waveforms: Vec<f32>,
    detector: SymbolDetector,
}

impl FskModem {
    /// Validate `config` and precompute the waveform table and tone detector.
    pub fn new(config: ModemConfig) -> Result<Self, ConfigError> {
        config.validate()?;

        let tone_count = config.tone_count();
        let n = config.samples_per_symbol;

        let mut symbol_waveforms = Vec::with_capacity(tone_count * n);
        for symbol in 0..tone_count {
            let bin = config.tone_bin(symbol) as f64;
            for sample_index in 0..n {
                // sin(2π·m·n/N). Phase is computed from the integer bin index
                // rather than from a frequency in Hz, so the "integer cycles
                // per symbol" property is exact rather than rounded: at
                // n = N the argument is exactly 2πm.
                let phase = TAU * bin * sample_index as f64 / n as f64;
                symbol_waveforms.push((f64::from(config.amplitude) * phase.sin()) as f32);
            }
        }

        let bins: Vec<usize> = (0..tone_count).map(|s| config.tone_bin(s)).collect();
        let detector = SymbolDetector::new(&bins, n);

        Ok(Self {
            config,
            symbol_waveforms,
            detector,
        })
    }

    /// The validated configuration.
    #[inline]
    pub fn config(&self) -> &ModemConfig {
        &self.config
    }

    /// Number of samples [`modulate`](Self::modulate) will emit for a payload
    /// of `payload_len` bytes.
    #[inline]
    pub fn modulated_len(&self, payload_len: usize) -> usize {
        payload_len * self.config.symbols_per_byte() * self.config.samples_per_symbol
    }

    /// Duration in seconds of a modulated payload of `payload_len` bytes.
    #[inline]
    pub fn duration_secs(&self, payload_len: usize) -> f64 {
        self.modulated_len(payload_len) as f64 / f64::from(self.config.sample_rate)
    }

    /// Modulate `payload` into normalised `f32` PCM in `[-amplitude, amplitude]`.
    ///
    /// Infallible: every input byte has a symbol representation, and the tone
    /// plan was validated at construction.
    pub fn modulate(&self, payload: &[u8]) -> Vec<f32> {
        let symbols = bits::bytes_to_symbols(payload, self.config.bits_per_symbol);
        let n = self.config.samples_per_symbol;

        let mut samples = Vec::with_capacity(symbols.len() * n);
        for &symbol in &symbols {
            let start = symbol as usize * n;
            samples.extend_from_slice(&self.symbol_waveforms[start..start + n]);
        }

        samples
    }

    /// Recover the payload from sample-aligned `samples`.
    ///
    /// Phase 1 assumes the lossless file channel: symbol 0 starts at sample 0
    /// and the buffer holds a whole number of symbols. An acoustic front end
    /// will own timing recovery and hand this function an already-trimmed,
    /// already-aligned slice, so the contract does not change.
    ///
    /// Symbols are independent — the detector carries no state between them —
    /// so the decode is embarrassingly parallel over symbol windows. That
    /// property survives into the FDM/STFT phase, which is why the parallelism
    /// lives here rather than inside the kernel.
    pub fn demodulate(&self, samples: &[f32]) -> Result<Vec<u8>, DemodError> {
        let n = self.config.samples_per_symbol;
        let remainder = samples.len() % n;
        if remainder != 0 {
            return Err(DemodError::RaggedSymbolBoundary {
                len: samples.len(),
                samples_per_symbol: n,
                remainder,
            });
        }

        let symbol_count = samples.len() / n;
        let symbols_per_byte = self.config.symbols_per_byte();
        let byte_remainder = symbol_count % symbols_per_byte;
        if byte_remainder != 0 {
            return Err(DemodError::RaggedByteBoundary {
                symbols: symbol_count,
                symbols_per_byte,
                remainder: byte_remainder,
            });
        }

        let symbols: Vec<u8> = if symbol_count >= PARALLEL_SYMBOL_THRESHOLD {
            samples
                .par_chunks_exact(n)
                .map(|window| self.detector.detect(window) as u8)
                .collect()
        } else {
            samples
                .chunks_exact(n)
                .map(|window| self.detector.detect(window) as u8)
                .collect()
        };

        // Both raggedness checks already passed, so this cannot fail.
        Ok(
            bits::symbols_to_bytes(&symbols, self.config.bits_per_symbol)
                .expect("symbol count was verified to fill whole bytes"),
        )
    }

    /// Per-tone power for one symbol window. Diagnostics and tests.
    pub fn tone_powers(&self, window: &[f32]) -> Vec<f64> {
        self.detector.powers(window)
    }
}

/// Convert normalised `f32` PCM to the `i16` the FLAC container carries.
///
/// This quantisation is the only lossy step in the whole file-only pipeline.
/// It costs ~96 dB of dynamic range against a full-scale tone, which is
/// nowhere near enough to move an `argmax` over 16 orthogonal bins — the
/// decision margin is set by tone separation, not by the noise floor.
///
/// Scaling by 32767 rather than 32768 keeps `+1.0` representable without
/// wrapping to `i16::MIN`; the clamp then only matters for out-of-range input.
pub fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

/// Inverse of [`to_i16`].
pub fn from_i16(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&s| f32::from(s) / 32767.0).collect()
}
