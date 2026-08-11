//! OFDM waveform conformance.
//!
//! The link has ~40 dB of unused margin at every supported constellation order,
//! so these assert *exact* recovery rather than a tolerable error rate. Any
//! error here is a logic bug, not noise.

use audio_modem_core::modem::ofdm::{OfdmConfig, OfdmModem};
use audio_modem_core::modem::qam::{gray_decode, gray_encode, Qam, MAX_BITS_PER_POINT};
use audio_modem_core::{from_i16, to_i16, Carrier, Plan, Profile};

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8
        })
        .collect()
}

fn carries(config: OfdmConfig, payload: &[u8]) -> bool {
    let modem = OfdmModem::new(config).unwrap();
    let samples = modem.modulate(payload);
    let back = modem.demodulate(&from_i16(&to_i16(&samples))).unwrap();
    back.len() >= payload.len() && back[..payload.len()] == payload[..]
}

// ---------------------------------------------------------------------------
// Constellation
// ---------------------------------------------------------------------------

#[test]
fn gray_coding_round_trips_and_neighbours_differ_by_one_bit() {
    for bits in [2u32, 4, 6, 8, 10, 12, 14, 20] {
        let levels = 1u32 << (bits / 2);
        for index in 0..levels {
            assert_eq!(gray_decode(gray_encode(index)), index);
        }
        // The defining property: adjacent amplitudes differ in exactly one bit,
        // so a decision that lands one region off costs one bit, not several.
        for index in 1..levels {
            let differing = (gray_encode(index) ^ gray_encode(index - 1)).count_ones();
            assert_eq!(
                differing,
                1,
                "levels {index} and {} at {bits} bits",
                index - 1
            );
        }
    }
}

#[test]
fn every_constellation_point_round_trips() {
    for bits in [2u32, 4, 6, 8, 10, 12, 14, 16] {
        let qam = Qam::new(bits).unwrap();
        for value in 0..qam.order() {
            assert_eq!(qam.demap(qam.map(value)), value, "{bits}-bit point {value}");
        }
    }
}

#[test]
fn odd_constellation_orders_are_refused() {
    for bits in [0u32, 1, 3, 5, 7, 21] {
        assert!(Qam::new(bits).is_none(), "{bits} should be refused");
    }
}

#[test]
fn the_constellation_ceiling_is_where_quantisation_puts_it() {
    // The limit is the 16-bit container, not the modulation. 20 bits per
    // subcarrier recovers exactly; 22 does not, and the boundary is sharp
    // because the impairment is deterministic quantisation rather than noise.
    assert!(Qam::new(MAX_BITS_PER_POINT).is_some());
    assert!(Qam::new(MAX_BITS_PER_POINT + 2).is_none());

    let config = OfdmConfig {
        fft_size: 512,
        base_bin: 8,
        top_bin: 250,
        bits_per_bin: MAX_BITS_PER_POINT,
        ..OfdmConfig::default()
    };
    for len in [91usize, 1652, 60_000] {
        assert!(
            carries(config, &pseudo_random(len, len as u64)),
            "{MAX_BITS_PER_POINT}-bit subcarriers failed at length {len}"
        );
    }

    // ...and it really is denser than the default, which is what makes the
    // margin/size tradeoff a real choice rather than a theoretical one.
    let dense_default = OfdmConfig {
        bits_per_bin: 12,
        ..config
    };
    assert!(config.bit_rate() > 1.5 * dense_default.bit_rate());
}

// ---------------------------------------------------------------------------
// Waveform
// ---------------------------------------------------------------------------

#[test]
fn the_carrier_never_clips() {
    // Peak normalisation is exact rather than a crest-factor guess, so the
    // requested amplitude is hit to the last sample and clipping is structurally
    // impossible. An earlier 4-sigma guess overshot full scale by 1.8x.
    for bits in [8u32, 12, 14] {
        let config = OfdmConfig {
            fft_size: 512,
            base_bin: 8,
            top_bin: 250,
            bits_per_bin: bits,
            amplitude: 0.9,
            ..OfdmConfig::default()
        };
        let modem = OfdmModem::new(config).unwrap();
        let samples = modem.modulate(&pseudo_random(200_000, bits as u64));
        let peak = samples.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));

        assert!((peak - 0.9).abs() < 1e-5, "peak {peak} at {bits} bits");
        assert!(
            !samples.iter().any(|s| s.abs() > 1.0),
            "clipped at {bits} bits"
        );
    }
}

