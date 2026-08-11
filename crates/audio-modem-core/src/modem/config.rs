//! Modulation parameters and the tone plan they imply.

use crate::error::ConfigError;

/// Parameters describing a bin-aligned M-FSK tone plan.
///
/// # The bin-alignment constraint
///
/// Every tone frequency is pinned to an exact DFT bin of the symbol window:
///
/// ```text
///   f_i = m_i * sample_rate / samples_per_symbol,     m_i integer
/// ```
///
/// Writing `N = samples_per_symbol` and `T = N / sample_rate` (the symbol
/// period), the spacing between adjacent tones is
///
/// ```text
///   Δf = bin_spacing * sample_rate / N = bin_spacing / T
/// ```
///
/// which is exactly the `Δf = k/T` condition for *non-coherent* orthogonality.
/// Coherent detection would only need `k/2T`, but we detect on magnitude alone
/// (see [`goertzel`](crate::modem::goertzel)), which is phase-blind and so
/// requires the wider spacing. Buying that factor of two back is what a
/// coherent PSK/QAM layer is for; it is not available to an FSK energy
/// detector.
///
/// Bin alignment additionally forces an integer number of cycles per symbol,
/// which is what makes the modulator a table lookup and the leakage zero. See
/// the crate-level docs for the full argument.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModemConfig {
    /// Container sample rate in Hz.
    pub sample_rate: u32,
    /// Symbol length `N` in samples. Sets both the baud rate (`fs/N`) and the
    /// frequency resolution (`fs/N` Hz per bin) — they are the same number,
    /// which is the whole time-frequency tradeoff in one parameter.
    pub samples_per_symbol: usize,
    /// Bits carried per symbol; the tone count is `2^bits_per_symbol`.
    pub bits_per_symbol: u32,
    /// DFT bin of tone 0. Must be >= 1 to stay off DC.
    pub base_bin: usize,
    /// Bin stride between adjacent tones. 1 is the minimum orthogonal spacing;
    /// larger values buy guard bands at a proportional cost in bandwidth.
    pub bin_spacing: usize,
    /// Peak amplitude in normalised full scale.
    pub amplitude: f32,
}

impl Default for ModemConfig {
    /// The phase 1 reference plan: 24 kHz mono, 500 baud, 16-FSK.
    ///
    /// | quantity | value |
    /// |---|---|
    /// | symbol period `T` | 2 ms (`N` = 48) |
    /// | bin width | 500 Hz |
    /// | tones | 16, bins 4..=19 |
    /// | occupied band | 2000 Hz .. 9500 Hz |
    /// | throughput | 2000 bit/s = 250 B/s |
    ///
    /// `bits_per_symbol = 4` is deliberate: one symbol is exactly one nibble,
    /// so two symbols are exactly one byte. No payload byte ever straddles a
    /// symbol boundary, which removes a whole class of off-by-one packing bugs
    /// before the framing layer exists to catch them.
    ///
    /// The band starts at 2 kHz rather than at bin 1 purely for forward
    /// compatibility: low-frequency rumble and HVAC noise are the first things
    /// an acoustic channel adds, and vacating that region now costs nothing.
    ///
    /// `amplitude = 0.25` (-12 dBFS) rather than near full scale. Detection is
    /// an `argmax` over bin powers and therefore completely gain-invariant, so
    /// amplitude is free to choose on other grounds — and two of those favour a
    /// quiet carrier. It is a wall of pure tones in the most piercing region of
    /// human hearing, so a lower level is kinder to anyone who plays it; and
    /// FLAC's residuals scale with amplitude, so a quieter carrier is a smaller
    /// file. Measured on one payload: 0.8 gives 297 KB, 0.25 gives 265 KB,
    /// 0.05 gives 222 KB. Even 0.01 still round-trips, but 0.25 keeps ~78 dB of
    /// margin over the 16-bit noise floor, which leaves room for an acoustic
    /// front end later without revisiting this.
    fn default() -> Self {
        Self {
            sample_rate: 24_000,
            samples_per_symbol: 48,
            bits_per_symbol: 4,
            base_bin: 4,
            bin_spacing: 1,
            amplitude: 0.25,
        }
    }
}

