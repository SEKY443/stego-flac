//! Command-line surface.

use std::path::PathBuf;

use anyhow::{bail, Result};
use audio_modem_core::codec::compress;
use audio_modem_core::codec::fec::{DEFAULT_REPAIR_PERCENT, DEFAULT_SYMBOL_SIZE};
use audio_modem_core::{FecParams, Plan, Profile};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "stego-flac",
    version,
    about = "Encapsulate files in acoustic waveforms stored as FLAC.",
    long_about = "Encodes a file into a modulated audio carrier and back.\n\n\
                  The payload is compressed (zstd), encrypted (AES-256-GCM with an\n\
                  Argon2id-derived key), erasure-coded (RaptorQ), then modulated as\n\
                  bin-aligned M-FSK into a mono 16-bit FLAC container.\n\n\
                  The tone plan is written into the carrier's FLAC metadata, so\n\
                  `decode` and `info` configure themselves automatically and any\n\
                  profile is safe to use. Tone-plan flags are only needed for a\n\
                  carrier whose metadata has been stripped."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Encode a file into a FLAC carrier.
    Encode(EncodeArgs),
    /// Recover a file from a FLAC carrier.
    Decode(DecodeArgs),
    /// Print the header of a carrier without decoding the payload.
    Info(InfoArgs),
    /// Print the tone plan and throughput for a given configuration.
    Plan(PlanArgs),
    /// Emit a shell completion script.
    Completions(CompletionArgs),
}

#[derive(Debug, Args)]
pub struct CompletionArgs {
    /// Shell to generate a script for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

/// Selectable waveform preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProfileArg {
    /// OFDM, 243 subcarriers of 4096-QAM. 128 kbit/s. The default.
    Dense,
    /// OFDM, 65536-QAM. 170 kbit/s; ~25% smaller than dense, less margin.
    Compact,
    /// M-FSK, 16 tones, 2000-9500 Hz. 2000 bit/s. Readable spectrogram.
    Standard,
    /// M-FSK, 4 tones, 4000-10000 Hz. 4000 bit/s.
    Fast,
}

impl From<ProfileArg> for Profile {
    fn from(value: ProfileArg) -> Self {
        match value {
            ProfileArg::Dense => Profile::Dense,
            ProfileArg::Compact => Profile::Compact,
            ProfileArg::Standard => Profile::Standard,
            ProfileArg::Fast => Profile::Fast,
        }
    }
}

/// What to do when the cover audio is longer than the payload needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CoverMode {
    /// End the carrier with the payload, cutting the cover short.
    Cut,
    /// Stretch the payload over the whole cover so it plays to the end.
    Spread,
}

/// How much spectrum the audible cover gets.
///
/// Bandwidth is taken from the data, so this trades file size for how the cover
/// sounds. `Auto` makes that trade on the payload's behalf: see
/// [`audio_modem_core::modem::ofdm::CoverPlan::auto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CoverQuality {
    /// Widen the band for small payloads, narrow it for large ones.
    Auto,
    /// Ceiling near 3.4 kHz. Sounds like a phone call; cheapest.
    Telephone,
    /// Ceiling near 5 kHz. Speech loses its muffled edge.
    Wide,
    /// Ceiling near 7 kHz. Music becomes listenable; roughly doubles the file.
    Full,
}

impl CoverQuality {
    /// The fixed ceiling this setting asks for, if it is not size-driven.
    pub fn ceiling_hz(self) -> Option<f64> {
        use audio_modem_core::modem::ofdm::{COVER_FULL_HZ, COVER_TELEPHONE_HZ, COVER_WIDE_HZ};
        match self {
            CoverQuality::Auto => None,
            CoverQuality::Telephone => Some(COVER_TELEPHONE_HZ),
            CoverQuality::Wide => Some(COVER_WIDE_HZ),
            CoverQuality::Full => Some(COVER_FULL_HZ),
        }
    }
}

/// How many audio channels to spread the payload over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelChoice {
    /// Decide from the frame size.
    Auto,
    /// Use exactly this many.
    Fixed(usize),
}

