//! Orthogonal frequency-division multiplexing.
//!
//! # Why this is so much denser than M-FSK
//!
//! M-FSK is a *one-of-M* code: sixteen tones exist, exactly one is active, and
//! the choice of which carries `log₂(16) = 4` bits. Fifteen sixteenths of the
//! occupied spectrum is silent at any instant. The signal is a line in the
//! time-frequency plane.
//!
//! OFDM fills the plane. Every subcarrier is active in every symbol, and each
//! independently carries a QAM point. With `K` bins at `b` bits each, a symbol
//! carries `K·b` bits instead of `log₂(K)` — for the default plan, 760 bits
//! against 4. The carrier becomes a genuine two-dimensional matrix: time along
//! one axis, frequency along the other, data in every cell.
//!
//! ```text
//!   M-FSK, 48 samples          OFDM, 256 samples
//!   bin  ...................   bin  ####################
//!    19  ....##.............    110 ####################
//!    ..  ..........##.......    ..  ####################
//!     4  ##..............##.     16 ####################
//!        --- time --->              --- time --->
//!        4 bits / 48 samples        760 bits / 256 samples
//! ```
//!
//! # No cyclic prefix
//!
//! Every real OFDM system prefixes each symbol with a copy of its tail, to
//! absorb multipath echo and keep subcarriers orthogonal across a dispersive
//! channel. That costs 7-25% of the payload.
//!
//! This channel is a file. It is bit-exact, has no echo, no delay spread, and
//! no timing offset, so there is no inter-symbol interference to absorb. The
//! cyclic prefix is omitted entirely — it would be pure overhead. This is the
//! same reasoning that makes FEC nearly free here, and it is the one place
//! where the "boring" lossless channel pays a real dividend.
//!
//! # The limit is quantisation, and it is far away
//!
//! Signal RMS sits ~12 dB below full scale (OFDM's near-Gaussian sample
//! distribution needs that much crest headroom), giving ~88 dB against the
//! 16-bit quantisation floor and ~89 dB per bin once the noise is spread over
//! `N/2` bins but the signal over `K`. Shannon puts that at ~30 bits/bin. At
//! the default 8 bits/bin the margin is ~55 dB, so errors come from
//! implementation mistakes, not noise — which is why the tests assert *exact*
//! recovery rather than a tolerable error rate.

use rayon::prelude::*;
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

use crate::bits::{BitReader, BitWriter};
use crate::error::{ConfigError, DemodError};
use crate::modem::qam::Qam;

/// Nominal per-point gain used while generating; the signal is rescaled to its
/// true peak afterwards, so this only affects intermediate conditioning.
const NOMINAL_GAIN: f32 = 1.0;

/// Subcarriers between pilots. Every `PILOT_STRIDE`-th active bin is a pilot.
///
/// # Why gain cannot be inferred from received power
///
/// The obvious way to recover the channel gain is to measure received power and
/// lean on the constellation having unit average energy. That fails, and it
/// fails for a reason no amount of averaging repairs: a symbol's *realised*
/// power `P` deviates from 1 by the data it happens to carry, and a
/// power-based estimator returns `p/sqrt(P)` — the data's power variation is
/// algebraically indistinguishable from channel gain.
///
/// It is worse than a small bias. When a payload ends mid-symbol the unused
/// bins are fed zero bits, which Gray-map to the outermost corner point, so a
/// partially filled symbol carries roughly three times normal power and every
/// point in it decodes wrong. That was the observed failure: a 1652-byte frame
/// decoded cleanly for two symbols and then collapsed at bit 5832, exactly the
/// first partial symbol.
///
/// Pilots remove the circularity by putting a *known* value on the wire. The
/// receiver compares the received pilot magnitude against the constellation
/// corner it must have been, which depends on nothing the payload does. The
/// cost is one subcarrier in sixteen, about 6% of throughput, and in exchange
/// the gain estimate is exact for a five-symbol frame and a five-hour one
/// alike.
const PILOT_STRIDE: usize = 16;

