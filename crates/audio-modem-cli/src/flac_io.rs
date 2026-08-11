//! FLAC container read/write.
//!
//! This lives in the binary rather than in `audio-modem-core` on purpose: the
//! core crate is container-independent, and keeping codec dependencies out of
//! it means the modem can be reused over WAV, over a raw socket, or over a
//! sound card without dragging FLAC along.
//!
//! Encoding uses `flacenc` (pure Rust); decoding uses `symphonia`, which reads
//! FLAC but does not write it. Two crates for one format is not ideal, but it
//! is the state of the pure-Rust ecosystem and it avoids a C dependency.
//!
//! # Why FLAC and not WAV — and what it does *not* buy
//!
//! Both are lossless, so detection is unaffected either way. The intuition that
//! FLAC should crush this signal — it is, at every instant, exactly one sine
//! wave at one of sixteen known frequencies — is wrong, and measurably so.
//! Encoding the same carrier three ways:
//!
//! | symbol stream | FLAC size vs raw PCM |
//! |---|---|
//! | one constant tone | 30.2% (3.31x) |
//! | two tones alternating | 67.8% (1.47x) |
//! | uniformly random symbols | **98.7% (1.01x)** |
//!
//! Linear prediction models a *stationary* sinusoid beautifully, which is why
//! the constant tone compresses. But the predictor is invalidated at every
//! symbol boundary — every 48 samples — and the resulting error burst forces
//! the Rice coder to widen its parameter across the whole partition. Aligning
//! FLAC's block size to the symbol length does not rescue it either: at
//! `block_size = 48` the per-frame LPC coefficient overhead makes the file
//! *larger* than raw PCM.
//!
//! The bottom row is the one that matters, because a real payload is encrypted,
//! and ciphertext is uniformly random by construction. **Encryption guarantees
//! FLAC's worst case here.** There is no tuning that recovers it: FLAC is a
//! general audio codec and cannot know the signal is drawn from a 16-symbol
//! alphabet. A compressor that did know would reach ~4 bits per 48 samples,
//! which is simply the frame itself — i.e. the payload before modulation.
//!
//! So FLAC is chosen for **interoperability, not compression**: the output is a
//! standard audio file that any player, editor, or messaging app will accept
//! and pass through without resampling. Treat the container as free-standing
//! transport, and expect it to be roughly `16 bits/sample ÷ 4 bits/symbol × 48
//! samples/symbol = 192x` the frame size regardless of codec.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use flacenc::component::BitRepr;
use flacenc::error::Verify;
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::default::codecs::FlacDecoder;
use symphonia::default::formats::FlacReader;

use crate::flac_tags;

/// PCM decoded from a FLAC file.
pub struct FlacAudio {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: usize,
}