impl std::str::FromStr for ChannelChoice {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        if text.eq_ignore_ascii_case("auto") {
            return Ok(ChannelChoice::Auto);
        }
        match text.parse::<usize>() {
            Ok(n) if (1..=8).contains(&n) => Ok(ChannelChoice::Fixed(n)),
            _ => Err(format!("expected 1-8 or `auto`, got {text:?}")),
        }
    }
}

#[derive(Debug, Args)]
pub struct EncodeArgs {
    /// File to encode, or `-` to read standard input.
    pub input: PathBuf,

    /// Output FLAC path. Defaults to the input path with `.flac` appended.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Write the payload without encryption.
    #[arg(long)]
    pub no_encrypt: bool,

    /// Read the passphrase from this file (trailing newline stripped).
    #[arg(long, value_name = "PATH", conflicts_with = "no_encrypt")]
    pub passphrase_file: Option<PathBuf>,

    /// Do not store the original filename inside the encrypted payload.
    #[arg(long)]
    pub no_store_name: bool,

    /// Name to store instead of the input's filename. Required to label a
    /// payload read from standard input.
    #[arg(long, value_name = "NAME", conflicts_with = "no_store_name")]
    pub name: Option<String>,

    /// Suppress the summary. Errors still go to standard error.
    #[arg(short, long)]
    pub quiet: bool,

    /// Zstandard compression level.
    #[arg(long, default_value_t = compress::DEFAULT_LEVEL as i64, value_parser = 1..=22)]
    pub level: i64,

    /// RaptorQ repair symbols as a percentage of source symbols. 0 disables
    /// repair; the carrier then cannot survive truncation.
    #[arg(long, default_value_t = DEFAULT_REPAIR_PERCENT, value_name = "PERCENT")]
    pub fec_overhead: u8,

    /// RaptorQ symbol size in bytes.
    #[arg(long, default_value_t = DEFAULT_SYMBOL_SIZE, value_name = "BYTES")]
    pub fec_symbol_size: u16,

    /// Hide the data under audible cover audio (FLAC, WAV, MP3, MP4/M4A).
    ///
    /// The cover takes the low, ear-sensitive end of the spectrum — from the
    /// plan's lowest subcarrier up to a ceiling set by --cover-quality — and the
    /// data moves above it, where broadband noise reads as hiss. The result has
    /// the spectral signature of a weak AM station. Costs at least 27% of
    /// throughput.
    #[arg(long, value_name = "AUDIO")]
    pub cover: Option<PathBuf>,

    /// How much spectrum the cover audio gets.
    ///
    /// Bandwidth comes out of the data's share, so a better-sounding cover
    /// means a longer, larger carrier. `auto` widens the band for small
    /// payloads, where the extra bytes are cheap and the expansion ratio is
    /// already dominated by fixed overhead, and narrows it for large ones.
    #[arg(long, value_enum, default_value_t = CoverQuality::Auto, requires = "cover")]
    pub cover_quality: CoverQuality,

    /// What to do when the cover audio outlasts the data.
    ///
    /// `cut` ends the carrier as soon as the payload does, so a long recording
    /// is truncated mid-phrase. `spread` stretches the payload across the whole
    /// cover by leaving gaps between data symbols, so the recording plays to
    /// its end — at the cost of a proportionally longer, larger file.
    #[arg(long, value_enum, default_value_t = CoverMode::Cut, requires = "cover")]
    pub cover_mode: CoverMode,

    /// How far below the cover to place the data, in decibels.
    ///
    /// Verified exact to 40 dB at the default constellation; 25 leaves margin
    /// while keeping the data well under the audible signal.
    #[arg(long, default_value_t = 25.0, value_name = "DB", requires = "cover")]
    pub cover_attenuation: f32,

    /// Audio channels to interleave the payload across: 1-8, or `auto`.
    ///
    /// Divides the carrier's *duration* by this factor and leaves its size
    /// alone: a channel multiplies capacity per second and bytes per second
    /// equally. `auto` spends channels only while the padding they cost stays
    /// under one percent of the frame, which keeps small payloads on one
    /// channel — where measurement says they belong.
    #[arg(long, default_value = "1", value_name = "N|auto")]
    pub channels: ChannelChoice,