/// Parameters for an OFDM carrier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OfdmConfig {
    /// Container sample rate in Hz.
    pub sample_rate: u32,
    /// FFT size `N`, in samples per symbol. Must be even.
    pub fft_size: usize,
    /// Lowest data-bearing bin.
    pub base_bin: usize,
    /// Highest data-bearing bin, inclusive.
    pub top_bin: usize,
    /// Bits per subcarrier. Even, `2..=20`.
    pub bits_per_bin: u32,
    /// Target peak amplitude in normalised full scale.
    pub amplitude: f32,
    /// Subcarrier range handed to cover audio instead of data, if any.
    ///
    /// When set, `base_bin..=cover_top` carry an audible signal and the data
    /// starts above it. The two sets are disjoint, so the cover cannot reach a
    /// data bin — see [`CoverPlan`].
    pub cover_top: Option<usize>,
    /// How far below the cover the data sits, in decibels.
    pub data_attenuation_db: f32,
    /// Place a data symbol only every `spread` symbols; 1 is contiguous.
    ///
    /// Used to stretch a short payload across a longer piece of cover audio so
    /// the recording plays to its end instead of being cut off mid-phrase. The
    /// symbols in between carry cover only — no pilots, no data — and the
    /// receiver skips them, which it can because the stride travels in the
    /// plan.
    pub spread: usize,
}

/// Where the audible band ends and the data band begins.
///
/// # Why a band split, and why this one
///
/// The ear is not uniformly sensitive. Its threshold bottoms out between 2 and
/// 5 kHz — the ear canal is a quarter-wave resonator around 3 kHz — and nearly
/// all speech intelligibility lives in the formant range 300-3400 Hz. That is
/// not a coincidence: it is why the telephone band was chosen, and roughly what
/// AM broadcast delivers.
///
/// So the cover audio is given 300-3400 Hz, where the ear is sharpest and a
/// voice remains recognisable, and the data is moved above it, where the ear is
/// far less discriminating and broadband noise reads as hiss. The result has
/// the spectral signature of a weak AM station: band-limited speech under
/// static.
///
/// # Interference is zero by construction
///
/// This is the part that matters technically. The cover is written *into the
/// same FFT bins* the modulator already owns, not filtered and mixed in the
/// time domain. Filtering could never be clean enough: a rectangular analysis
/// window scatters out-of-band energy into distant bins at roughly
/// `1/(pi * bin_distance)`, which is about -50 dB a hundred bins away — and
/// 4096-QAM needs ~46 dB of headroom, so a cover sitting 20 dB *above* the data
/// would swamp it. Writing disjoint bins sidesteps the whole problem: the
/// demodulator reads only data bins, and no cover energy exists there to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverPlan {
    /// First bin carrying cover audio.
    pub base_bin: usize,
    /// Last bin carrying cover audio, inclusive.
    pub top_bin: usize,
}

/// Upper edge of the narrowest cover band, in Hz: the telephone band.
pub const COVER_TELEPHONE_HZ: f64 = 3400.0;
/// Upper edge of the medium cover band, in Hz.
pub const COVER_WIDE_HZ: f64 = 5000.0;
/// Upper edge of the widest cover band, in Hz.
pub const COVER_FULL_HZ: f64 = 7000.0;

/// Below this many data subcarriers a widened cover band is refused.
///
/// Pilots are one bin in [`PILOT_STRIDE`], so a band this narrow still leaves
/// several of them to estimate gain from, and the equaliser needs more than a
/// couple of samples across the band to be worth anything.
const MIN_DATA_BINS: usize = 48;

impl CoverPlan {
    /// The telephone/AM band, in bins, for a given geometry.
    pub fn telephone_band(sample_rate: u32, fft_size: usize) -> Self {
        Self::to_ceiling(sample_rate, fft_size, COVER_TELEPHONE_HZ)
    }

    /// A cover band from 300 Hz up to `ceiling_hz`.
    ///
    /// Only `top_bin` reaches the modulator: the cover's floor is the plan's own
    /// `base_bin`, since there are no subcarriers below it to hand over. The
    /// `base_bin` computed here is what a 300 Hz floor *would* be, and exists
    /// for callers that want to describe the band rather than set it.
    pub fn to_ceiling(sample_rate: u32, fft_size: usize, ceiling_hz: f64) -> Self {
        let width = sample_rate as f64 / fft_size as f64;
        Self {
            base_bin: (300.0 / width).ceil() as usize,
            top_bin: (ceiling_hz / width) as usize,
        }
    }

