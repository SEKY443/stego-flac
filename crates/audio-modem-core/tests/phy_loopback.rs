//! Phase 1 PHY conformance tests.
//!
//! These verify the three properties the bin-alignment design claims —
//! orthogonality, zero leakage, phase continuity — rather than only checking
//! that bytes survive the round trip.

use std::f64::consts::TAU;

use audio_modem_core::modem::goertzel::goertzel_power;
use audio_modem_core::{from_i16, to_i16, ConfigError, DemodError, FskModem, ModemConfig};

/// Deterministic xorshift64*, so failures reproduce without a `rand` dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}

/// Naive single-bin DFT power, used as ground truth for the Goertzel kernel.
fn reference_bin_power(window: &[f32], bin: usize) -> f64 {
    let n = window.len();
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (index, &sample) in window.iter().enumerate() {
        let angle = TAU * bin as f64 * index as f64 / n as f64;
        re += f64::from(sample) * angle.cos();
        im -= f64::from(sample) * angle.sin();
    }
    re * re + im * im
}

// ---------------------------------------------------------------------------
// Tone plan
// ---------------------------------------------------------------------------

#[test]
fn default_plan_is_valid_and_matches_documented_numbers() {
    let config = ModemConfig::default();
    config.validate().expect("default plan must validate");

    assert_eq!(config.tone_count(), 16);
    assert_eq!(config.symbols_per_byte(), 2);
    assert_eq!(config.symbol_rate(), 500.0);
    assert_eq!(config.bit_rate(), 2000.0);
    assert_eq!(config.tone_hz(0), 2000.0);
    assert_eq!(config.tone_hz(15), 9500.0);
    assert_eq!(config.highest_bin(), 19);
    // Strictly inside Nyquist (bin 24 at N = 48).
    assert!(config.highest_bin() < config.samples_per_symbol / 2);
}

#[test]
fn every_tone_completes_an_integer_number_of_cycles_per_symbol() {
    let config = ModemConfig::default();
    let period = config.samples_per_symbol as f64 / f64::from(config.sample_rate);

    for symbol in 0..config.tone_count() {
        let cycles = config.tone_hz(symbol) * period;
        assert!(
            (cycles - cycles.round()).abs() < 1e-12,
            "tone {symbol} at {} Hz completes {cycles} cycles per symbol, not an integer",
            config.tone_hz(symbol)
        );
        // The cycle count *is* the bin index; that identity is the whole trick.
        assert_eq!(cycles.round() as usize, config.tone_bin(symbol));
    }
}

#[test]
fn symbols_begin_and_end_at_zero_crossings() {
    let config = ModemConfig::default();
    let modem = FskModem::new(config).unwrap();
    let n = config.samples_per_symbol;

    // Concatenating symbols in a deliberately jumpy order must not produce a
    // step discontinuity, because each waveform starts and ends at phase zero.
    let payload: Vec<u8> = vec![0x0F, 0xF0, 0x18, 0x81];
    let samples = modem.modulate(&payload);

    for boundary in (0..samples.len()).step_by(n) {
        assert!(
            samples[boundary].abs() < 1e-6,
            "symbol starting at sample {boundary} does not begin at a zero crossing: {}",
            samples[boundary]
        );
    }
}

// ---------------------------------------------------------------------------
// Detector
// ---------------------------------------------------------------------------

#[test]
fn goertzel_agrees_with_a_naive_dft() {
    let config = ModemConfig::default();
    let modem = FskModem::new(config).unwrap();
    let n = config.samples_per_symbol;

    let samples = modem.modulate(&[0x3C]);
    let window = &samples[..n];

    for symbol in 0..config.tone_count() {
        let bin = config.tone_bin(symbol);
        let coeff = 2.0 * (TAU * bin as f64 / n as f64).cos();
        let goertzel = goertzel_power(window, coeff);
        let reference = reference_bin_power(window, bin);

        let scale = reference.max(1.0);
        assert!(
            (goertzel - reference).abs() / scale < 1e-9,
            "bin {bin}: goertzel {goertzel} vs reference {reference}"
        );
    }
}

#[test]
fn a_pure_symbol_leaks_no_energy_into_neighbouring_tones() {
    let config = ModemConfig::default();
    let modem = FskModem::new(config).unwrap();
    let n = config.samples_per_symbol;

    for symbol in 0..config.tone_count() {
        // 0x00, 0x11, 0x22 ... — both nibbles equal, so the first window is a
        // pure tone for `symbol`.
        let byte = (symbol as u8) << 4 | symbol as u8;
        let samples = modem.modulate(&[byte]);
        let powers = modem.tone_powers(&samples[..n]);

        let wanted = powers[symbol];
        let worst_other = powers
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != symbol)
            .map(|(_, &p)| p)
            .fold(0.0f64, f64::max);

        assert!(
            wanted / worst_other.max(f64::MIN_POSITIVE) > 1e9,
            "tone {symbol}: wanted {wanted}, worst neighbour {worst_other} \
             (rejection {:.1} dB)",
            10.0 * (wanted / worst_other.max(f64::MIN_POSITIVE)).log10()
        );
    }
}

// ---------------------------------------------------------------------------
// Loopback
// ---------------------------------------------------------------------------