#[test]
fn a_partially_filled_final_symbol_still_decodes() {
    // Payload bits rarely land on a symbol boundary, so the last symbol is
    // usually padded. Those padding bins map to the outermost corner point,
    // which tripled the symbol's power and broke every power-based gain
    // estimate -- the bug the pilot subcarriers exist to prevent.
    let config = OfdmConfig {
        fft_size: 512,
        base_bin: 8,
        top_bin: 250,
        bits_per_bin: 12,
        ..OfdmConfig::default()
    };
    let full = config.bits_per_symbol() / 8;

    for len in [1usize, 7, 64, 92, full - 1, full, full + 1, full * 2 + 3] {
        assert!(
            carries(config, &pseudo_random(len, len as u64)),
            "failed at payload length {len}"
        );
    }
}

#[test]
fn every_supported_geometry_carries_data_exactly() {
    let mut checked = 0;
    for (fft_size, base_bin, top_bin) in [(256usize, 16usize, 110usize), (512, 8, 250)] {
        for bits_per_bin in [2u32, 4, 6, 8, 10, 12, 14] {
            let config = OfdmConfig {
                fft_size,
                base_bin,
                top_bin,
                bits_per_bin,
                ..OfdmConfig::default()
            };
            for len in [0usize, 1, 91, 1652, 20_000] {
                assert!(
                    carries(
                        config,
                        &pseudo_random(len, len as u64 + bits_per_bin as u64)
                    ),
                    "N={fft_size} qbits={bits_per_bin} len={len}"
                );
                checked += 1;
            }
        }
    }
    assert!(checked >= 70);
}

#[test]
fn decoding_is_immune_to_a_uniform_level_change() {
    // Gain is recovered from pilots, so a carrier that has been volume
    // normalised or re-encoded at another level still decodes.
    let config = OfdmConfig {
        fft_size: 512,
        base_bin: 8,
        top_bin: 250,
        bits_per_bin: 12,
        ..OfdmConfig::default()
    };
    let modem = OfdmModem::new(config).unwrap();
    let payload = pseudo_random(20_000, 3);
    let samples = modem.modulate(&payload);

    for factor in [0.1f32, 0.5, 2.0] {
        let scaled: Vec<f32> = samples.iter().map(|s| s * factor).collect();
        let back = modem.demodulate(&scaled).unwrap();
        assert_eq!(&back[..payload.len()], &payload[..], "at gain {factor}");
    }
}

#[test]
fn trailing_silence_does_not_disturb_the_decode() {
    // Containers pad to a whole number of blocks. Because gain is measured per
    // symbol from its own pilots, silent padding cannot skew a live symbol.
    let config = OfdmConfig {
        fft_size: 512,
        base_bin: 8,
        top_bin: 250,
        bits_per_bin: 12,
        ..OfdmConfig::default()
    };
    let modem = OfdmModem::new(config).unwrap();
    let payload = pseudo_random(1652, 11);

    let mut samples = modem.modulate(&payload);
    samples.resize(samples.len().next_multiple_of(4096), 0.0);

    let back = modem.demodulate(&samples).unwrap();
    assert_eq!(&back[..payload.len()], &payload[..]);
}

#[test]
fn silence_alone_is_reported_rather_than_decoded() {
    let modem = OfdmModem::new(OfdmConfig::default()).unwrap();
    assert!(modem.demodulate(&vec![0.0; 4096]).is_err());
}

// ---------------------------------------------------------------------------
// Density
// ---------------------------------------------------------------------------

#[test]
fn ofdm_is_dramatically_denser_than_fsk() {
    let fsk = Profile::Standard.plan();
    let dense = Profile::Dense.plan();

    // FSK lights one tone in sixteen; OFDM fills every bin in every symbol.
    // The whole point of the waveform is that this is worth ~60x, not 2x.
    let ratio = dense.bit_rate() / fsk.bit_rate();
    assert!(ratio > 50.0, "dense is only {ratio:.1}x standard");

    // Raw PCM expansion per payload byte: 16 bits per sample divided by the
    // bits each sample carries.
    let expansion = 16.0 / (dense.bit_rate() / f64::from(dense.sample_rate()));
    assert!(
        expansion < 3.5,
        "dense expands {expansion:.2}x raw PCM, expected under 3.5x"
    );
}

#[test]
fn the_dense_profile_round_trips_through_the_carrier_abstraction() {
    let plan = Profile::Dense.plan();
    assert!(matches!(plan, Plan::Ofdm(_)));

    let modem = Carrier::new(plan).unwrap();
    let payload = pseudo_random(50_000, 99);
    let samples = modem.modulate(&payload);
    let back = modem.demodulate(&from_i16(&to_i16(&samples))).unwrap();

    assert_eq!(&back[..payload.len()], &payload[..]);
    assert_eq!(modem.plan(), plan);
}