    /// The widest cover band a frame of `frame_bytes` can afford.
    ///
    /// # Why size should decide this
    ///
    /// Widening the cover band is paid for in subcarriers: every bin handed to
    /// the audible signal is one the data no longer has, so the carrier gets
    /// proportionally longer and the file proportionally larger. The *rate* of
    /// that penalty does not depend on payload size — but the *absolute* cost
    /// very much does. Doubling the size of a 200 KB carrier is free in any
    /// sense a user cares about; doubling a 300 MB one is not.
    ///
    /// Small payloads are also exactly where the expansion ratio is already
    /// worst, because the fixed overhead — header, 5% FEC, container — is
    /// amortised over less data. That budget is being spent whether or not the
    /// cover sounds good, so it may as well buy bandwidth.
    ///
    /// So the band widens as the payload shrinks: 7 kHz under a few megabytes,
    /// 5 kHz under a few tens, and the plain telephone band above that. The
    /// thresholds are on the *frame* — post-compression, post-FEC — because
    /// that is what actually becomes audio.
    pub fn auto(config: &OfdmConfig, frame_bytes: usize) -> Self {
        const WIDE_ABOVE: usize = 4 << 20;
        const TELEPHONE_ABOVE: usize = 32 << 20;

        let ceiling = if frame_bytes <= WIDE_ABOVE {
            COVER_FULL_HZ
        } else if frame_bytes <= TELEPHONE_ABOVE {
            COVER_WIDE_HZ
        } else {
            COVER_TELEPHONE_HZ
        };

        // Step back down rather than return a band the modem cannot carry: a
        // narrow FFT or an unusual base/top pair can make the widest tier leave
        // too few data bins, and a worse-sounding cover beats a failed encode.
        for candidate in [ceiling, COVER_WIDE_HZ, COVER_TELEPHONE_HZ] {
            if candidate > ceiling {
                continue;
            }
            let band = Self::to_ceiling(config.sample_rate, config.fft_size, candidate);
            if band.fits(config) {
                return band;
            }
        }
        Self::to_ceiling(config.sample_rate, config.fft_size, COVER_TELEPHONE_HZ)
    }

    /// Whether `config` keeps enough data subcarriers under this cover band.
    fn fits(&self, config: &OfdmConfig) -> bool {
        if self.top_bin < config.base_bin || self.top_bin >= config.top_bin {
            return false;
        }
        let mut probe = *config;
        probe.cover_top = Some(self.top_bin);
        probe.data_bins() >= MIN_DATA_BINS
    }
}

impl Default for OfdmConfig {
    /// 24 kHz, 256-point FFT, 95 subcarriers of 256-QAM.
    ///
    /// | quantity | value |
    /// |---|---|
    /// | symbol period | 10.67 ms (`N` = 256) |
    /// | bin width | 93.75 Hz |
    /// | subcarriers | 95, bins 16..=110 |
    /// | occupied band | 1500 Hz .. 10312 Hz |
    /// | throughput | 71250 bit/s = 8.9 kB/s |
    ///
    /// `bits_per_bin = 8` is chosen partly for its 55 dB of margin and partly
    /// because it makes every subcarrier carry exactly one byte, so a symbol is
    /// exactly `K` bytes and no payload byte ever straddles a symbol boundary.
    /// That is the same alignment argument that picked 4 bits/symbol for the
    /// FSK plan, one layer up.
    fn default() -> Self {
        Self {
            sample_rate: 24_000,
            fft_size: 256,
            base_bin: 16,
            top_bin: 110,
            bits_per_bin: 8,
            amplitude: 0.9,
            cover_top: None,
            data_attenuation_db: 0.0,
            spread: 1,
        }
    }
}

impl OfdmConfig {
    /// First subcarrier carrying data, which sits above any cover band.
    #[inline]
    pub fn data_base_bin(&self) -> usize {
        match self.cover_top {
            Some(top) => top + 1,
            None => self.base_bin,
        }
    }

    /// Number of data-bearing subcarriers.
    #[inline]
    pub fn active_bins(&self) -> usize {
        (self.top_bin + 1).saturating_sub(self.data_base_bin())
    }

    /// Symbols occupied by one data symbol and the blanks that follow it.
    #[inline]
    pub fn stride(&self) -> usize {
        self.spread.max(1)
    }

    /// Linear amplitude applied to data bins relative to the cover.
    #[inline]
    pub fn data_gain(&self) -> f32 {
        10f32.powf(-self.data_attenuation_db.abs() / 20.0)
    }

    /// Subcarriers reserved as pilots.
    #[inline]
    pub fn pilot_bins(&self) -> usize {
        self.active_bins().div_ceil(PILOT_STRIDE)
    }

