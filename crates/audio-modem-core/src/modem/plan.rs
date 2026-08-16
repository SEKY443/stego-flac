//! A tone plan and its modem, across both waveforms.
//!
//! Two physical layers now exist, and they answer different questions:
//!
//! * **M-FSK** puts one tone on at a time. Its spectrogram is a readable line,
//!   its detector needs no amplitude reference, and it degrades gracefully.
//!   It is also spectacularly wasteful — 16 bins to carry 4 bits.
//! * **OFDM** fills every bin in every symbol with a QAM point. It is ~68x
//!   faster and produces files ~60x smaller, at the cost of sounding like
//!   static and needing a gain reference.
//!
//! [`Plan`] is the serialisable description, [`Carrier`] the built modem. The
//! plan is recorded in the carrier's container metadata, so a reader
//! reconstructs the right waveform without being told which one to expect.

use crate::error::{ConfigError, DemodError};
use crate::modem::config::{ModemConfig, PlanParseError};
use crate::modem::fsk::FskModem;
use crate::modem::ofdm::{OfdmConfig, OfdmModem};

/// A complete, serialisable description of a physical layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Plan {
    /// Bin-aligned M-FSK.
    Fsk(ModemConfig),
    /// QAM over orthogonal subcarriers.
    Ofdm(OfdmConfig),
}

impl Default for Plan {
    fn default() -> Self {
        Plan::Ofdm(OfdmConfig::default())
    }
}