/// A tone plan preset.
///
/// Presets exist because the plan is recorded in the carrier's FLAC metadata,
/// so choosing a non-default one no longer risks producing a file that only
/// works if the reader remembers the right flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// 2000 bit/s. 16 tones from 2000 Hz to 9500 Hz, 2 ms symbols.
    ///
    /// Wide tone spacing and long symbols; the most forgiving plan and the one
    /// whose spectrogram is easiest to read when debugging.
    Standard,
    /// 4000 bit/s. 4 tones from 4000 Hz to 10000 Hz, 0.5 ms symbols.
    ///
    /// Halves both the carrier duration and the file size. This is the fastest
    /// plan reachable under the "`bits_per_symbol` divides 8" restriction:
    /// throughput is `(fs/N)·log₂(M)` subject to `M` tones fitting below
    /// Nyquist, and sweeping the legal `(N, bits)` pairs puts the maximum at
    /// `N = 12`, 2 bits/symbol. Notably, allowing 3 bits/symbol would *not*
    /// beat it (`N = 20` yields 3600 bit/s), so lifting that restriction buys
    /// nothing here.
    Fast,
    /// 136688 bit/s. 243 OFDM subcarriers of 4096-QAM, 750-23437 Hz.
    ///
    /// Roughly 68x the throughput of `standard` and ~60x smaller files, because
    /// every subcarrier carries data in every symbol instead of one tone in
    /// sixteen. Sounds like static rather than a modem; that is what filling
    /// the time-frequency plane costs.
    Dense,
    /// 170250 bit/s. Like `dense` but 65536-QAM: ~25% smaller again.
    ///
    /// Exact recovery is measured to hold up to 20 bits per subcarrier and to
    /// collapse at 22, so 16 sits comfortably inside the limit while spending
    /// some of the margin `dense` keeps in reserve. Prefer it when the carrier
    /// will be stored and read back untouched; prefer `dense` if the file may
    /// be converted, resampled, or passed through any gain stage.
    Compact,
}

impl Profile {
    /// The plan for this preset.
    pub fn plan(self) -> crate::modem::plan::Plan {
        use crate::modem::plan::Plan;
        match self {
            Profile::Standard => Plan::Fsk(ModemConfig::default()),
            Profile::Fast => Plan::Fsk(ModemConfig {
                samples_per_symbol: 12,
                bits_per_symbol: 2,
                base_bin: 2,
                ..ModemConfig::default()
            }),
            Profile::Dense => Plan::Ofdm(crate::modem::ofdm::OfdmConfig {
                fft_size: 512,
                base_bin: 8,
                top_bin: 250,
                bits_per_bin: 12,
                ..crate::modem::ofdm::OfdmConfig::default()
            }),
            Profile::Compact => Plan::Ofdm(crate::modem::ofdm::OfdmConfig {
                fft_size: 512,
                base_bin: 8,
                top_bin: 250,
                bits_per_bin: 16,
                ..crate::modem::ofdm::OfdmConfig::default()
            }),
        }
    }

    /// Preset name as used on the command line and in metadata.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Standard => "standard",
            Profile::Fast => "fast",
            Profile::Dense => "dense",
            Profile::Compact => "compact",
        }
    }

    /// Every preset, for lookup and display.
    pub const ALL: [Profile; 4] = [
        Profile::Standard,
        Profile::Fast,
        Profile::Dense,
        Profile::Compact,
    ];
}

impl ModemConfig {
    /// Number of distinct tones, `M = 2^bits_per_symbol`.
    ///
    /// Only meaningful once [`validate`](Self::validate) has passed;
    /// `bits_per_symbol` is bounded there.
    #[inline]
    pub fn tone_count(&self) -> usize {
        1usize << self.bits_per_symbol
    }

    /// Symbols packed into one payload byte.
    #[inline]
    pub fn symbols_per_byte(&self) -> usize {
        (8 / self.bits_per_symbol) as usize
    }

    /// DFT bin index of tone `symbol`.
    #[inline]
    pub fn tone_bin(&self, symbol: usize) -> usize {
        self.base_bin + symbol * self.bin_spacing
    }

    /// Centre frequency of tone `symbol` in Hz. Exact by construction.
    #[inline]
    pub fn tone_hz(&self, symbol: usize) -> f64 {
        self.tone_bin(symbol) as f64 * self.sample_rate as f64 / self.samples_per_symbol as f64
    }

    /// Highest occupied DFT bin.
    #[inline]
    pub fn highest_bin(&self) -> usize {
        self.tone_bin(self.tone_count().saturating_sub(1))
    }

    /// Symbol rate in baud, equal to the bin width in Hz.
    #[inline]
    pub fn symbol_rate(&self) -> f64 {
        self.sample_rate as f64 / self.samples_per_symbol as f64
    }

    /// Gross channel throughput in bits per second, before framing and FEC.
    #[inline]
    pub fn bit_rate(&self) -> f64 {
        self.symbol_rate() * f64::from(self.bits_per_symbol)
    }