    /// Subcarriers carrying payload.
    #[inline]
    pub fn data_bins(&self) -> usize {
        self.active_bins() - self.pilot_bins()
    }

    /// Whether the subcarrier at `offset` from `base_bin` is a pilot.
    #[inline]
    pub fn is_pilot(offset: usize) -> bool {
        offset.is_multiple_of(PILOT_STRIDE)
    }

    /// Bits carried by one OFDM symbol.
    #[inline]
    pub fn bits_per_symbol(&self) -> usize {
        self.data_bins() * self.bits_per_bin as usize
    }

    /// Symbol rate in baud.
    #[inline]
    pub fn symbol_rate(&self) -> f64 {
        self.sample_rate as f64 / self.fft_size as f64
    }

    /// Gross throughput in bits per second.
    #[inline]
    pub fn bit_rate(&self) -> f64 {
        self.symbol_rate() * self.bits_per_symbol() as f64
    }

    /// Bin width in Hz.
    #[inline]
    pub fn bin_width_hz(&self) -> f64 {
        self.symbol_rate()
    }

    /// Centre frequency of a bin.
    #[inline]
    pub fn bin_hz(&self, bin: usize) -> f64 {
        bin as f64 * self.bin_width_hz()
    }

    /// Serialise as a plan string field list.
    pub fn to_plan_string(&self) -> String {
        format!(
            "mode=ofdm;fs={};n={};base={};top={};qbits={};amp={}{}{}",
            self.sample_rate,
            self.fft_size,
            self.base_bin,
            self.top_bin,
            self.bits_per_bin,
            self.amplitude,
            match self.cover_top {
                Some(top) => format!(";cover={};atten={}", top, self.data_attenuation_db),
                None => String::new(),
            },
            if self.stride() > 1 {
                format!(";spread={}", self.stride())
            } else {
                String::new()
            }
        )
    }

    /// Check every invariant the modulator relies on.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sample_rate == 0 {
            return Err(ConfigError::ZeroSampleRate);
        }
        if self.fft_size < 8 || !self.fft_size.is_multiple_of(2) {
            return Err(ConfigError::BadFftSize { got: self.fft_size });
        }
        if self.bits_per_bin == 0
            || !self.bits_per_bin.is_multiple_of(2)
            || self.bits_per_bin > crate::modem::qam::MAX_BITS_PER_POINT
        {
            return Err(ConfigError::BadQamOrder {
                got: self.bits_per_bin,
            });
        }
        if self.base_bin == 0 {
            return Err(ConfigError::DcBaseBin);
        }
        if self.top_bin < self.base_bin {
            return Err(ConfigError::EmptyBand {
                base_bin: self.base_bin,
                top_bin: self.top_bin,
            });
        }
        if let Some(cover_top) = self.cover_top {
            if cover_top < self.base_bin || cover_top >= self.top_bin {
                return Err(ConfigError::EmptyBand {
                    base_bin: self.base_bin,
                    top_bin: cover_top,
                });
            }
        }
        // Bin N/2 is the Nyquist bin: it is its own conjugate, so it cannot
        // carry an independent complex value and must stay empty.
        let nyquist_bin = self.fft_size / 2;
        if self.top_bin >= nyquist_bin {
            return Err(ConfigError::ExceedsNyquist {
                highest: self.top_bin,
                nyquist_bin,
                tones: self.active_bins(),
                base_bin: self.base_bin,
                bin_spacing: 1,
            });
        }
        if !self.amplitude.is_finite() || self.amplitude <= 0.0 || self.amplitude > 1.0 {
            return Err(ConfigError::InvalidAmplitude {
                got: self.amplitude,
            });
        }
        Ok(())
    }
}

/// A configured OFDM modem.
pub struct OfdmModem {
    config: OfdmConfig,
    qam: Qam,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
    /// Transmit gain applied to each constellation point.
    gain: f32,
}

impl std::fmt::Debug for OfdmModem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfdmModem")
            .field("config", &self.config)
            .field("gain", &self.gain)
            .finish_non_exhaustive()
    }
}

impl OfdmModem {
    /// Validate `config` and plan the transforms.
    pub fn new(config: OfdmConfig) -> Result<Self, ConfigError> {
        config.validate()?;

        let qam = Qam::new(config.bits_per_bin).ok_or(ConfigError::BadQamOrder {
            got: config.bits_per_bin,
        })?;

        let mut planner = FftPlanner::new();
        let forward = planner.plan_fft_forward(config.fft_size);
        let inverse = planner.plan_fft_inverse(config.fft_size);

        Ok(Self {
            config,
            qam,
            forward,
            inverse,
            gain: NOMINAL_GAIN,
        })
    }