#[test]
fn loopback_is_lossless_for_every_supported_symbol_size() {
    for bits_per_symbol in [1u32, 2, 4] {
        let config = ModemConfig {
            bits_per_symbol,
            ..ModemConfig::default()
        };
        let modem = FskModem::new(config).unwrap_or_else(|error| {
            panic!("plan with bits_per_symbol={bits_per_symbol} rejected: {error}")
        });

        let payload = Rng::new(0xA5A5_0000 | u64::from(bits_per_symbol)).bytes(4096);
        let samples = modem.modulate(&payload);

        assert_eq!(samples.len(), modem.modulated_len(payload.len()));
        assert_eq!(
            modem.demodulate(&samples).unwrap(),
            payload,
            "loopback failed at {bits_per_symbol} bit/symbol"
        );
    }
}

#[test]
fn loopback_survives_i16_container_quantisation() {
    let modem = FskModem::new(ModemConfig::default()).unwrap();
    let payload = Rng::new(0x00C0_FFEE).bytes(8192);

    let samples = modem.modulate(&payload);
    // Exactly what the FLAC container will do to the signal.
    let round_tripped = from_i16(&to_i16(&samples));

    assert_eq!(modem.demodulate(&round_tripped).unwrap(), payload);
}

#[test]
fn parallel_and_serial_decode_paths_agree() {
    let modem = FskModem::new(ModemConfig::default()).unwrap();

    // 512 symbols is the rayon threshold; straddle it in both directions.
    for payload_len in [8usize, 255, 256, 257, 20_000] {
        let payload = Rng::new(payload_len as u64 + 7).bytes(payload_len);
        let samples = modem.modulate(&payload);
        assert_eq!(
            modem.demodulate(&samples).unwrap(),
            payload,
            "mismatch at payload_len={payload_len}"
        );
    }
}

#[test]
fn empty_payload_round_trips_to_empty() {
    let modem = FskModem::new(ModemConfig::default()).unwrap();
    let samples = modem.modulate(&[]);
    assert!(samples.is_empty());
    assert!(modem.demodulate(&samples).unwrap().is_empty());
}

#[test]
fn all_byte_values_survive() {
    let modem = FskModem::new(ModemConfig::default()).unwrap();
    let payload: Vec<u8> = (0..=255u8).collect();
    let samples = modem.modulate(&payload);
    assert_eq!(modem.demodulate(&samples).unwrap(), payload);
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[test]
fn ragged_sample_buffer_is_rejected() {
    let modem = FskModem::new(ModemConfig::default()).unwrap();
    let mut samples = modem.modulate(b"abcd");
    samples.truncate(samples.len() - 3);

    assert_eq!(
        modem.demodulate(&samples),
        Err(DemodError::RaggedSymbolBoundary {
            len: samples.len(),
            samples_per_symbol: 48,
            remainder: 45,
        })
    );
}

#[test]
fn whole_symbols_that_do_not_fill_a_byte_are_rejected() {
    let modem = FskModem::new(ModemConfig::default()).unwrap();
    let mut samples = modem.modulate(b"ab");
    // Drop one whole symbol: aligned, but now an odd number of nibbles.
    samples.truncate(samples.len() - 48);

    assert_eq!(
        modem.demodulate(&samples),
        Err(DemodError::RaggedByteBoundary {
            symbols: 3,
            symbols_per_byte: 2,
            remainder: 1,
        })
    );
}

#[test]
fn tone_plans_that_reach_nyquist_are_rejected() {
    // 32 tones at 500 Hz spacing needs bins 4..=35, but N = 48 puts Nyquist at
    // bin 24. Tones above it would alias onto their own images.
    let config = ModemConfig {
        bits_per_symbol: 8,
        ..ModemConfig::default()
    };
    assert!(matches!(
        config.validate(),
        Err(ConfigError::ExceedsNyquist {
            nyquist_bin: 24,
            ..
        })
    ));
}

#[test]
fn non_byte_aligned_symbol_sizes_are_rejected_in_phase_1() {
    for bits_per_symbol in [0u32, 3, 5, 6, 7, 9] {
        let config = ModemConfig {
            bits_per_symbol,
            ..ModemConfig::default()
        };
        assert!(
            matches!(
                config.validate(),
                Err(ConfigError::BitsPerSymbolNotByteAligned { .. })
            ),
            "bits_per_symbol={bits_per_symbol} should be rejected in phase 1"
        );
    }
}

#[test]
fn degenerate_plans_are_rejected() {
    let base = ModemConfig::default();

    let cases: [(ModemConfig, ConfigError); 5] = [
        (
            ModemConfig {
                sample_rate: 0,
                ..base
            },
            ConfigError::ZeroSampleRate,
        ),
        (
            ModemConfig {
                samples_per_symbol: 1,
                ..base
            },
            ConfigError::SymbolTooShort { got: 1 },
        ),
        (
            ModemConfig {
                bin_spacing: 0,
                ..base
            },
            ConfigError::ZeroBinSpacing,
        ),
        (
            ModemConfig {
                base_bin: 0,
                ..base
            },
            ConfigError::DcBaseBin,
        ),
        (
            ModemConfig {
                amplitude: 0.0,
                ..base
            },
            ConfigError::InvalidAmplitude { got: 0.0 },
        ),
    ];

    for (config, expected) in cases {
        assert_eq!(config.validate(), Err(expected));
    }
}
