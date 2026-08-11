//! `stego-flac info`

use std::fs;

use anyhow::{bail, Context, Result};
use audio_modem_core::{from_i16, Carrier, Header, HEADER_LEN};

use crate::cli::InfoArgs;
use crate::commands::{human_bytes, human_duration, plan_from_tags};
use crate::flac_io;
use crate::flac_tags::{self, PLAN_TAG, PROFILE_TAG};

pub fn run(args: &InfoArgs) -> Result<()> {
    let raw = fs::read(&args.input).with_context(|| format!("reading {}", args.input.display()))?;

    let recorded = plan_from_tags(&raw, args.plan.is_explicit());
    let plan = args.plan.resolve(recorded)?;
    let modem = Carrier::new(plan).context("building the modem")?;

    let tags = flac_tags::read_tags(&raw).unwrap_or_default();
    let audio = flac_io::read_flac(&args.input)?;

    if audio.sample_rate != plan.sample_rate() {
        bail!(
            "{} is {} Hz but the tone plan expects {} Hz",
            args.input.display(),
            audio.sample_rate,
            plan.sample_rate()
        );
    }

    let duration = audio.samples.len() as f64 / f64::from(audio.sample_rate);

    println!("{}", args.input.display());
    println!();
    println!(
        "  container          {} Hz, {} channel(s), {} samples",
        audio.sample_rate,
        audio.channels,
        audio.samples.len()
    );
    println!("  duration           {}", human_duration(duration));
    let (low_hz, high_hz) = plan.band_hz();
    println!(
        "  profile            {}",
        match tags.get(PROFILE_TAG) {
            Some(name) => name.clone(),
            None if recorded.is_some() => "custom (from metadata)".to_string(),
            None => "assumed default".to_string(),
        }
    );
    println!(
        "  waveform           {} ({:.0} bit/s, {:.0}-{:.0} Hz)",
        plan.describe(),
        plan.bit_rate(),
        low_hz,
        high_hz
    );

    if recorded.is_none() && !tags.contains_key(PLAN_TAG) {
        println!("                     (no plan recorded in metadata)");
    }

    // Only the header is demodulated. At the default plan that is 92 bytes =
    // 184 symbols = 8832 samples, so `info` is effectively instant even for a
    // multi-hour carrier.
    let needed = modem
        .modulated_len(HEADER_LEN)
        .next_multiple_of(modem.alignment_samples())
        * audio.channels;
    if audio.samples.len() < needed {
        bail!(
            "{} holds {} samples, fewer than the {needed} needed for a header",
            args.input.display(),
            audio.samples.len()
        );
    }

    let header_samples = from_i16(&audio.samples[..needed]);
    let header_bytes = modem.demodulate_interleaved(&header_samples, audio.channels)?;
    let header = Header::parse(&header_bytes)?;

    let declared = header.frame_len();
    let carried = ((audio.samples.len() / modem.alignment_samples()) as f64
        * modem.alignment_samples() as f64
        * plan.bit_rate()
        / (8.0 * f64::from(plan.sample_rate()))) as u64;

    println!("  format version     {}", header.version);
    println!(
        "  payload size       {}{}",
        human_bytes(header.original_len),
        if header.is_named() {
            " (includes the stored filename)"
        } else {
            ""
        }
    );
    println!("  compressed         {}", yes_no(header.is_compressed()));
    println!("  encrypted          {}", yes_no(header.is_encrypted()));
    if header.is_encrypted() {
        println!(
            "  argon2id           m={} KiB, t={}, p={}",
            header.kdf.m_cost, header.kdf.t_cost, header.kdf.p_cost
        );
    }
    println!(
        "  filename stored    {}{}",
        yes_no(header.is_named()),
        if header.is_named() && header.is_encrypted() {
            " (encrypted; visible only after decode)"
        } else {
            ""
        }
    );
    println!(
        "  format stored      {}{}",
        yes_no(header.has_format()),
        if header.has_format() && header.is_encrypted() {
            " (encrypted; visible only after decode)"
        } else {
            ""
        }
    );
    println!("  fec                {}", yes_no(header.is_fec()));
    println!("  fec symbol size    {} B", header.fec_symbol_size);
    println!("  frame size         {}", human_bytes(declared));
    println!("  carried bytes      {}", human_bytes(carried));

    if carried < declared {
        println!();
        println!(
            "  WARNING: carrier is short by {} -- {:.2}% of the frame is missing.",
            human_bytes(declared - carried),
            100.0 * (declared - carried) as f64 / declared as f64
        );
        println!("  RaptorQ repair symbols may still recover it; try `decode`.");
    }

    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}
