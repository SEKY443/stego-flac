//! Gray-coded square QAM constellations.
//!
//! Each OFDM subcarrier carries one QAM symbol: a complex point whose in-phase
//! and quadrature components each take `2^(bits/2)` evenly spaced levels. Only
//! even `bits` are supported, which keeps the constellation square and the two
//! axes independent — a rectangular 32-QAM would carry one more bit per bin at
//! the cost of a two-dimensional decision rule, and the gain is not worth it
//! when the link has 40+ dB of unused margin.
//!
//! # How deep a cell can go
//!
//! The ceiling is the 16-bit container, not the modulation. Measured over 30
//! trials per order (six seeds x five payload sizes), recovery is exact up to
//! **20 bits per subcarrier** and collapses at 22:
//!
//! | bits/bin | constellation | file vs raw PCM | failures |
//! |---|---|---|---|
//! | 12 (default) | 4096-QAM | 3.01x | 0/30 |
//! | 16 | 65536-QAM | 2.26x | 0/30 |
//! | 20 | 1048576-QAM | 1.80x | 0/30 |
//! | 22 | 4194304-QAM | 1.64x | **13/30** |
//!
//! The cliff is sharp because the impairment is deterministic quantisation
//! rather than random noise: below it nothing fails, above it the constellation
//! spacing is simply finer than one LSB.
//!
//! The default stays at 12 for margin. A lossless file will not drift, but any
//! later processing — a format conversion, a resample, a gain stage — eats the
//! headroom that higher orders spend. Pass `--qam-bits 16` for a 25% smaller
//! carrier if that risk is acceptable.
//!
//! # Gray coding
//!
//! Adjacent constellation levels differ in exactly one bit. On a clean channel
//! this changes nothing, but when a symbol *does* land in the wrong decision
//! region it is almost always a neighbouring one, so Gray coding turns what
//! would be a multi-bit error into a single-bit error. That matters here
//! because RaptorQ operates over whole 256-byte symbols: a burst that corrupts
//! four bits of one byte costs exactly as much as one bit, but confining damage
//! to fewer *bytes* is what keeps a packet recoverable.
//!
//! # Normalisation
//!
//! Points are scaled so the average symbol energy is 1. That makes the
//! transmitter's gain calculation independent of the constellation order, and
//! lets the receiver recover the channel gain blindly by measuring received
//! power — see [`OfdmModem::demodulate`](super::ofdm::OfdmModem::demodulate).

use rustfft::num_complex::Complex32;

/// Largest constellation that still recovers exactly through 16-bit PCM.
///
/// Measured, not assumed: 20 bits per subcarrier is clean across every trial
/// and 22 fails roughly half of them.
pub const MAX_BITS_PER_POINT: u32 = 20;

/// A square QAM constellation.
#[derive(Debug, Clone)]
pub struct Qam {
    bits: u32,
    /// Levels per axis, `2^(bits/2)`.
    levels: u32,
    /// Scale giving unit average energy.
    norm: f32,
}

impl Qam {
    /// Build a constellation carrying `bits` bits per symbol. `bits` must be
    /// even and in `2..=20`; see the module docs for where that ceiling comes
    /// from.
    pub fn new(bits: u32) -> Option<Self> {
        if bits == 0 || !bits.is_multiple_of(2) || bits > MAX_BITS_PER_POINT {
            return None;
        }
        let levels = 1u32 << (bits / 2);
        // Levels are the odd integers +-1, +-3, ... +-(L-1), whose mean square
        // is (L^2 - 1)/3 per axis, so a 2-D point averages 2(L^2 - 1)/3.
        let mean_energy = 2.0 * ((levels * levels) as f32 - 1.0) / 3.0;
        Some(Self {
            bits,
            levels,
            norm: 1.0 / mean_energy.sqrt(),
        })
    }

    /// Bits carried per constellation point.
    #[inline]
    pub fn bits(&self) -> u32 {
        self.bits
    }

    /// Number of points.
    #[inline]
    pub fn order(&self) -> u32 {
        1 << self.bits
    }

    /// Map `value` (`bits` wide) to a unit-energy constellation point.
    #[inline]
    pub fn map(&self, value: u32) -> Complex32 {
        let half = self.bits / 2;
        let axis_mask = (1u32 << half) - 1;
        let i_bits = (value >> half) & axis_mask;
        let q_bits = value & axis_mask;
        Complex32::new(
            self.level(i_bits) * self.norm,
            self.level(q_bits) * self.norm,
        )
    }

    /// Nearest-point decision for a received, gain-normalised sample.
    #[inline]
    pub fn demap(&self, point: Complex32) -> u32 {
        let half = self.bits / 2;
        let i_bits = self.slice(point.re / self.norm);
        let q_bits = self.slice(point.im / self.norm);
        (i_bits << half) | q_bits
    }

    /// The fixed point used for pilot subcarriers.
    ///
    /// The outermost corner, which has the largest magnitude in the
    /// constellation and therefore the strongest gain reference. Its magnitude
    /// is known to both ends without reference to any data.
    #[inline]
    pub fn pilot(&self) -> Complex32 {
        self.map(0)
    }

    /// Magnitude of [`pilot`](Self::pilot).
    #[inline]
    pub fn pilot_magnitude(&self) -> f32 {
        self.pilot().norm()
    }

    /// Gray codeword -> signed odd-integer amplitude.
    #[inline]
    fn level(&self, gray: u32) -> f32 {
        let index = gray_decode(gray);
        2.0 * index as f32 - (self.levels as f32 - 1.0)
    }

    /// Amplitude -> nearest level -> Gray codeword.
    #[inline]
    fn slice(&self, amplitude: f32) -> u32 {
        let index = ((amplitude + self.levels as f32 - 1.0) * 0.5).round();
        let index = index.clamp(0.0, self.levels as f32 - 1.0) as u32;
        gray_encode(index)
    }
}

/// Binary -> reflected Gray code.
#[inline]
pub fn gray_encode(value: u32) -> u32 {
    value ^ (value >> 1)
}

/// Reflected Gray code -> binary.
#[inline]
pub fn gray_decode(mut gray: u32) -> u32 {
    let mut value = gray;
    while gray > 0 {
        gray >>= 1;
        value ^= gray;
    }
    value
}
