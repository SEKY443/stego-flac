//! `stego-flac encode`

use std::fs;
use std::io::Read;

use anyhow::bail;
use anyhow::{Context, Result};
use audio_modem_core::modem::ofdm::COVER_TELEPHONE_HZ;
use audio_modem_core::{encode_frame, format, to_i16, Carrier, EncodeParams, KdfParams, Plan};

use crate::cli::{ChannelChoice, CoverMode, EncodeArgs};
use crate::commands::{guard_output, human_bytes, human_duration, Stage};
use crate::flac_tags::{PLAN_TAG, PROFILE_TAG};
use crate::{cover, flac_io, passphrase};

pub fn run(args: &EncodeArgs) -> Result<()> {
    let mut plan = args.plan.resolve(None)?;

    // Check the waveform can carry a cover at all before anything expensive
    // happens — a passphrase prompt and a multi-minute encode are both worse
    // places to discover an FSK plan has no spectrum to partition. The band
    // itself is chosen later, once the frame size is known.
    if args.cover.is_some() && plan.set_cover_ceiling(COVER_TELEPHONE_HZ, 0.0).is_none() {
        bail!(
            "--cover needs a waveform with a spectrum to partition; {} lights one \
             tone at a time. Use an OFDM profile (dense or compact).",
            plan.describe()
        );
    }
    let output = args.output_path()?;
    guard_output(&output, args.force)?;

    let plaintext = if args.reads_stdin() {
        let mut buffer = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buffer)
            .context("reading standard input")?;
        buffer
    } else {
        fs::read(&args.input).with_context(|| format!("reading {}", args.input.display()))?
    };

    // Warn before the passphrase prompt, not after: this is the last moment the
    // user can cancel without having typed a secret, and a multi-hour encode is
    // worth interrupting early.
    let estimated = plaintext.len() as f64 * 8.0 / plan.bit_rate();
    if estimated > 600.0 {
        eprintln!(
            "note: {} uncompressed would be ~{} of audio at {:.0} bit/s.",
            human_bytes(plaintext.len() as u64),
            human_duration(estimated),
            plan.bit_rate()
        );
        eprintln!("      compression usually cuts this a lot; `--profile fast` halves it.");
    }

    // Confirm on encode: a mistyped write-side passphrase produces a carrier
    // that nobody, including its author, can ever open.
    let secret = if args.no_encrypt {
        None
    } else {
        Some(passphrase::acquire(args.passphrase_file.as_deref(), true)?)
    };

    let params = EncodeParams {
        compression_level: args.level as i32,
        passphrase: secret.as_ref().map(|s| s.as_slice()),
        kdf: KdfParams::default(),
        fec: args.fec_params(),
        store_format: !args.no_store_name,
        store_timestamp: !args.no_store_name,
    };

    // A payload piped in has no filename of its own, so one is synthesised from
    // whatever the content turns out to be: `payload.pdf` rather than nothing.
    let detected = format::detect(&plaintext);
    let stored_name = args.stored_name().or_else(|| {
        (!args.no_store_name && args.reads_stdin()).then(|| format::default_name(detected))
    });

    let stage = Stage::new();
    stage.begin("compressing and encrypting");
    let (frame, report) = encode_frame(&plaintext, stored_name.as_deref(), &params)?;

    // The band is settled here rather than up front because `auto` keys off the
    // frame — post-compression, post-encryption, post-FEC — which is what
    // actually becomes audio. A 40 MB text file that zstd takes down to 6 MB
    // should get the wide band its carrier can afford, not the one its
    // uncompressed size suggests.
    let cover_audio = match &args.cover {
        Some(path) => {
            let ceiling = match args.cover_quality.ceiling_hz() {
                Some(fixed) => plan.set_cover_ceiling(fixed, f64::from(args.cover_attenuation)),
                None => plan.set_auto_cover(frame.len(), f64::from(args.cover_attenuation)),
            }
            .expect("cover support was checked before the frame was built");
            plan.validate()
                .map_err(|error| anyhow::anyhow!("cover band does not fit this plan: {error}"))?;

            stage.begin("loading cover audio");
            // The loader's anti-alias filter has to track the band, otherwise a
            // wider band would be handed audio that was already thrown away and
            // the extra subcarriers would carry nothing but silence.
            Some(cover::load(path, plan.sample_rate(), ceiling as f32)?)
        }
        None => None,
    };

    // Deciding the stride needs both lengths, so it happens after the frame
    // exists and the cover is loaded — and before the modem is built, because
    // the stride is part of the plan the metadata records.
    if let (CoverMode::Spread, Some(audio)) = (args.cover_mode, &cover_audio) {
        let symbols = plan.symbols_for(frame.len());
        let cover_symbols = audio.len() / 512;
        if symbols > 0 && cover_symbols > symbols {
            plan.set_spread(cover_symbols / symbols);
        }
    }
    let plan = plan;
    let modem = Carrier::new(plan).context("building the modem")?;

    stage.begin("modulating");
    // Cover mode is single-channel. The cover is meant to be heard as an
    // ordinary recording, and the lane splitting used for extra channels
    // produces one independent carrier per channel -- there is no sensible
    // single audible signal to spread across them.
    let channels = match (args.channels, cover_audio.is_some()) {
        (_, true) => 1,
        (ChannelChoice::Fixed(n), false) => n,
        (ChannelChoice::Auto, false) => plan.auto_channels(frame.len()),
    };
    if cover_audio.is_some() {
        if let ChannelChoice::Fixed(requested) = args.channels {
            if requested > 1 {
                bail!(
                    "--cover produces a single audible carrier, so it cannot also spread \
                     the payload across {requested} channels. Drop --channels, or drop \
                     --cover."
                );
            }
        }
    }
    let samples = match &cover_audio {
        Some(audio) => modem
            .modulate_with_cover(&frame, audio, args.cover_mode == CoverMode::Spread)
            .expect("cover support was checked when the plan was resolved"),
        None => modem.modulate_interleaved(&frame, channels),
    };
    let pcm = to_i16(&samples);

    stage.begin("encoding FLAC");
    flac_io::write_flac(
        &output,
        &pcm,
        plan.sample_rate(),
        channels,
        &tags(plan, &report_title(&report)),
    )?;
    stage.done();

    let carrier_bytes = fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    let duration = (samples.len() / channels) as f64 / f64::from(plan.sample_rate());

    if args.quiet {
        if !report.encrypted {
            eprintln!("warning: payload is not encrypted; anyone with this file can read it");
        }
        return Ok(());
    }

    println!("wrote {}", output.display());
    println!();
    println!(
        "  input              {}",
        human_bytes(report.plaintext_len as u64)
    );
    if report.compressed {
        println!(
            "  compressed         {} ({:.1}% of original)",
            human_bytes(report.compressed_len as u64),
            report.compression_ratio() * 100.0
        );
    } else {
        println!("  compressed         skipped (input is incompressible)");
    }
    println!(
        "  encrypted          {}",
        if report.encrypted {
            "AES-256-GCM, Argon2id key"
        } else {
            "no (--no-encrypt)"
        }
    );
    println!(
        "  filename stored    {}",
        match &stored_name {
            Some(name) => name.as_str(),
            None => "no",
        }
    );
    println!(
        "  format detected    {}",
        match (detected, args.no_store_name) {
            (_, true) => "not stored".to_string(),
            (Some(format), _) => format.description.to_string(),
            (None, _) => "unrecognised".to_string(),
        }
    );
    println!(
        "  fec                {} RaptorQ packets ({}% repair)",
        report.fec_packets, args.fec_overhead
    );
    println!(
        "  frame              {} ({:.2}x plaintext)",
        human_bytes(report.frame_len as u64),
        report.expansion_ratio()
    );
    let (low_hz, high_hz) = plan.band_hz();
    println!(
        "  waveform           {} ({:.0} bit/s, {:.0}-{:.0} Hz)",
        plan.describe(),
        plan.bit_rate(),
        low_hz,
        high_hz
    );
    if let Some((low, high)) = plan.cover_band_hz() {
        println!(
            "  cover audio        {:.0}-{:.0} Hz, data {:.0} dB below",
            low, high, args.cover_attenuation
        );
    }
    println!(
        "  carrier            {}{}",
        human_duration(duration),
        if channels > 1 {
            format!(
                " across {channels} channels{}",
                if args.channels == ChannelChoice::Auto {
                    " (auto)"
                } else {
                    ""
                }
            )
        } else {
            String::new()
        }
    );
    println!(
        "  flac file          {} ({:.2}x plaintext)",
        human_bytes(carrier_bytes),
        carrier_bytes as f64 / report.plaintext_len.max(1) as f64
    );

    if !report.encrypted {
        eprintln!();
        eprintln!("warning: payload is not encrypted; anyone with this file can read it");
    }

    Ok(())
}