    /// Overwrite the output if it already exists.
    #[arg(short, long)]
    pub force: bool,

    #[command(flatten)]
    pub plan: PlanArgs,
}

#[derive(Debug, Args)]
pub struct DecodeArgs {
    /// FLAC carrier to decode.
    pub input: PathBuf,

    /// Output path, or `-` for standard output. Defaults to the filename
    /// stored in the carrier, written to the current directory.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Suppress the summary. Errors still go to standard error.
    #[arg(short, long)]
    pub quiet: bool,

    /// Read the passphrase from this file (trailing newline stripped).
    #[arg(long, value_name = "PATH")]
    pub passphrase_file: Option<PathBuf>,

    /// Overwrite the output if it already exists.
    #[arg(short, long)]
    pub force: bool,

    #[command(flatten)]
    pub plan: PlanArgs,
}

#[derive(Debug, Args)]
pub struct InfoArgs {
    /// FLAC carrier to inspect.
    pub input: PathBuf,

    #[command(flatten)]
    pub plan: PlanArgs,
}

/// Tone-plan parameters, shared by every subcommand.
///
/// Every field is optional so the resolver can distinguish "the user asked for
/// this" from "nobody said". Precedence is: an explicit flag, then the profile,
/// then whatever the carrier's metadata recorded, then the built-in default.
#[derive(Debug, Clone, Args)]
pub struct PlanArgs {
    /// Waveform preset. [default: dense]
    #[arg(long, value_enum)]
    pub profile: Option<ProfileArg>,

    /// Container sample rate in Hz. [default: 24000]
    #[arg(long, value_name = "HZ")]
    pub sample_rate: Option<u32>,

    /// Peak amplitude in normalised full scale. Detection is gain-invariant,
    /// so this only affects loudness and file size.
    #[arg(long, value_name = "GAIN")]
    pub amplitude: Option<f32>,

    /// FSK only: symbol length in samples.
    #[arg(long, value_name = "N", help_heading = "FSK plan")]
    pub samples_per_symbol: Option<usize>,

    /// FSK only: bits per symbol; tone count is 2^this. Must divide 8.
    #[arg(long, value_name = "BITS", help_heading = "FSK plan")]
    pub bits_per_symbol: Option<u32>,

    /// FSK only: bin stride between adjacent tones.
    #[arg(long, value_name = "BINS", help_heading = "FSK plan")]
    pub bin_spacing: Option<usize>,

    /// OFDM only: FFT size in samples per symbol.
    #[arg(long, value_name = "N", help_heading = "OFDM plan")]
    pub fft_size: Option<usize>,

    /// OFDM only: bits per subcarrier. Even, 2..=20. Higher is smaller but
    /// leaves less margin; 16 gives a 25% smaller carrier than the default 12.
    #[arg(long, value_name = "BITS", help_heading = "OFDM plan")]
    pub qam_bits: Option<u32>,

    /// OFDM only: highest data-bearing bin.
    #[arg(long, value_name = "BIN", help_heading = "OFDM plan")]
    pub top_bin: Option<usize>,

    /// Lowest data-bearing bin. Applies to both waveforms.
    #[arg(long, value_name = "BIN")]
    pub base_bin: Option<usize>,
}

impl PlanArgs {
    /// Whether the user named any plan option.
    pub fn is_explicit(&self) -> bool {
        self.profile.is_some()
            || self.sample_rate.is_some()
            || self.amplitude.is_some()
            || self.base_bin.is_some()
            || self.fsk_flags_used()
            || self.ofdm_flags_used()
    }

    fn fsk_flags_used(&self) -> bool {
        self.samples_per_symbol.is_some()
            || self.bits_per_symbol.is_some()
            || self.bin_spacing.is_some()
    }

    fn ofdm_flags_used(&self) -> bool {
        self.fft_size.is_some() || self.qam_bits.is_some() || self.top_bin.is_some()
    }

