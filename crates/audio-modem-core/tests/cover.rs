//! Cover-audio mode: audible camouflage that cannot touch the data.

use audio_modem_core::modem::ofdm::{CoverPlan, OfdmConfig, OfdmModem};
use audio_modem_core::{from_i16, to_i16, Plan, Profile};

fn payload(len: usize, seed: u64) -> Vec<u8> {
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

/// Formant-like tones with vibrato, standing in for speech.
fn voice(samples: usize, sample_rate: f32) -> Vec<f32> {
    (0..samples)
        .map(|i| {
            let t = i as f32 / sample_rate;
            let envelope = 0.6 + 0.4 * (std::f32::consts::TAU * 3.0 * t).sin();
            envelope
                * ((std::f32::consts::TAU * 320.0 * t).sin() * 0.6
                    + (std::f32::consts::TAU * 900.0 * t).sin() * 0.3
                    + (std::f32::consts::TAU * 2400.0 * t).sin() * 0.15)
                * 0.5
        })
        .collect()
}

fn covered(attenuation_db: f32, bits_per_bin: u32) -> OfdmConfig {
    let band = CoverPlan::telephone_band(24_000, 512);
    OfdmConfig {
        fft_size: 512,
        base_bin: band.base_bin,
        top_bin: 250,
        bits_per_bin,
        cover_top: Some(band.top_bin),
        data_attenuation_db: attenuation_db,
        ..OfdmConfig::default()
    }
}

#[test]
fn the_band_split_follows_the_ear() {
    // 300-3400 Hz: where the hearing threshold bottoms out and where essentially
    // all speech intelligibility lives. It is the telephone band for a reason.
    let band = CoverPlan::telephone_band(24_000, 512);
    let width = 24_000.0 / 512.0;

    assert!((band.base_bin as f64 * width - 300.0).abs() < width);
    assert!((band.top_bin as f64 * width - 3400.0).abs() < width);
    assert!(band.top_bin > band.base_bin);
}

#[test]
fn cover_audio_cannot_corrupt_the_data() {
    // The load-bearing claim. Cover and data occupy disjoint bins, and the
    // demodulator reads only data bins, so there is no interference to bound --
    // there is none at all. Checked across the full attenuation range.
    let data = payload(120_000, 4);
    let cover = voice(24_000 * 3, 24_000.0);

    for attenuation in [0.0f32, 6.0, 12.0, 20.0, 25.0, 30.0, 40.0] {
        let config = covered(attenuation, 12);
        let modem = OfdmModem::new(config).unwrap();
        let carrier = modem.modulate_with_cover(&data, &cover, false);
        let back = modem.demodulate(&from_i16(&to_i16(&carrier))).unwrap();

        assert!(
            back.len() >= data.len() && back[..data.len()] == data[..],
            "cover corrupted the payload at {attenuation} dB"
        );
    }
}

#[test]
fn the_data_can_hide_forty_decibels_under_the_cover() {
    // Measured limit at the default constellation: exact at 40 dB, corrupt at
    // 50. The CLI default is 25, which keeps 15 dB of margin.
    let data = payload(120_000, 8);
    let cover = voice(24_000 * 3, 24_000.0);

    let modem = OfdmModem::new(covered(40.0, 12)).unwrap();
    let carrier = modem.modulate_with_cover(&data, &cover, false);
    let back = modem.demodulate(&from_i16(&to_i16(&carrier))).unwrap();
    assert_eq!(
        &back[..data.len()],
        &data[..],
        "40 dB should still be exact"
    );
}

#[test]
fn reserving_a_cover_band_costs_only_throughput() {
    let plain = OfdmConfig {
        fft_size: 512,
        base_bin: 8,
        top_bin: 250,
        bits_per_bin: 12,
        ..OfdmConfig::default()
    };
    let with_cover = covered(25.0, 12);

    assert!(with_cover.active_bins() < plain.active_bins());
    let retained = with_cover.bit_rate() / plain.bit_rate();
    assert!(
        (0.6..0.85).contains(&retained),
        "expected to keep roughly three quarters of throughput, kept {retained:.2}"
    );

    // And the data band must start above the cover, not overlap it.
    assert_eq!(
        with_cover.data_base_bin(),
        with_cover.cover_top.unwrap() + 1
    );
}

#[test]
fn a_silent_cover_does_not_destroy_the_payload() {
    // Cover level is measured, not assumed, so silence would otherwise scale
    // the data to zero. There is nothing to hide behind, so it falls back to a
    // plain carrier rather than producing an empty one.
    let data = payload(40_000, 12);
    let modem = OfdmModem::new(covered(25.0, 12)).unwrap();

    let carrier = modem.modulate_with_cover(&data, &vec![0.0; 24_000], false);
    let back = modem.demodulate(&from_i16(&to_i16(&carrier))).unwrap();
    assert_eq!(&back[..data.len()], &data[..]);
}

#[test]
fn the_cover_band_travels_in_the_plan() {
    // `decode` has to learn the split from the carrier, or a covered file would
    // need flags to open.
    let mut plan = Profile::Dense.plan();
    let band = CoverPlan::telephone_band(24_000, 512);
    assert!(plan.set_cover(Some(band.top_bin), 25.0));

    let text = plan.to_plan_string();
    assert!(
        text.contains("cover="),
        "plan string omits the cover band: {text}"
    );

    let parsed = Plan::from_plan_string(&text).unwrap();
    assert_eq!(parsed, plan);
    assert!(parsed.cover_band_hz().is_some());
}

#[test]
fn fsk_refuses_a_cover_band() {
    // One tone at a time across the whole band leaves nowhere to put a cover.
    let mut plan = Profile::Standard.plan();
    assert!(!plan.set_cover(Some(72), 25.0));
    assert!(plan.cover_band_hz().is_none());
}

#[test]
fn auto_channels_declines_to_help_small_payloads() {
    // Each lane costs fixed padding, so on a small frame extra channels only
    // add bytes -- measured, a 21.7 KB PDF went 4.56 KB to 10.5 KB at eight.
    let plan = Profile::Dense.plan();

    assert_eq!(plan.auto_channels(2_000), 1);
    assert_eq!(plan.auto_channels(20_000), 1);
    assert!(plan.auto_channels(500_000) > 1);
    assert_eq!(
        plan.auto_channels(50_000_000),
        8,
        "should cap at FLAC's limit"
    );
}

// ---------------------------------------------------------------------------
// Cover modes
// ---------------------------------------------------------------------------

#[test]
fn spreading_stretches_the_payload_over_the_whole_cover() {
    // The point of the mode: a short payload would otherwise cut a recording
    // off mid-phrase. With a stride the data is dealt out evenly instead, and
    // the cover plays to its end.
    let data = payload(20_000, 3);
    let cover = voice(24_000 * 10, 24_000.0);

    let tight = OfdmModem::new(covered(25.0, 12)).unwrap();
    let short = tight.modulate_with_cover(&data, &cover, false);

    let mut config = covered(25.0, 12);
    config.spread = 8;
    let stretched = OfdmModem::new(config).unwrap();
    let long = stretched.modulate_with_cover(&data, &cover, true);

    assert!(
        long.len() >= short.len() * 7,
        "a stride of 8 should lengthen the carrier ~8x, got {} vs {}",
        long.len(),
        short.len()
    );

    // ...and it must still come back exactly.
    let back = stretched.demodulate(&from_i16(&to_i16(&long))).unwrap();
    assert_eq!(&back[..data.len()], &data[..]);
}

#[test]
fn every_stride_round_trips() {
    let data = payload(12_000, 21);
    let cover = voice(24_000 * 12, 24_000.0);

    for spread in [1usize, 2, 3, 8, 17, 49] {
        let mut config = covered(25.0, 12);
        config.spread = spread;
        let modem = OfdmModem::new(config).unwrap();

        let carrier = modem.modulate_with_cover(&data, &cover, false);
        let back = modem.demodulate(&from_i16(&to_i16(&carrier))).unwrap();
        assert!(
            back.len() >= data.len() && back[..data.len()] == data[..],
            "stride {spread} did not round-trip"
        );
    }
}

#[test]
fn filling_the_cover_tail_does_not_disturb_the_payload() {
    // The tail is cover audio with no data and no pilots. The receiver reads
    // those symbols, finds no gain reference, and contributes nothing -- the
    // frame header decides where the payload ended anyway.
    let data = payload(8_000, 5);
    let cover = voice(24_000 * 20, 24_000.0);

    let modem = OfdmModem::new(covered(25.0, 12)).unwrap();
    let filled = modem.modulate_with_cover(&data, &cover, true);

    assert!(
        filled.len() >= 24_000 * 19,
        "the carrier should run nearly the length of the cover"
    );
    let back = modem.demodulate(&from_i16(&to_i16(&filled))).unwrap();
    assert_eq!(&back[..data.len()], &data[..]);
}

#[test]
fn the_stride_travels_in_the_plan() {
    let mut plan = Profile::Dense.plan();
    let band = CoverPlan::telephone_band(24_000, 512);
    assert!(plan.set_cover(Some(band.top_bin), 25.0));
    assert!(plan.set_spread(37));

    let text = plan.to_plan_string();
    assert!(text.contains("spread=37"), "plan omits the stride: {text}");
    assert_eq!(Plan::from_plan_string(&text).unwrap(), plan);
    assert_eq!(Plan::from_plan_string(&text).unwrap().spread(), 37);
}

#[test]
fn auto_quality_widens_the_band_for_small_payloads() {
    // The whole point of the feature: a small frame should end up with more
    // audible bandwidth than a large one, because the bytes it costs are cheap
    // at that size.
    let width = 24_000.0 / 512.0;
    let ceiling = |frame: usize| {
        let mut plan = Profile::Dense.plan();
        plan.set_auto_cover(frame, 25.0).unwrap()
    };

    let small = ceiling(256 << 10);
    let medium = ceiling(16 << 20);
    let large = ceiling(200 << 20);

    assert!(
        small > medium && medium > large,
        "bands should narrow as the frame grows: {small} {medium} {large}"
    );
    // The largest tier is the plain telephone band, to within one bin.
    assert!((large - 3400.0).abs() < width);
}

#[test]
fn auto_quality_is_paid_for_in_throughput() {
    // Bandwidth handed to the cover is bandwidth taken from the data, so the
    // wide tiers must carry strictly less per second. If this ever stopped
    // being true the feature would be free, which would mean it is not doing
    // anything.
    let rate = |frame: usize| {
        let mut plan = Profile::Dense.plan();
        plan.set_auto_cover(frame, 25.0).unwrap();
        plan.bit_rate()
    };

    assert!(rate(256 << 10) < rate(16 << 20));
    assert!(rate(16 << 20) < rate(200 << 20));
}

#[test]
fn every_auto_tier_still_demodulates() {
    // A wider cover leaves fewer data subcarriers and fewer pilots with them.
    // Each tier has to survive the round trip through i16, or the feature is
    // trading correctness for fidelity.
    let data = payload(6_000, 11);
    let cover = voice(24_000 * 30, 24_000.0);

    for frame in [64 << 10, 8 << 20, 64 << 20] {
        let mut plan = Profile::Dense.plan();
        let ceiling = plan.set_auto_cover(frame, 25.0).unwrap();
        plan.validate()
            .unwrap_or_else(|e| panic!("auto band invalid at frame {frame}: {e}"));

        let config = match plan {
            Plan::Ofdm(config) => config,
            Plan::Fsk(_) => unreachable!("dense is OFDM"),
        };
        let modem = OfdmModem::new(config).unwrap();
        let carrier = modem.modulate_with_cover(&data, &cover, false);
        let back = modem
            .demodulate(&from_i16(&to_i16(&carrier)))
            .unwrap_or_else(|e| panic!("tier at {ceiling:.0} Hz failed: {e}"));
        assert_eq!(&back[..data.len()], &data[..], "tier at {ceiling:.0} Hz");
    }
}

#[test]
fn auto_quality_survives_a_narrow_plan() {
    // A plan with little spectrum to give cannot afford the wide tiers. It must
    // step back down rather than hand out a band that leaves no data bins.
    let mut plan = Plan::Ofdm(OfdmConfig {
        fft_size: 512,
        base_bin: 8,
        top_bin: 120,
        bits_per_bin: 12,
        ..OfdmConfig::default()
    });
    plan.set_auto_cover(1024, 25.0).unwrap();
    plan.validate().expect("stepped-down band should be valid");

    match plan {
        Plan::Ofdm(config) => assert!(
            config.data_bins() >= 40,
            "left only {} data bins",
            config.data_bins()
        ),
        Plan::Fsk(_) => unreachable!(),
    }
}

#[test]
fn fsk_plans_report_no_cover_band() {
    let mut plan = Profile::Standard.plan();
    assert!(plan.set_auto_cover(1 << 20, 25.0).is_none());
    assert!(plan.set_cover_ceiling(5000.0, 25.0).is_none());
}