/// Write mono 16-bit `samples` to `path` as FLAC.
///
/// The sample count is padded up to a whole number of encoder blocks with
/// silence. That is an interoperability fix, not an aesthetic one.
///
/// FLAC's STREAMINFO records the minimum and maximum block length actually
/// present. A stream whose final block is short therefore has `min != max`,
/// even though every frame uses the *fixed* blocking strategy and is numbered
/// by frame rather than by sample. Some readers — symphonia among them —
/// infer the blocking strategy by testing `min == max`, conclude the stream is
/// variable-blocked, and then reject every fixed-strategy frame header they
/// find. The result is a file that macOS `afinfo` and the reference decoder
/// read happily but symphonia rejects with "unexpected end of file", because
/// its resync scans the entire stream without ever accepting a frame.
///
/// Padding to a block multiple forces `min == max` and sidesteps the ambiguity.
/// The cost is at most `block_size - 1` samples of trailing silence (~170 ms at
/// the default plan). The decoder discards them for free: the frame header
/// declares the payload length, so anything past it is never read.
pub fn write_flac(
    path: &Path,
    samples: &[i16],
    sample_rate: u32,
    channels: usize,
    tags: &[(String, String)],
) -> Result<()> {
    let channels = channels.max(1);
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, error)| anyhow!("invalid flac encoder configuration: {error}"))?;

    // flacenc takes i32 regardless of the declared bit depth.
    let mut widened: Vec<i32> = samples.iter().map(|&s| i32::from(s)).collect();
    // A block holds `block_size` frames, and a frame is one sample per channel.
    let block = config.block_size.max(1) * channels;
    let padded_len = widened.len().div_ceil(block) * block;
    widened.resize(padded_len, 0);

    let source =
        flacenc::source::MemSource::from_samples(&widened, channels, 16, sample_rate as usize);

    let mut stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|error| anyhow!("flac encoding failed: {error}"))?;

    // flacenc has no Vorbis comment type, but it will carry an arbitrary block
    // verbatim, and the block type number is all a reader needs to recognise
    // it. So the payload is built by hand and handed over as block type 4.
    if !tags.is_empty() {
        let block = flac_tags::build_block(tags);
        stream.add_metadata_block(
            flacenc::component::MetadataBlockData::new_unknown(
                flac_tags::VORBIS_COMMENT_BLOCK,
                &block,
            )
            .map_err(|error| anyhow!("could not build the metadata block: {error}"))?,
        );
    }

    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|error| anyhow!("flac serialisation failed: {error}"))?;

    fs::write(path, sink.as_slice()).with_context(|| format!("writing {}", path.display()))?;

    Ok(())
}

/// Read a FLAC file into interleaved 16-bit PCM.
pub fn read_flac(path: &Path) -> Result<FlacAudio> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());

    // The format is known, so the reader is constructed directly rather than
    // through the probe: fewer moving parts, and a clearer error when the file
    // simply is not FLAC.
    let mut reader = FlacReader::try_new(stream, FormatOptions::default())
        .with_context(|| format!("{} is not a readable FLAC file", path.display()))?;

    let track = reader
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("{} contains no audio track", path.display()))?;
    let track_id = track.id;

    let audio_params = match track.codec_params.as_ref() {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => bail!("{} has no usable audio codec parameters", path.display()),
    };

    let sample_rate = audio_params
        .sample_rate
        .ok_or_else(|| anyhow!("{} does not declare a sample rate", path.display()))?;
    let channels = audio_params
        .channels
        .as_ref()
        .map(|channels| channels.count())
        .unwrap_or(1);

    let mut decoder = FlacDecoder::try_new(&audio_params, &AudioDecoderOptions::default())
        .map_err(|error| anyhow!("could not start the FLAC decoder: {error}"))?;

    let mut samples: Vec<i16> = Vec::new();
    let mut scratch: Vec<i16> = Vec::new();

    // A truncated carrier is a supported input, not an error: RaptorQ repair
    // symbols exist precisely so a clipped tail stays recoverable. So a read or
    // decode failure part-way through is treated as end-of-stream provided some
    // audio was already recovered, and only reported as fatal if the very first
    // frame fails.
    loop {
        let packet = match reader.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) if !samples.is_empty() => {
                eprintln!(
                    "note: {} ends mid-frame ({error}); decoding what survived",
                    path.display()
                );
                break;
            }
            Err(error) => bail!("reading {}: {error}", path.display()),
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(error) if !samples.is_empty() => {
                eprintln!(
                    "note: {} has a damaged frame ({error}); decoding what survived",
                    path.display()
                );
                break;
            }
            Err(error) => bail!("decoding {}: {error}", path.display()),
        };

        scratch.clear();
        copy_interleaved_i16(&decoded, &mut scratch);
        samples.extend_from_slice(&scratch);
    }

    Ok(FlacAudio {
        samples,
        sample_rate,
        channels,
    })
}

/// Copy any decoded buffer format into interleaved `i16`.
fn copy_interleaved_i16(buffer: &GenericAudioBufferRef<'_>, dst: &mut Vec<i16>) {
    buffer.copy_to_vec_interleaved(dst);
}
