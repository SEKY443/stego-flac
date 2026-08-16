# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning is
[SemVer](https://semver.org/) in spirit, though this is a pre-1.0 personal
project and breaking changes can still happen on a minor bump.

## [0.2.0] - 2026-08-16

### Added

- `--json` on every subcommand that prints a report (`encode`, `decode`,
  `info`, `plan`): emits one JSON object on stdout instead of the
  human-readable text, for a caller that parses the result programmatically
  rather than a person reading a terminal.
- In `--json` mode, the stage progress that used to only appear on an
  interactive terminal (`compressing and encrypting`, `modulating`, …) is now
  written to stderr as one `{"stage": "..."}` line per stage regardless of
  whether stderr is a TTY — the case a GUI wrapper or script actually runs in.
  `decode -o -` keeps the payload on stdout and moves the JSON report to
  stderr, same as it already did for the text summary.
- `CHANGELOG.md` (this file).

### Fixed

- `cover.rs`: a cover-audio file that declared a `0` sample rate could drive
  the resampler's length calculation to infinity, which a saturating cast
  turned into a `usize::MAX`-element allocation attempt. Now guarded.
- A broken `rustdoc` intra-doc link (`CoverPlan::auto`) and an inaccurate doc
  comment claiming `fec_symbol_size` isn't known until after encryption — it
  is, it's just excluded from the AAD for a different reason (see the updated
  comment on `Header::aad`).
- Stale placeholder `repository` URL in the workspace `Cargo.toml`.

### Changed

- `EncodeReport.compressed_len` is now taken directly from the compressed
  buffer's length instead of being back-derived from
  `sealed.len() - TAG_LEN`, which depended on the encryption flag to know
  whether to subtract the AEAD tag. Same value, simpler derivation.
- Minor clippy-pedantic cleanups with no behavioural change: `map().unwrap_or()`
  → `map_or()`, a couple of redundant closures, `iter_mut().for_each(...)` →
  `slice::fill(...)`, one doc-backtick fix.

### Internal

- New dependency: `serde_json` (CLI crate only), for the `--json` output.

## [0.1.0] - Initial release

The tool as it stood before this changelog existed: a working, fully
round-tripping pipeline in two crates (`audio-modem-core`,
container-independent; `audio-modem-cli`, the `stego-flac` binary).

### Pipeline

- `zstd` (level 19 by default, with a cheap probe pass that skips compression
  on already-incompressible input) → AES-256-GCM with an Argon2id-derived key
  → RaptorQ forward error correction → an audio modem → a FLAC container.
- A 92-byte authenticated frame header (magic, version, flags, lengths,
  Argon2id/RaptorQ parameters, CRC-32) carried ahead of the FEC packet region.
- Filename, detected file format, and encode timestamp stored *inside* the
  encrypted envelope, not the container metadata.

### Physical layer

- Two waveforms, selectable by preset: bin-aligned M-FSK (`standard`,
  `fast` — a readable spectrogram, gain-invariant detection) and OFDM/QAM
  (`dense`, `compact` — ~60x smaller carriers, up to 65536-QAM).
- The chosen tone plan is written into the carrier's own FLAC Vorbis-comment
  metadata, so `decode` and `info` configure themselves without being told
  the encoding parameters out of band.
- 1-8 interleaved audio channels, with an `auto` mode that spends channels
  only while the padding they cost stays under 1% of the frame.

### Cover audio ("radio mode")

- `--cover` hides the data under audible cover audio (FLAC/WAV/MP3/MP4-AAC),
  writing cover and data into disjoint OFDM subcarriers so there is zero
  interference by construction, not by budget.
- `--cover-quality auto` widens the audible band for small payloads (where
  the extra bytes are cheap) and narrows it for large ones; `--cover-mode
  spread` stretches a short payload across a whole recording so it plays to
  its end instead of cutting off mid-phrase.

### CLI

- `encode`, `decode`, `info`, `plan`, `completions` subcommands.
- Passphrase from an interactive prompt, `--passphrase-file`, or
  `AUDIO_MODEM_PASSPHRASE` — deliberately no `--passphrase` flag, since a
  command-line argument is visible in `ps` and shell history.
- Content-based format detection (~30 signatures) for naming piped payloads
  and reporting the recovered file's type without trusting an extension.
- Path-traversal-safe output resolution on `decode` (a stored name like
  `../../.ssh/authorized_keys` cannot steer the write outside the working
  directory).
- Streaming-friendly: `encode -` reads stdin, `decode -o -` writes stdout,
  composing with `tar`, `gpg`, `curl`, etc.

### Tests

- 142 tests across both crates: PHY conformance (orthogonality, zero leakage,
  phase continuity), OFDM/QAM exact-recovery limits, cover-audio isolation,
  full pipeline round trips, and CLI end-to-end/format-detection suites.
