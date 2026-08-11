//! Single-bin DFT power via the Goertzel algorithm.
//!
//! # Why Goertzel and not an FFT
//!
//! An `N`-point real FFT costs `O(N log N)` and yields all `N/2` bins.
//! Goertzel costs `O(N)` per bin and yields one. For `M` tones the crossover
//! is roughly `M` vs `log2(N)`, so at the default plan (`M = 16`, `N = 48`,
//! `log2 N ≈ 5.6`) an FFT is arithmetically cheaper — 16 Goertzel passes do
//! ~768 multiply-accumulates against the FFT's ~270.
//!
//! Goertzel is still the right phase 1 choice:
//!
//! * **No allocation, no plan, no library.** The whole detector is the eight
//!   lines below, which keeps the first correctness proof small.
//! * **Streaming and cache-perfect.** Three scalar registers of state and a
//!   single forward pass over the window; the window stays in L1 and never
//!   needs a scratch buffer or a bit-reversal permutation.
//! * **Parallelism lives elsewhere.** Symbols are independent, so throughput
//!   comes from `rayon` fanning out over symbols, not from a faster kernel.
//!   That decomposition is unchanged if the kernel is later swapped.
//!
//! The STFT demodulator planned for the FDM phase inverts this tradeoff — it
//! wants every bin of every window at once, which is exactly what an FFT is
//! for. [`SymbolDetector`] exists so that substitution touches one type.
//!
//! # Numerics
//!
//! The recurrence is a marginally stable second-order IIR: its poles sit
//! exactly on the unit circle, so round-off accumulates rather than decays.
//! Over `N = 48` samples that is irrelevant, but the state is kept in `f64`
//! regardless — the cost is nil at this window length and it removes any doubt
//! about longer symbols later.

use std::f64::consts::TAU;

/// Precomputed per-tone filter coefficients for a fixed window length.
#[derive(Debug, Clone)]
pub struct SymbolDetector {
    /// `2·cos(2π·m/N)` for each tone's bin `m`.
    coeffs: Vec<f64>,
    window_len: usize,
}

impl SymbolDetector {
    /// Build a detector for `bins`, each interpreted against an
    /// `window_len`-point DFT.
    pub fn new(bins: &[usize], window_len: usize) -> Self {
        let coeffs = bins
            .iter()
            .map(|&bin| 2.0 * (TAU * bin as f64 / window_len as f64).cos())
            .collect();
        Self { coeffs, window_len }
    }

    /// Window length this detector was built for.
    #[inline]
    pub fn window_len(&self) -> usize {
        self.window_len
    }

    /// Number of tones under test.
    #[inline]
    pub fn tone_count(&self) -> usize {
        self.coeffs.len()
    }

    /// Index of the strongest tone in `window`.
    ///
    /// This is the maximum-likelihood decision for non-coherent M-FSK on an
    /// AWGN channel: with equiprobable symbols and equal tone energies, the
    /// likelihood is monotone in the received bin power, so `argmax` over
    /// power *is* the ML rule. No threshold or amplitude reference is needed,
    /// which is also what makes the detector immune to channel gain.
    ///
    /// `window.len()` should equal [`window_len`](Self::window_len); a shorter
    /// or longer window still returns an answer, but the bins are no longer
    /// orthogonal and the result is not meaningful.
    #[inline]
    pub fn detect(&self, window: &[f32]) -> usize {
        let mut best_index = 0usize;
        let mut best_power = f64::NEG_INFINITY;

        for (index, &coeff) in self.coeffs.iter().enumerate() {
            let power = goertzel_power(window, coeff);
            if power > best_power {
                best_power = power;
                best_index = index;
            }
        }

        best_index
    }

    /// Per-tone power across `window`, in tone order.
    ///
    /// Diagnostics only — [`detect`](Self::detect) avoids the allocation.
    /// Useful for asserting the leakage claim and for spectrogram tooling.
    pub fn powers(&self, window: &[f32]) -> Vec<f64> {
        self.coeffs
            .iter()
            .map(|&coeff| goertzel_power(window, coeff))
            .collect()
    }
}

/// Squared magnitude of one DFT bin, given that bin's `coeff = 2·cos(2π·m/N)`.
///
/// The recurrence realises the resonator
///
/// ```text
///   s[n] = x[n] + coeff·s[n-1] - s[n-2]
/// ```
///
/// after which `|X[m]|²` is recovered from the final two states without ever
/// forming the complex output:
///
/// ```text
///   |X[m]|² = s[N-1]² + s[N-2]² - coeff·s[N-1]·s[N-2]
/// ```
///
/// Skipping the complex reconstruction is the reason this is cheaper than a
/// one-bin DFT: no per-sample sin/cos, and no complex multiply at all.
#[inline]
pub fn goertzel_power(window: &[f32], coeff: f64) -> f64 {
    let mut s_prev = 0.0f64;
    let mut s_prev2 = 0.0f64;

    for &sample in window {
        let s = f64::from(sample) + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }

    s_prev * s_prev + s_prev2 * s_prev2 - coeff * s_prev * s_prev2
}