    /// Occupied bandwidth in Hz, measured tone-centre to tone-centre plus one
    /// bin of skirt on each side.
    #[inline]
    pub fn occupied_bandwidth_hz(&self) -> f64 {
        let bins = self.highest_bin() - self.base_bin + 2;
        bins as f64 * self.symbol_rate()
    }

    /// Serialise the tone plan as a compact, machine-readable string.
    ///
    /// This is what gets written into the carrier's FLAC metadata. It exists so
    /// a reader can reconstruct the plan without being told it out of band —
    /// the plan cannot live in the modulated audio, because you need it in
    /// order to demodulate anything at all, but a metadata block sits outside
    /// the audio and is readable directly.
    ///
    /// Amplitude is included for completeness but is not needed to decode:
    /// detection is gain-invariant.
    pub fn to_plan_string(&self) -> String {
        format!(
            "fs={};n={};bits={};base={};step={};amp={}",
            self.sample_rate,
            self.samples_per_symbol,
            self.bits_per_symbol,
            self.base_bin,
            self.bin_spacing,
            self.amplitude
        )
    }

    /// Parse a plan produced by [`to_plan_string`](Self::to_plan_string).
    ///
    /// Unknown keys are ignored so a newer encoder can add fields without
    /// making its carriers unreadable here; missing keys keep their default.
    /// The result is validated, so a malformed plan fails at parse time rather
    /// than producing a modem that silently decodes noise.
    pub fn from_plan_string(text: &str) -> Result<Self, PlanParseError> {
        let mut config = ModemConfig::default();

        for field in text.split(';').filter(|f| !f.trim().is_empty()) {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| PlanParseError::Malformed(field.to_string()))?;

            let key = key.trim();
            let value = value.trim();
            let bad = || PlanParseError::BadValue {
                key: key.to_string(),
                value: value.to_string(),
            };

            match key {
                "fs" => config.sample_rate = value.parse().map_err(|_| bad())?,
                "n" => config.samples_per_symbol = value.parse().map_err(|_| bad())?,
                "bits" => config.bits_per_symbol = value.parse().map_err(|_| bad())?,
                "base" => config.base_bin = value.parse().map_err(|_| bad())?,
                "step" => config.bin_spacing = value.parse().map_err(|_| bad())?,
                "amp" => config.amplitude = value.parse().map_err(|_| bad())?,
                _ => {}
            }
        }

        config.validate()?;
        Ok(config)
    }

    /// Check every invariant the modulator and demodulator rely on.
    ///
    /// Called once by [`FskModem::new`](crate::FskModem::new); everything
    /// downstream of it is total.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sample_rate == 0 {
            return Err(ConfigError::ZeroSampleRate);
        }
        if self.samples_per_symbol < 2 {
            return Err(ConfigError::SymbolTooShort {
                got: self.samples_per_symbol,
            });
        }
        // Restricting to divisors of 8 keeps the symbol stream byte-aligned,
        // which is what lets phase 1 skip the framing layer entirely.
        if self.bits_per_symbol == 0 || self.bits_per_symbol > 8 || 8 % self.bits_per_symbol != 0 {
            return Err(ConfigError::BitsPerSymbolNotByteAligned {
                got: self.bits_per_symbol,
            });
        }
        if self.bin_spacing == 0 {
            return Err(ConfigError::ZeroBinSpacing);
        }
        if self.base_bin == 0 {
            return Err(ConfigError::DcBaseBin);
        }
        if !self.amplitude.is_finite() || self.amplitude <= 0.0 || self.amplitude > 1.0 {
            return Err(ConfigError::InvalidAmplitude {
                got: self.amplitude,
            });
        }

        // Strictly below Nyquist. Bin N/2 is real-valued for even N and folds
        // onto itself, so it cannot be told apart from its own image.
        let nyquist_bin = self.samples_per_symbol / 2;
        let highest = self.highest_bin();
        if highest >= nyquist_bin {
            return Err(ConfigError::ExceedsNyquist {
                highest,
                nyquist_bin,
                tones: self.tone_count(),
                base_bin: self.base_bin,
                bin_spacing: self.bin_spacing,
            });
        }

        Ok(())
    }
}

/// A tone plan string that could not be parsed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlanParseError {
    #[error("plan field {0:?} is not `key=value`")]
    Malformed(String),

    #[error("plan field {key}={value:?} is not a valid value")]
    BadValue { key: String, value: String },

    #[error("plan describes an unusable tone plan: {0}")]
    Invalid(#[from] ConfigError),

    #[error("plan names an unknown waveform mode {0:?}")]
    UnknownMode(String),
}