impl Plan {
    /// Container sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        match self {
            Plan::Fsk(config) => config.sample_rate,
            Plan::Ofdm(config) => config.sample_rate,
        }
    }

    /// Gross throughput in bits per second.
    pub fn bit_rate(&self) -> f64 {
        match self {
            Plan::Fsk(config) => config.bit_rate(),
            Plan::Ofdm(config) => config.bit_rate(),
        }
    }

    /// Peak amplitude in normalised full scale.
    pub fn amplitude(&self) -> f32 {
        match self {
            Plan::Fsk(config) => config.amplitude,
            Plan::Ofdm(config) => config.amplitude,
        }
    }

    /// Override the amplitude.
    pub fn set_amplitude(&mut self, amplitude: f32) {
        match self {
            Plan::Fsk(config) => config.amplitude = amplitude,
            Plan::Ofdm(config) => config.amplitude = amplitude,
        }
    }

    /// Override the sample rate.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        match self {
            Plan::Fsk(config) => config.sample_rate = sample_rate,
            Plan::Ofdm(config) => config.sample_rate = sample_rate,
        }
    }

    /// Reserve the audible band for cover audio, if this plan can carry one.
    pub fn set_cover(&mut self, cover_top: Option<usize>, attenuation_db: f32) -> bool {
        match self {
            Plan::Ofdm(config) => {
                config.cover_top = cover_top;
                config.data_attenuation_db = attenuation_db;
                true
            }
            Plan::Fsk(_) => false,
        }
    }

    /// Stretch the payload across `spread` times as many symbols.
    ///
    /// Only meaningful with cover audio: it exists so a short payload can be
    /// spread over a longer recording, letting the cover play to its end rather
    /// than being cut off. Returns false for waveforms that cannot carry a
    /// cover in the first place.
    pub fn set_spread(&mut self, spread: usize) -> bool {
        match self {
            Plan::Ofdm(config) => {
                config.spread = spread.max(1);
                true
            }
            Plan::Fsk(_) => false,
        }
    }

    /// Data symbols needed to carry `frame_bytes`, ignoring any stride.
    pub fn symbols_for(&self, frame_bytes: usize) -> usize {
        match self {
            Plan::Ofdm(config) => (frame_bytes * 8).div_ceil(config.bits_per_symbol()),
            Plan::Fsk(config) => (frame_bytes * config.symbols_per_byte()).div_ceil(1),
        }
    }

    /// How many symbols each data symbol occupies.
    pub fn spread(&self) -> usize {
        match self {
            Plan::Ofdm(config) => config.stride(),
            Plan::Fsk(_) => 1,
        }
    }

    /// Reserve the widest cover band a frame of `frame_bytes` can afford.
    ///
    /// Returns the band's upper edge in Hz, or `None` for waveforms that cannot
    /// carry a cover. See [`crate::modem::ofdm::CoverPlan::auto`] for why size
    /// decides this.
    pub fn set_auto_cover(&mut self, frame_bytes: usize, attenuation_db: f64) -> Option<f64> {
        match self {
            Plan::Ofdm(config) => {
                let band = crate::modem::ofdm::CoverPlan::auto(config, frame_bytes);
                config.cover_top = Some(band.top_bin);
                config.data_attenuation_db = attenuation_db as f32;
                Some(config.bin_hz(band.top_bin))
            }
            Plan::Fsk(_) => None,
        }
    }

    /// Reserve a cover band ending at `ceiling_hz`, falling back to the
    /// telephone band if that leaves too little room for data.
    pub fn set_cover_ceiling(&mut self, ceiling_hz: f64, attenuation_db: f64) -> Option<f64> {
        match self {
            Plan::Ofdm(config) => {
                let band = crate::modem::ofdm::CoverPlan::to_ceiling(
                    config.sample_rate,
                    config.fft_size,
                    ceiling_hz,
                );
                config.cover_top = Some(band.top_bin);
                config.data_attenuation_db = attenuation_db as f32;
                Some(config.bin_hz(band.top_bin))
            }
            Plan::Fsk(_) => None,
        }
    }

    /// The cover band, if one is reserved.
    pub fn cover_band_hz(&self) -> Option<(f64, f64)> {
        match self {
            Plan::Ofdm(config) => config
                .cover_top
                .map(|top| (config.bin_hz(config.base_bin), config.bin_hz(top))),
            Plan::Fsk(_) => None,
        }
    }

    /// Choose a channel count for a frame of `frame_bytes`.
    ///
    /// Every lane rounds its last symbol up and the container pads to a whole
    /// block, so each extra channel costs a fixed amount of padding. On a large
    /// payload that is lost in the noise and the duration divides cleanly; on a
    /// small one it dominates — a 21.7 KB PDF measured 4.56 KB at one channel
    /// and 10.5 KB at eight, because the overhead multiplied while the payload
    /// did not.
    ///
    /// So the rule is a padding budget: take as many channels as keep the waste
    /// under one percent of the frame, capped at the eight FLAC allows. Small
    /// payloads land on 1, which is exactly where the measurements say they
    /// belong.
    pub fn auto_channels(&self, frame_bytes: usize) -> usize {
        let per_symbol = match self {
            Plan::Ofdm(config) => config.bits_per_symbol() / 8,
            Plan::Fsk(_) => 1,
        }
        .max(1);

        // A lane wastes half a symbol on average, plus its share of the block.
        let waste_per_lane = per_symbol / 2 + 1;
        let budget = frame_bytes / 100;
        (budget / waste_per_lane).clamp(1, 8)
    }

    /// Lowest and highest occupied frequency in Hz.
    pub fn band_hz(&self) -> (f64, f64) {
        match self {
            Plan::Fsk(config) => (config.tone_hz(0), config.tone_hz(config.tone_count() - 1)),
            Plan::Ofdm(config) => (
                config.bin_hz(config.data_base_bin()),
                config.bin_hz(config.top_bin),
            ),
        }
    }

    /// Short human-readable summary.
    pub fn describe(&self) -> String {
        match self {
            Plan::Fsk(config) => {
                format!("{}-FSK, {} tones", config.tone_count(), config.tone_count())
            }
            Plan::Ofdm(config) => format!(
                "OFDM, {} subcarriers x {}-QAM",
                config.active_bins(),
                1u32 << config.bits_per_bin
            ),
        }
    }

    /// Check every invariant.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Plan::Fsk(config) => config.validate(),
            Plan::Ofdm(config) => config.validate(),
        }
    }

    /// Serialise for storage in container metadata.
    pub fn to_plan_string(&self) -> String {
        match self {
            // The FSK form predates the mode field and is left unprefixed, so
            // carriers written before OFDM existed still parse.
            Plan::Fsk(config) => config.to_plan_string(),
            Plan::Ofdm(config) => config.to_plan_string(),
        }
    }

    /// Parse a plan string.
    ///
    /// A missing `mode` field means FSK, which is what carriers written before
    /// the OFDM waveform existed look like.
    pub fn from_plan_string(text: &str) -> Result<Self, PlanParseError> {
        let mode = text
            .split(';')
            .filter_map(|field| field.split_once('='))
            .find(|(key, _)| key.trim() == "mode")
            .map(|(_, value)| value.trim().to_ascii_lowercase());

        match mode.as_deref() {
            None | Some("fsk") => Ok(Plan::Fsk(ModemConfig::from_plan_string(text)?)),
            Some("ofdm") => Ok(Plan::Ofdm(parse_ofdm(text)?)),
            Some(other) => Err(PlanParseError::UnknownMode(other.to_string())),
        }
    }
}