    /// The validated configuration.
    #[inline]
    pub fn config(&self) -> &OfdmConfig {
        &self.config
    }

    /// Bytes carried per OFDM symbol, when the symbol is byte-aligned.
    #[inline]
    fn bytes_per_symbol(&self) -> usize {
        self.config.bits_per_symbol() / 8
    }

    /// Samples emitted for a payload of `payload_len` bytes.
    pub fn modulated_len(&self, payload_len: usize) -> usize {
        let bits = payload_len * 8;
        let symbols = bits.div_ceil(self.config.bits_per_symbol());
        symbols * self.config.stride() * self.config.fft_size
    }

    /// Duration in seconds of a modulated payload.
    pub fn duration_secs(&self, payload_len: usize) -> f64 {
        self.modulated_len(payload_len) as f64 / f64::from(self.config.sample_rate)
    }

    /// Modulate `payload` into normalised `f32` PCM.
    ///
    /// # Peak normalisation instead of a crest-factor guess
    ///
    /// An OFDM sample is a sum of `K` independent subcarriers, so by the
    /// central limit theorem its amplitude is near-Gaussian and has no bound.
    /// A real-time transmitter must therefore guess a backoff: scale for the
    /// theoretical worst case (all carriers aligned, peak `= K × max`) and
    /// throw away ~30 dB, or pick a crest factor and accept occasional
    /// clipping. Measured here, a 4-sigma guess overshot full scale by 1.8x on
    /// some payloads and put 83 byte errors into a 1024-QAM decode.
    ///
    /// This encoder is offline and holds the whole carrier, so it does not have
    /// to guess: it generates at unit gain, measures the true peak, and scales
    /// exactly to the requested amplitude. Clipping becomes structurally
    /// impossible and the dynamic range is used to the last dB.
    ///
    /// Choosing the scale per file is only safe because the receiver recovers
    /// gain blindly — see [`demodulate`](Self::demodulate). Nothing has to
    /// agree on the number.
    pub fn modulate(&self, payload: &[u8]) -> Vec<f32> {
        let mut out = self.modulate_raw(payload);
        normalise_peak(&mut out, self.config.amplitude);
        out
    }

    /// Modulate `payload` with audible cover audio in the low band.
    ///
    /// The cover is written into its own subcarriers and the data into theirs,
    /// then the two are summed. Because the bin sets are disjoint and the
    /// transform is linear, summing in the time domain is identical to writing
    /// one combined spectrum — and the demodulator, which reads only data bins,
    /// cannot see the cover at all. Interference is not small here; it is
    /// absent.
    ///
    /// `cover` is looped if it is shorter than the carrier, and its level is
    /// measured rather than assumed, so the requested attenuation between cover
    /// and data holds whatever the source material is.
    /// `fill_cover` extends the carrier to the cover's full length with
    /// symbols that carry audio and no data, so a recording finishes instead of
    /// stopping mid-phrase. The stride handles the coarse spacing; this handles
    /// the remainder the integer stride cannot express.
    pub fn modulate_with_cover(&self, payload: &[u8], cover: &[f32], fill_cover: bool) -> Vec<f32> {
        let mut data = self.modulate_raw(payload);
        if cover.is_empty() || self.config.cover_top.is_none() {
            normalise_peak(&mut data, self.config.amplitude);
            return data;
        }

        if fill_cover {
            let group = self.config.fft_size * self.config.stride();
            let wanted = (cover.len() / group) * group;
            if wanted > data.len() {
                data.resize(wanted, 0.0);
            }
        }

        let voice = self.render_cover(cover, data.len());

        // Put the data the requested number of decibels below the cover, using
        // the levels actually produced rather than nominal ones.
        let data_rms = rms(&data);
        let voice_rms = rms(&voice);

        // Silence, or cover material with no energy in the audible band, would
        // otherwise scale the data to zero and destroy the payload. Fall back
        // to a plain carrier: there is nothing to hide behind.
        if voice_rms <= 0.0 || data_rms <= 0.0 {
            normalise_peak(&mut data, self.config.amplitude);
            return data;
        }

        let scale = voice_rms * self.config.data_gain() / data_rms;

        for (slot, &audible) in data.iter_mut().zip(voice.iter()) {
            *slot = audible + *slot * scale;
        }
        normalise_peak(&mut data, self.config.amplitude);
        data
    }

