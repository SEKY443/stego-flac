//! `stego-flac plan`

use anyhow::Result;
use audio_modem_core::{Carrier, Plan, Profile};

use crate::cli::PlanArgs;
use crate::commands::{human_bytes, human_duration};

pub fn run(args: &PlanArgs) -> Result<()> {
    let plan = args.resolve(None)?;
    let modem = Carrier::new(plan)?;
    let (low_hz, high_hz) = plan.band_hz();

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