fn parse_ofdm(text: &str) -> Result<OfdmConfig, PlanParseError> {
    let mut config = OfdmConfig::default();

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
            "mode" => {}
            "fs" => config.sample_rate = value.parse().map_err(|_| bad())?,
            "n" => config.fft_size = value.parse().map_err(|_| bad())?,
            "base" => config.base_bin = value.parse().map_err(|_| bad())?,
            "top" => config.top_bin = value.parse().map_err(|_| bad())?,
            "qbits" => config.bits_per_bin = value.parse().map_err(|_| bad())?,
            "cover" => config.cover_top = Some(value.parse().map_err(|_| bad())?),
            "spread" => config.spread = value.parse().map_err(|_| bad())?,
            "atten" => config.data_attenuation_db = value.parse().map_err(|_| bad())?,
            "amp" => config.amplitude = value.parse().map_err(|_| bad())?,
            _ => {}
        }
    }

    config.validate()?;
    Ok(config)
}

/// A built modem for one [`Plan`].
#[derive(Debug)]
pub enum Carrier {
    Fsk(FskModem),
    Ofdm(OfdmModem),
}

impl Carrier {
    /// Validate `plan` and build its modem.
    pub fn new(plan: Plan) -> Result<Self, ConfigError> {
        Ok(match plan {
            Plan::Fsk(config) => Carrier::Fsk(FskModem::new(config)?),
            Plan::Ofdm(config) => Carrier::Ofdm(OfdmModem::new(config)?),
        })
    }

    /// The plan this modem was built from.
    pub fn plan(&self) -> Plan {
        match self {
            Carrier::Fsk(modem) => Plan::Fsk(*modem.config()),
            Carrier::Ofdm(modem) => Plan::Ofdm(*modem.config()),
        }
    }

    /// Modulate a frame with audible cover audio mixed into the low band.
    ///
    /// Only OFDM can do this: it needs a spectrum it can partition. FSK lights
    /// one tone at a time across the whole band and has nowhere to put a cover.
    pub fn modulate_with_cover(
        &self,
        payload: &[u8],
        cover: &[f32],
        fill_cover: bool,
    ) -> Option<Vec<f32>> {
        match self {
            Carrier::Ofdm(modem) => Some(modem.modulate_with_cover(payload, cover, fill_cover)),
            Carrier::Fsk(_) => None,
        }
    }

    /// Modulate a frame into normalised `f32` PCM.
    pub fn modulate(&self, payload: &[u8]) -> Vec<f32> {
        match self {
            Carrier::Fsk(modem) => modem.modulate(payload),
            Carrier::Ofdm(modem) => modem.modulate(payload),
        }
    }

    /// Demodulate sample-aligned PCM back into frame bytes.
    pub fn demodulate(&self, samples: &[f32]) -> Result<Vec<u8>, DemodError> {
        match self {
            Carrier::Fsk(modem) => modem.demodulate(samples),
            Carrier::Ofdm(modem) => modem.demodulate(samples),
        }
    }

    /// Samples emitted for a payload of `payload_len` bytes.
    pub fn modulated_len(&self, payload_len: usize) -> usize {
        match self {
            Carrier::Fsk(modem) => modem.modulated_len(payload_len),
            Carrier::Ofdm(modem) => modem.modulated_len(payload_len),
        }
    }