    /// Band-limit `cover` into the cover bins, one OFDM symbol at a time.
    ///
    /// Done in the transform domain rather than with a filter because a filter
    /// cannot be clean enough: out-of-band energy leaks into distant bins at
    /// about `1/(pi * distance)`, roughly -50 dB a hundred bins away, and the
    /// constellation needs more headroom than that. Copying bins gives exact
    /// containment.
    fn render_cover(&self, cover: &[f32], samples: usize) -> Vec<f32> {
        let n = self.config.fft_size;
        let cover_top = self.config.cover_top.unwrap_or(0);
        let base = self.config.base_bin;
        let symbols = samples / n;

        let mut out = vec![0.0f32; samples];
        out.par_chunks_exact_mut(n).enumerate().for_each_init(
            || vec![Complex32::new(0.0, 0.0); n],
            |buffer, (index, slot)| {
                for (offset, cell) in buffer.iter_mut().enumerate() {
                    let position = index * n + offset;
                    *cell = Complex32::new(cover[position % cover.len()], 0.0);
                }
                self.forward.process(buffer);

                // Keep the audible band and its conjugate mirror; discard
                // everything else, which is what makes the containment exact.
                for (bin, cell) in buffer.iter_mut().enumerate() {
                    let mirrored = n - bin;
                    let keep =
                        (base..=cover_top).contains(&bin) || (base..=cover_top).contains(&mirrored);
                    if !keep {
                        *cell = Complex32::new(0.0, 0.0);
                    }
                }

                self.inverse.process(buffer);
                // rustfft normalises neither direction, so a forward and an
                // inverse pass together scale by N.
                let inverse_n = 1.0 / n as f32;
                for (dst, src) in slot.iter_mut().zip(buffer.iter()) {
                    *dst = src.re * inverse_n;
                }
            },
        );
        let _ = symbols;
        out
    }

    fn modulate_raw(&self, payload: &[u8]) -> Vec<f32> {
        let n = self.config.fft_size;
        let bits_per_symbol = self.config.bits_per_symbol();
        let symbol_count = (payload.len() * 8).div_ceil(bits_per_symbol);

        let bins = self.config.active_bins();
        let data_base = self.config.data_base_bin();
        let pilot = self.qam.pilot();

        // Symbol `i` reads from a known bit offset, so symbols are independent
        // and the modulator fans out exactly like the demodulator does. Each
        // worker keeps its own spectrum buffer and FFT scratch.
        //
        // With a stride, each chunk holds one data symbol followed by blanks.
        // Only the leading `n` samples are written; the rest stay silent in the
        // data band and are later filled by cover audio alone.
        let stride = self.config.stride();
        let mut out = vec![0.0f32; symbol_count * stride * n];
        out.par_chunks_exact_mut(n * stride)
            .enumerate()
            .for_each_init(
                || vec![Complex32::new(0.0, 0.0); n],
                |spectrum, (index, slot)| {
                    spectrum
                        .iter_mut()
                        .for_each(|c| *c = Complex32::new(0.0, 0.0));
                    let mut reader = BitReader::new_at(payload, index * bits_per_symbol);

                    for offset in 0..bins {
                        let bin = data_base + offset;
                        let point = if OfdmConfig::is_pilot(offset) {
                            // Pilots alternate sign across bins so they do not
                            // sum coherently into a peak; magnitude, which is
                            // all the receiver measures, is unaffected.
                            pilot * pilot_sign(bin)
                        } else {
                            self.qam.map(reader.read(self.config.bits_per_bin))
                        } * self.gain;

                        // Hermitian symmetry makes the inverse transform
                        // real-valued: X[N-k] = conj(X[k]) means the two
                        // rotating phasors sum to a cosine. Without it the
                        // output would be complex and half the spectrum wasted.
                        spectrum[bin] = point;
                        spectrum[n - bin] = point.conj();
                    }

                    self.inverse.process(spectrum);
                    for (dst, src) in slot.iter_mut().take(n).zip(spectrum.iter()) {
                        *dst = src.re;
                    }
                },
            );

        out
    }

