//! `stego-flac plan`

use anyhow::Result;
use audio_modem_core::{Carrier, Plan, Profile};
use serde_json::json;

use crate::cli::{OutputArgs, PlanArgs};
use crate::commands::{human_bytes, human_duration};

pub fn run(args: &PlanArgs, output: &OutputArgs) -> Result<()> {
    let plan = args.resolve(None)?;
    let modem = Carrier::new(plan)?;
    let (low_hz, high_hz) = plan.band_hz();

    if output.json {
        return print_json(&plan, &modem, low_hz, high_hz);
    }

    println!("{}", plan.describe());
    println!();
    println!("  sample rate        {} Hz", plan.sample_rate());
    println!("  occupied band      {low_hz:.0} Hz .. {high_hz:.0} Hz");
    println!("  amplitude          {:.2} full scale", plan.amplitude());

    match plan {
        Plan::Fsk(config) => {
            println!(
                "  symbol length      {} samples ({:.3} ms)",
                config.samples_per_symbol,
                1000.0 / config.symbol_rate()
            );
            println!("  symbol rate        {:.1} baud", config.symbol_rate());
            println!("  bin width          {:.1} Hz", config.symbol_rate());
            println!(
                "  tones              {} on bins {}..={} step {}",
                config.tone_count(),
                config.base_bin,
                config.highest_bin(),
                config.bin_spacing
            );
            println!(
                "  bits per symbol    {} (one of {} tones active)",
                config.bits_per_symbol,
                config.tone_count()
            );
        }
        Plan::Ofdm(config) => {
            println!(
                "  symbol length      {} samples ({:.3} ms)",
                config.fft_size,
                1000.0 / config.symbol_rate()
            );
            println!("  symbol rate        {:.1} baud", config.symbol_rate());
            println!("  bin width          {:.2} Hz", config.bin_width_hz());
            println!(
                "  subcarriers        {} on bins {}..={} (all active)",
                config.active_bins(),
                config.base_bin,
                config.top_bin
            );
            println!(
                "  constellation      {}-QAM, {} bits per subcarrier",
                1u32 << config.bits_per_bin,
                config.bits_per_bin
            );
            println!(
                "  bits per symbol    {} ({} bytes)",
                config.bits_per_symbol(),
                config.bits_per_symbol() / 8
            );
        }
    }

    println!(
        "  throughput         {:.0} bit/s ({} /s)",
        plan.bit_rate(),
        human_bytes((plan.bit_rate() / 8.0) as u64)
    );
    println!(
        "  carrier expansion  {:.2}x raw PCM per payload byte",
        16.0 / (plan.bit_rate() / f64::from(plan.sample_rate()))
    );

    println!();
    println!("  carrier duration for a given payload (before compression):");
    for size in [1_024u64, 262_144, 20_000_000] {
        println!(
            "    {:>9}        {}",
            human_bytes(size),
            human_duration(modem.duration_secs(size as usize))
        );
    }

    println!();
    println!("  presets:");
    for profile in Profile::ALL {
        let other = profile.plan();
        println!(
            "    {:<9} {:>9.0} bit/s  {}",
            profile.name(),
            other.bit_rate(),
            other.describe()
        );
    }

    Ok(())
}

fn print_json(plan: &Plan, modem: &Carrier, low_hz: f64, high_hz: f64) -> Result<()> {
    let waveform = match plan {
        Plan::Fsk(config) => json!({
            "mode": "fsk",
            "symbol_length_samples": config.samples_per_symbol,
            "symbol_rate_baud": config.symbol_rate(),
            "bin_width_hz": config.symbol_rate(),
            "tone_count": config.tone_count(),
            "tone_bins": [config.base_bin, config.highest_bin()],
            "bin_spacing": config.bin_spacing,
            "bits_per_symbol": config.bits_per_symbol,
        }),
        Plan::Ofdm(config) => json!({
            "mode": "ofdm",
            "fft_size": config.fft_size,
            "symbol_rate_baud": config.symbol_rate(),
            "bin_width_hz": config.bin_width_hz(),
            "subcarriers": config.active_bins(),
            "subcarrier_bins": [config.base_bin, config.top_bin],
            "constellation_bits": config.bits_per_bin,
            "bits_per_symbol": config.bits_per_symbol(),
        }),
    };

    let duration_table: Vec<_> = [1_024u64, 262_144, 20_000_000]
        .into_iter()
        .map(|size| {
            json!({
                "payload_bytes": size,
                "duration_secs": modem.duration_secs(size as usize),
            })
        })
        .collect();

    let presets: Vec<_> = Profile::ALL
        .into_iter()
        .map(|profile| {
            let other = profile.plan();
            json!({
                "name": profile.name(),
                "bit_rate": other.bit_rate(),
                "description": other.describe(),
            })
        })
        .collect();

    let out = json!({
        "description": plan.describe(),
        "sample_rate_hz": plan.sample_rate(),
        "band_hz": [low_hz, high_hz],
        "amplitude": plan.amplitude(),
        "waveform": waveform,
        "bit_rate": plan.bit_rate(),
        "carrier_expansion_ratio": 16.0 / (plan.bit_rate() / f64::from(plan.sample_rate())),
        "duration_for_payload": duration_table,
        "presets": presets,
    });

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