    /// Carrier duration in seconds for a payload of `payload_len` bytes.
    pub fn duration_secs(&self, payload_len: usize) -> f64 {
        match self {
            Carrier::Fsk(modem) => modem.duration_secs(payload_len),
            Carrier::Ofdm(modem) => modem.duration_secs(payload_len),
        }
    }

    /// Modulate `payload` across `channels` interleaved lanes.
    ///
    /// # What extra channels do and do not buy
    ///
    /// Channel is the only axis of the storage tensor that is not already
    /// spoken for. Time, frequency and I/Q are three views of the same samples
    /// — a real signal of `N` samples has exactly `N` degrees of freedom, and
    /// `N/2` complex bins is the same `N` reals — so they cannot be stacked for
    /// extra room. Adding a channel adds genuinely new samples.
    ///
    /// It divides **duration** by the channel count and leaves **file size**
    /// alone, because a channel multiplies capacity per second and bytes per
    /// second by the same factor. Measured on a 2 MB payload: 125.3 s at one
    /// channel, 15.7 s at eight, with the file within 7% either way — and that
    /// residual is peak-normalisation noise, not a channel effect.
    ///
    /// Bytes are dealt round-robin rather than split into contiguous blocks, so
    /// no lane needs to know the payload length to be reassembled. That matters
    /// because the length lives in the frame header, which is itself part of
    /// the payload being split.
    pub fn modulate_interleaved(&self, payload: &[u8], channels: usize) -> Vec<f32> {
        if channels <= 1 {
            return self.modulate(payload);
        }

        let lanes: Vec<Vec<f32>> = (0..channels)
            .map(|lane| {
                let bytes: Vec<u8> = payload
                    .iter()
                    .skip(lane)
                    .step_by(channels)
                    .copied()
                    .collect();
                self.modulate(&bytes)
            })
            .collect();

        let longest = lanes.iter().map(Vec::len).max().unwrap_or(0);
        let mut out = vec![0.0f32; longest * channels];
        for (index, lane) in lanes.iter().enumerate() {
            for (position, &sample) in lane.iter().enumerate() {
                out[position * channels + index] = sample;
            }
        }
        out
    }

    /// Inverse of [`modulate_interleaved`](Self::modulate_interleaved).
    ///
    /// Lanes are demodulated independently and their bytes dealt back in the
    /// same round-robin order. Reassembly stops at the shortest lane, which is
    /// harmless: the frame header declares the payload length and anything past
    /// it was never read.
    pub fn demodulate_interleaved(
        &self,
        samples: &[f32],
        channels: usize,
    ) -> Result<Vec<u8>, DemodError> {
        if channels <= 1 {
            return self.demodulate(samples);
        }

        let per_lane = samples.len() / channels;
        let usable = per_lane - per_lane % self.alignment_samples();
        if usable == 0 {
            return Ok(Vec::new());
        }

        let mut decoded = Vec::with_capacity(channels);
        for lane in 0..channels {
            let pcm: Vec<f32> = (0..usable).map(|i| samples[i * channels + lane]).collect();
            decoded.push(self.demodulate(&pcm)?);
        }

        let shortest = decoded.iter().map(Vec::len).min().unwrap_or(0);
        let mut out = Vec::with_capacity(shortest * channels);
        for index in 0..shortest {
            for lane in &decoded {
                out.push(lane[index]);
            }
        }
        Ok(out)
    }

    /// Sample count that a decodable buffer must be a multiple of.
    ///
    /// FSK needs whole symbols *and* whole bytes, because its byte packing has
    /// no length field of its own. OFDM only needs whole symbols: its bit
    /// packing spans symbols freely and the frame header supplies the length.
    pub fn alignment_samples(&self) -> usize {
        match self {
            Carrier::Fsk(modem) => {
                modem.config().samples_per_symbol * modem.config().symbols_per_byte()
            }
            Carrier::Ofdm(modem) => modem.config().fft_size * modem.config().stride(),
        }
    }
}