    /// Recover the payload from sample-aligned `samples`.
    ///
    /// The receive gain is estimated from the signal rather than assumed. Every
    /// constellation is normalised to unit average energy, so the mean received
    /// bin power *is* the squared gain — no pilot tone or reference amplitude is
    /// needed. This costs nothing and makes the demodulator immune to any
    /// uniform level change, so a carrier that has been volume-normalised or
    /// re-encoded at a different gain still decodes.
    pub fn demodulate(&self, samples: &[f32]) -> Result<Vec<u8>, DemodError> {
        let n = self.config.fft_size;
        let group = n * self.config.stride();
        let remainder = samples.len() % group;
        if remainder != 0 {
            return Err(DemodError::RaggedSymbolBoundary {
                len: samples.len(),
                samples_per_symbol: group,
                remainder,
            });
        }

        let stride = self.config.stride();
        let symbol_count = samples.len() / (n * stride);
        if symbol_count == 0 {
            return Ok(Vec::new());
        }

        let bins = self.config.active_bins();
        let data_base = self.config.data_base_bin();
        let pilot_magnitude = self.qam.pilot_magnitude();
        let mut any_signal = false;

        // Symbols are independent -- no state crosses a boundary, because there
        // is no cyclic prefix and no channel memory -- so the decode is
        // embarrassingly parallel over symbols, exactly as in the FSK path.
        let per_symbol: Vec<(bool, Vec<u32>)> = samples
            .par_chunks_exact(n * stride)
            .map_init(
                || vec![Complex32::new(0.0, 0.0); n],
                |buffer, window| {
                    // Only the leading symbol of each stride carries data; the
                    // blanks after it hold cover audio and no pilots.
                    for (slot, &sample) in buffer.iter_mut().zip(&window[..n]) {
                        *slot = Complex32::new(sample, 0.0);
                    }
                    self.forward.process(buffer);

                    // Recover the gain from the pilots alone. Their transmitted
                    // magnitude is a fixed property of the constellation, so
                    // this measurement is independent of what the payload
                    // happens to be -- including in a partly filled final
                    // symbol, which is where a power-based estimate breaks.
                    let mut pilot_sum = 0.0f32;
                    let mut pilot_count = 0usize;
                    for offset in (0..bins).step_by(PILOT_STRIDE) {
                        pilot_sum += buffer[data_base + offset].norm();
                        pilot_count += 1;
                    }

                    let gain = if pilot_count > 0 {
                        pilot_sum / pilot_count as f32 / pilot_magnitude
                    } else {
                        0.0
                    };

                    if gain <= 0.0 || !gain.is_finite() {
                        return (false, Vec::new());
                    }

                    let values = (0..bins)
                        .filter(|&offset| !OfdmConfig::is_pilot(offset))
                        .map(|offset| {
                            let bin = data_base + offset;
                            self.qam.demap(buffer[bin] / gain)
                        })
                        .collect();
                    (true, values)
                },
            )
            .collect();

        for (live, _) in &per_symbol {
            any_signal |= *live;
        }
        if !any_signal {
            return Err(DemodError::NoSignal);
        }

        let mut writer = BitWriter::with_capacity(symbol_count * self.bytes_per_symbol() + 1);
        for (_, symbol) in &per_symbol {
            for &value in symbol {
                writer.write(value, self.config.bits_per_bin);
            }
        }

        Ok(writer.finish())
    }
}

/// Deterministic +-1 pattern that keeps pilots from summing coherently.
///
/// Both ends derive it from the bin index alone, so nothing has to be
/// transmitted, and the receiver only uses pilot *magnitude* anyway.
#[inline]
fn pilot_sign(bin: usize) -> f32 {
    if (bin.wrapping_mul(2_654_435_761) >> 16) & 1 == 0 {
        1.0
    } else {
        -1.0
    }
}

/// Scale a signal so its largest excursion sits exactly at `amplitude`.
///
/// Exact rather than a crest-factor guess: this encoder is offline and holds
/// the whole carrier, so it can measure the peak instead of budgeting for one.
fn normalise_peak(samples: &mut [f32], amplitude: f32) {
    let peak = samples
        .par_iter()
        .fold(|| 0.0f32, |acc, &s| acc.max(s.abs()))
        .reduce(|| 0.0f32, f32::max);
    if peak > 0.0 {
        let scale = amplitude / peak;
        samples.par_iter_mut().for_each(|sample| *sample *= scale);
    }
}

/// Root-mean-square level of a signal.
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}