/// Human-readable one-liner for the DESCRIPTION tag.
fn report_title(report: &audio_modem_core::EncodeReport) -> String {
    format!(
        "stego-flac carrier, {} payload, {}",
        human_bytes(report.plaintext_len as u64),
        if report.encrypted {
            "encrypted"
        } else {
            "not encrypted"
        }
    )
}

/// Metadata written into the carrier.
///
/// `AUDIOMODEM_PLAN` is the load-bearing one: it is what lets `decode` and
/// `info` configure themselves. The rest exist so the file looks like a real
/// tagged audio file in a player or library scanner instead of an untitled blob.
fn tags(plan: Plan, description: &str) -> Vec<(String, String)> {
    let mut tags = vec![
        ("TITLE".to_string(), "stego-flac carrier".to_string()),
        ("DESCRIPTION".to_string(), description.to_string()),
        (
            "ENCODER".to_string(),
            format!("stego-flac {}", env!("CARGO_PKG_VERSION")),
        ),
        (PLAN_TAG.to_string(), plan.to_plan_string()),
    ];

    // Record the preset name when the plan matches one exactly, purely so
    // `info` can say "fast" rather than reciting five numbers.
    for profile in audio_modem_core::Profile::ALL {
        if profile.plan() == plan {
            tags.push((PROFILE_TAG.to_string(), profile.name().to_string()));
            break;
        }
    }

    tags
}