    /// Resolve a validated plan.
    ///
    /// `recorded` is the plan read from a carrier's metadata, if any. It is the
    /// base when the user did not select a profile, so individual flags still
    /// override what the file says.
    pub fn resolve(&self, recorded: Option<Plan>) -> Result<Plan> {
        let mut plan = match (self.profile, recorded) {
            (Some(profile), _) => Profile::from(profile).plan(),
            (None, Some(recorded)) => recorded,
            (None, None) => Profile::Dense.plan(),
        };

        // Reject flags that do not apply to the selected waveform rather than
        // silently ignoring them; a user who passes --qam-bits to an FSK plan
        // has a wrong mental model and should hear about it.
        match &mut plan {
            Plan::Fsk(config) => {
                if self.ofdm_flags_used() {
                    bail!(
                        "--fft-size, --qam-bits and --top-bin apply to OFDM plans; \
                         this plan is {}. Pass --profile dense to select OFDM.",
                        Plan::Fsk(*config).describe()
                    );
                }
                if let Some(value) = self.samples_per_symbol {
                    config.samples_per_symbol = value;
                }
                if let Some(value) = self.bits_per_symbol {
                    config.bits_per_symbol = value;
                }
                if let Some(value) = self.bin_spacing {
                    config.bin_spacing = value;
                }
                if let Some(value) = self.base_bin {
                    config.base_bin = value;
                }
            }
            Plan::Ofdm(config) => {
                if self.fsk_flags_used() {
                    bail!(
                        "--samples-per-symbol, --bits-per-symbol and --bin-spacing apply to \
                         FSK plans; this plan is {}. Pass --profile standard to select FSK.",
                        Plan::Ofdm(*config).describe()
                    );
                }
                if let Some(value) = self.fft_size {
                    config.fft_size = value;
                }
                if let Some(value) = self.qam_bits {
                    config.bits_per_bin = value;
                }
                if let Some(value) = self.top_bin {
                    config.top_bin = value;
                }
                if let Some(value) = self.base_bin {
                    config.base_bin = value;
                }
            }
        }

        if let Some(value) = self.sample_rate {
            plan.set_sample_rate(value);
        }
        if let Some(value) = self.amplitude {
            plan.set_amplitude(value);
        }

        if let Err(error) = plan.validate() {
            bail!("invalid plan: {error}");
        }

        Ok(plan)
    }
}

/// Whether a path means "use the standard stream" rather than a file.
pub fn is_stream(path: &std::path::Path) -> bool {
    path.as_os_str() == "-"
}

impl EncodeArgs {
    /// Whether the payload comes from standard input.
    pub fn reads_stdin(&self) -> bool {
        is_stream(&self.input)
    }

    /// Resolve the FLAC output path.
    ///
    /// A carrier cannot be streamed to standard output: FLAC's STREAMINFO
    /// records the total sample count and an MD5 of the audio, neither of which
    /// is known until the last sample is generated, so the header has to be
    /// rewritten at the end. That needs a seekable file.
    pub fn output_path(&self) -> Result<PathBuf> {
        if let Some(path) = &self.output {
            if is_stream(path) {
                bail!(
                    "a FLAC carrier cannot be written to standard output; its header \
                     records a length and checksum that are only known once encoding \
                     finishes. Give -o a real path."
                );
            }
            return Ok(path.clone());
        }

        if self.reads_stdin() {
            bail!("reading from standard input, so -o is required to name the carrier");
        }

        let mut path = self.input.clone().into_os_string();
        path.push(".flac");
        Ok(PathBuf::from(path))
    }

    /// The filename to store inside the encrypted payload, if any.
    pub fn stored_name(&self) -> Option<String> {
        if self.no_store_name {
            return None;
        }
        if let Some(name) = &self.name {
            return Some(name.clone());
        }
        if self.reads_stdin() {
            return None;
        }
        self.input
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
    }

    /// Build FEC parameters.
    pub fn fec_params(&self) -> FecParams {
        FecParams {
            symbol_size: self.fec_symbol_size,
            repair_overhead_percent: self.fec_overhead,
        }
    }
}
