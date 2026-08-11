//! `stego-flac decode`

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use audio_modem_core::{decode_frame, from_i16, Carrier, Header};

use crate::cli::{is_stream, DecodeArgs};
use crate::commands::{guard_output, human_bytes, plan_from_tags, Stage};
use crate::{flac_io, passphrase};

pub fn run(args: &DecodeArgs) -> Result<()> {
    let raw = fs::read(&args.input).with_context(|| format!("reading {}", args.input.display()))?;

    // The tone plan comes from the carrier itself unless the user overrode it.
    let recorded = plan_from_tags(&raw, args.plan.is_explicit());
    let plan = args.plan.resolve(recorded)?;
    let modem = Carrier::new(plan).context("building the modem")?;

    let stage = Stage::new();
    stage.begin("decoding FLAC");
    let audio = flac_io::read_flac(&args.input)?;
    let samples = prepare_samples(&audio, &modem, &args.input)?;

    stage.begin("demodulating");
    // The channel count comes from the container's own header, so nothing has
    // to be recorded in the metadata or remembered by the reader.
    let frame = modem.demodulate_interleaved(&samples, audio.channels)?;
    stage.done();

    // Read the header before asking for a passphrase, so a tone-plan mismatch
    // fails with a clear message rather than after the user has typed a secret.
    let header = Header::parse(&frame).with_context(|| {
        if recorded.is_some() {
            "the carrier's recorded tone plan did not demodulate to a valid frame"
        } else {
            "no tone plan found in the carrier's metadata; if it was encoded with \
             non-default settings, pass the same flags used to encode it"
        }
    })?;

    let secret = if header.is_encrypted() {
        Some(passphrase::acquire(args.passphrase_file.as_deref(), false)?)
    } else {
        if args.passphrase_file.is_some() {
            bail!("this carrier is not encrypted, but --passphrase-file was given");
        }
        None
    };

    let stage = Stage::new();
    stage.begin("decrypting and decompressing");
    let payload = decode_frame(&frame, secret.as_ref().map(|s| s.as_slice()))?;
    stage.done();

    // Writing the payload to standard output makes stdout a data channel, so
    // the summary moves to stderr rather than corrupting whatever is piped.
    let to_stdout = args.output.as_deref().is_some_and(is_stream);
    if to_stdout {
        std::io::stdout()
            .write_all(&payload.data)
            .context("writing standard output")?;
        if !args.quiet {
            eprintln!("recovered {}", human_bytes(payload.data.len() as u64));
        }
        return Ok(());
    }

    let output = resolve_output(args, payload.suggested_name())?;
    guard_output(&output, args.force)?;

    fs::write(&output, &payload.data).with_context(|| format!("writing {}", output.display()))?;

    if !args.quiet {
        println!(
            "recovered {} to {}{}",
            human_bytes(payload.data.len() as u64),
            output.display(),
            match payload.format {
                Some(format) => format!(" ({})", format.description),
                None => String::new(),
            }
        );
    }

    Ok(())
}

/// Decide where to write the recovered file.
///
/// `-o` wins. Otherwise the name stored inside the encrypted payload is used
/// verbatim, with any directory component stripped. A carrier is untrusted
/// input, and a stored name like `../../.ssh/authorized_keys` must not be able
/// to steer the write anywhere outside the working directory.
fn resolve_output(args: &DecodeArgs, stored: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = &args.output {
        return Ok(path.clone());
    }

    let Some(stored) = stored else {
        bail!("this carrier does not store a filename; pass -o to say where to write the payload");
    };

    let safe = Path::new(stored)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..");

    let Some(safe) = safe else {
        bail!("the carrier stores an unusable filename ({stored:?}); pass -o instead");
    };

    if safe != stored {
        eprintln!("note: stored name {stored:?} was reduced to {safe:?}");
    }

    Ok(PathBuf::from(safe))
}

/// Validate the container against the tone plan and trim to a decodable length.
fn prepare_samples(audio: &flac_io::FlacAudio, modem: &Carrier, path: &Path) -> Result<Vec<f32>> {
    let plan = modem.plan();
    if audio.channels == 0 || audio.channels > 8 {
        bail!(
            "{} declares {} channels; stego-flac carriers use 1 to 8",
            path.display(),
            audio.channels
        );
    }

    if audio.sample_rate != plan.sample_rate() {
        bail!(
            "{} is {} Hz but the tone plan expects {} Hz; pass --sample-rate {} \
             (every tone frequency is defined relative to the sample rate, so a \
             mismatch shifts the whole plan)",
            path.display(),
            audio.sample_rate,
            plan.sample_rate(),
            audio.sample_rate
        );
    }

    // Trim to a whole number of bytes' worth of symbols. A FLAC encoder is free
    // to pad its final block, and a truncated carrier is exactly the case FEC
    // exists to survive, so a ragged tail is discarded rather than rejected.
    // Interleaved samples must divide evenly into whole frames *and* leave each
    // lane a whole number of symbols.
    let group = modem.alignment_samples() * audio.channels;
    let usable = audio.samples.len() - audio.samples.len() % group;

    if usable == 0 {
        bail!(
            "{} holds {} samples, fewer than one symbol group",
            path.display(),
            audio.samples.len()
        );
    }

    // The remainder is discarded silently. It is always benign: the encoder
    // pads the carrier to a whole number of FLAC blocks, and the frame header
    // declares the payload length, so trailing samples are never part of the
    // message. Warning about them would fire on almost every successful decode.
    Ok(from_i16(&audio.samples[..usable]))
}
