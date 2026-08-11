//! Loading cover audio for the radio-camouflage mode.
//!
//! The cover is whatever audio the user supplies — FLAC, WAV, MP3, or the audio
//! track of an MP4/M4A — reduced to a
//! single mono channel at the carrier's sample rate. It is deliberately *not*
//! made high fidelity: the modulator band-limits it to the plan's lowest
//! subcarrier up to whatever ceiling the plan reserved, so anything outside
//! that is thrown away regardless.
//!
//! The ceiling is a parameter rather than a constant because it moves: a small
//! payload can afford to hand the cover more spectrum than a large one. The
//! filter below has to track it, or a widened band would be fed audio whose top
//! octave had already been discarded.
//!
//! # Why the source is low-passed before resampling
//!
//! Rate conversion without a filter folds everything above the new Nyquist back
//! down into the audible range, and those aliases would land squarely in the
//! band the cover occupies. Low-passing near the cover's own ceiling first
//! makes the decimation harmless: there is nothing left up there to fold.
//!
//! The filter is a cascaded one-pole, which is gentle — but it only has to stop
//! aliasing being audible, since the modulator's bin copy enforces the exact
//! band edges afterwards.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Decode `path` to mono `f32` at `target_rate`, low-passed at `ceiling_hz`.
pub fn load(path: &Path, target_rate: u32, ceiling_hz: f32) -> Result<Vec<f32>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening cover audio {}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }

    // The probe is used here, unlike the carrier reader, because the cover can
    // be any format the user happens to have.
    let mut reader = symphonia::default::get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .with_context(|| {
            format!(
                "{} is not audio this build can read (FLAC, WAV, MP3 and MP4/AAC \
                 are supported)",
                path.display()
            )
        })?;

    let track = reader
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("{} has no audio track", path.display()))?
        .clone();
    let params = match track.codec_params.as_ref() {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => bail!("{} has no usable audio parameters", path.display()),
    };

    let source_rate = params
        .sample_rate
        .ok_or_else(|| anyhow!("{} does not declare a sample rate", path.display()))?;
    let channels = params
        .channels
        .as_ref()
        .map(|channels| channels.count())
        .unwrap_or(1)
        .max(1);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|error| anyhow!("no decoder for {}: {error}", path.display()))?;

    let mut interleaved: Vec<f32> = Vec::new();
    let mut scratch: Vec<f32> = Vec::new();
    loop {
        match reader.next_packet() {
            Ok(Some(packet)) => {
                if packet.track_id != track.id {
                    continue;
                }
                match decoder.decode(&packet) {
                    Ok(decoded) => {
                        scratch.clear();
                        copy_f32(&decoded, &mut scratch);
                        interleaved.extend_from_slice(&scratch);
                    }
                    // A damaged tail is not worth failing over; the cover is
                    // decoration, and whatever decoded is enough to loop.
                    Err(_) if !interleaved.is_empty() => break,
                    Err(error) => bail!("decoding {}: {error}", path.display()),
                }
            }
            Ok(None) => break,
            Err(_) if !interleaved.is_empty() => break,
            Err(error) => bail!("reading {}: {error}", path.display()),
        }
    }

    if interleaved.is_empty() {
        bail!("{} decoded to no audio", path.display());
    }

    let mono = downmix(&interleaved, channels);
    Ok(resample(&mono, source_rate, target_rate, ceiling_hz))
}

/// Average the channels together.
fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Low-pass near the cover ceiling, then linearly resample.
fn resample(mono: &[f32], from: u32, to: u32, ceiling_hz: f32) -> Vec<f32> {
    let filtered = low_pass(mono, from as f32, ceiling_hz);
    if from == to || filtered.is_empty() {
        return filtered;
    }

    let ratio = from as f64 / to as f64;
    let out_len = (filtered.len() as f64 / ratio) as usize;
    (0..out_len)
        .map(|i| {
            let position = i as f64 * ratio;
            let index = position as usize;
            let fraction = (position - index as f64) as f32;
            let a = filtered[index.min(filtered.len() - 1)];
            let b = filtered[(index + 1).min(filtered.len() - 1)];
            a + (b - a) * fraction
        })
        .collect()
}

/// Two cascaded one-pole low-pass sections.
fn low_pass(samples: &[f32], sample_rate: f32, cutoff_hz: f32) -> Vec<f32> {
    if sample_rate <= 0.0 || cutoff_hz >= sample_rate / 2.0 {
        return samples.to_vec();
    }
    let dt = 1.0 / sample_rate;
    let rc = 1.0 / (std::f32::consts::TAU * cutoff_hz);
    let alpha = dt / (rc + dt);

    let mut out = samples.to_vec();
    for _ in 0..2 {
        let mut state = 0.0f32;
        for value in out.iter_mut() {
            state += alpha * (*value - state);
            *value = state;
        }
    }
    out
}

fn copy_f32(buffer: &GenericAudioBufferRef<'_>, dst: &mut Vec<f32>) {
    buffer.copy_to_vec_interleaved(dst);
}
