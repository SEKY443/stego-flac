//! End-to-end tests through the real binary and a real FLAC file.
//!
//! The core crate's tests cover the pipeline; these cover what only the binary
//! can: the FLAC container, argument handling, and the exit codes a user or a
//! script actually sees.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_stego-flac");
const PASSPHRASE: &str = "correct horse battery staple";

/// A scratch directory that removes itself.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = format!(
            "audio-modem-test-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("creating the scratch directory");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("AUDIO_MODEM_PASSPHRASE", PASSPHRASE)
        .output()
        .expect("running stego-flac")
}

/// Deterministic pseudo-random bytes.
///
/// A real xorshift rather than `i * constant >> shift`: the latter has enough
/// structure that zstd crushes it, which quietly turns an "incompressible
/// payload" test into a tiny-carrier test.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn encode(input: &Path, output: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "encode",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--force",
    ];
    args.extend_from_slice(extra);
    run(&args)
}

fn decode(input: &Path, output: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "decode",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--force",
    ];
    args.extend_from_slice(extra);
    run(&args)
}

#[test]
fn encrypted_round_trip_through_a_real_flac_file() {
    let dir = TempDir::new("rt");
    let (input, carrier, recovered) = (
        dir.join("message.txt"),
        dir.join("message.flac"),
        dir.join("out.txt"),
    );

    let payload = "attack at dawn, bring snacks. ".repeat(300);
    fs::write(&input, &payload).unwrap();

    let out = encode(&input, &carrier, &[]);
    assert!(out.status.success(), "encode failed: {}", stderr(&out));
    assert!(carrier.exists());

    // The carrier really is a FLAC file, not just bytes we named `.flac`.
    assert_eq!(&fs::read(&carrier).unwrap()[..4], b"fLaC");

    let out = decode(&carrier, &recovered, &[]);
    assert!(out.status.success(), "decode failed: {}", stderr(&out));
    assert_eq!(fs::read_to_string(&recovered).unwrap(), payload);
}

#[test]
fn unencrypted_binary_round_trip() {
    let dir = TempDir::new("bin");
    let (input, carrier, recovered) = (
        dir.join("blob.bin"),
        dir.join("blob.flac"),
        dir.join("blob.out"),
    );

    // Pseudo-random and therefore incompressible, so the compression-skip path
    // and the largest realistic carrier are both exercised.
    let payload = pseudo_random(8192, 12345);
    fs::write(&input, &payload).unwrap();

    let out = encode(&input, &carrier, &["--no-encrypt"]);
    assert!(out.status.success(), "encode failed: {}", stderr(&out));
    assert!(
        stderr(&out).contains("not encrypted"),
        "an unencrypted carrier must warn the user"
    );

    let out = decode(&carrier, &recovered, &[]);
    assert!(out.status.success(), "decode failed: {}", stderr(&out));
    assert_eq!(fs::read(&recovered).unwrap(), payload);
}

#[test]
fn info_reports_the_header_without_the_passphrase() {
    let dir = TempDir::new("info");
    let (input, carrier) = (dir.join("m.txt"), dir.join("m.flac"));
    fs::write(&input, "hello ".repeat(200)).unwrap();

    assert!(encode(&input, &carrier, &[]).status.success());

    let out = Command::new(BIN)
        .args(["info", carrier.to_str().unwrap()])
        .env_remove("AUDIO_MODEM_PASSPHRASE")
        .output()
        .unwrap();

    assert!(out.status.success(), "info failed: {}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("encrypted          yes"), "got:\n{text}");
    assert!(text.contains("24000 Hz, 1 channel(s)"), "got:\n{text}");
    assert!(text.contains("argon2id"), "got:\n{text}");
}

#[test]
fn a_wrong_passphrase_fails_without_writing_output() {
    let dir = TempDir::new("wrong");
    let (input, carrier, recovered) = (dir.join("m.txt"), dir.join("m.flac"), dir.join("out.txt"));
    fs::write(&input, "sensitive ".repeat(100)).unwrap();

    assert!(encode(&input, &carrier, &[]).status.success());

    let out = Command::new(BIN)
        .args([
            "decode",
            carrier.to_str().unwrap(),
            "-o",
            recovered.to_str().unwrap(),
            "--force",
        ])
        .env("AUDIO_MODEM_PASSPHRASE", "not the passphrase")
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("wrong passphrase"),
        "{}",
        stderr(&out)
    );
    assert!(
        !recovered.exists(),
        "a failed decode must not leave a partial file behind"
    );
}

#[test]
fn a_tone_plan_mismatch_is_diagnosed_clearly() {
    let dir = TempDir::new("plan");
    let (input, carrier, recovered) = (dir.join("m.txt"), dir.join("m.flac"), dir.join("out.txt"));
    fs::write(&input, "plan mismatch ".repeat(100)).unwrap();

    assert!(encode(&input, &carrier, &["--profile", "standard"])
        .status
        .success());

    let out = decode(
        &carrier,
        &recovered,
        &["--profile", "standard", "--bits-per-symbol", "2"],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("tone plan differs"),
        "the error should point at the tone plan, got: {}",
        stderr(&out)
    );
}

#[test]
fn a_waveform_specific_flag_is_rejected_on_the_wrong_plan() {
    let dir = TempDir::new("wrongflag");
    let input = dir.join("m.txt");
    fs::write(&input, "hello").unwrap();

    // --qam-bits is meaningless for FSK, and --bits-per-symbol for OFDM.
    // Silently ignoring either would leave the user with a plan they did not
    // ask for, so both are refused.
    let out = encode(
        &input,
        &dir.join("a.flac"),
        &["--profile", "standard", "--qam-bits", "8"],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("OFDM plans"), "{}", stderr(&out));

    let out = encode(
        &input,
        &dir.join("b.flac"),
        &["--profile", "dense", "--bits-per-symbol", "4"],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("FSK plans"), "{}", stderr(&out));
}

#[test]
fn a_truncated_carrier_is_recovered_by_the_repair_symbols() {
    let dir = TempDir::new("trunc");
    let (input, carrier, clipped, recovered) = (
        dir.join("m.txt"),
        dir.join("m.flac"),
        dir.join("clipped.flac"),
        dir.join("out.txt"),
    );

    // Incompressible and large enough to span many FLAC frames. This matters
    // more than it used to: the dense waveform packs a small payload into one
    // or two FLAC blocks, and clipping a file mid-block destroys that whole
    // block rather than just a tail, so a tiny carrier has nothing left to
    // reconstruct from.
    let payload = pseudo_random(400_000, 0xC0FFEE);
    fs::write(&input, &payload).unwrap();
    assert!(encode(&input, &carrier, &["--fec-overhead", "30"])
        .status
        .success());

    // Chop the tail, as a failed upload or an over-eager editor would.
    let full = fs::read(&carrier).unwrap();
    fs::write(&clipped, &full[..full.len() * 90 / 100]).unwrap();

    let out = decode(&clipped, &recovered, &[]);
    assert!(
        out.status.success(),
        "a 10% truncation should be recoverable: {}",
        stderr(&out)
    );
    assert_eq!(fs::read(&recovered).unwrap(), payload);
}

#[test]
fn existing_output_is_not_clobbered_without_force() {
    let dir = TempDir::new("force");
    let (input, carrier) = (dir.join("m.txt"), dir.join("m.flac"));
    fs::write(&input, "hello").unwrap();
    fs::write(&carrier, "PRECIOUS").unwrap();

    let out = run(&[
        "encode",
        input.to_str().unwrap(),
        "-o",
        carrier.to_str().unwrap(),
    ]);

    assert!(!out.status.success());
    assert!(stderr(&out).contains("--force"), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(&carrier).unwrap(),
        "PRECIOUS",
        "the existing file must be untouched"
    );
}

#[test]
fn a_non_flac_input_is_rejected() {
    let dir = TempDir::new("notflac");
    let junk = dir.join("notes.txt");
    fs::write(&junk, "this is plainly not audio").unwrap();

    let out = run(&["info", junk.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("FLAC"), "{}", stderr(&out));
}

#[test]
fn an_invalid_tone_plan_is_refused_before_any_work() {
    let dir = TempDir::new("badplan");
    let input = dir.join("m.txt");
    fs::write(&input, "hello").unwrap();

    // 3 bits per symbol does not divide 8, so the FSK byte packing is undefined.
    let out = encode(
        &input,
        &dir.join("m.flac"),
        &["--profile", "standard", "--bits-per-symbol", "3"],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("invalid plan"), "{}", stderr(&out));
}

#[test]
fn plan_reports_a_consistent_throughput_budget() {
    let out = run(&["plan", "--profile", "standard"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("500.0 baud"), "got:\n{text}");
    assert!(text.contains("2000 bit/s"), "got:\n{text}");
    assert!(text.contains("2000 Hz .. 9500 Hz"), "got:\n{text}");

    // The default is now OFDM, and it must be dramatically denser.
    let out = run(&["plan"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("OFDM"), "got:\n{text}");
    assert!(text.contains("4096-QAM"), "got:\n{text}");
    assert!(text.contains("subcarriers"), "got:\n{text}");
}

// ---------------------------------------------------------------------------
// Metadata, auto-configuration, and filename restoration
// ---------------------------------------------------------------------------

#[test]
fn decode_needs_no_flags_even_for_a_non_default_profile() {
    let dir = TempDir::new("autoplan");
    let (input, carrier, recovered) = (dir.join("m.txt"), dir.join("m.flac"), dir.join("out.txt"));
    let payload = "self describing carrier. ".repeat(200);
    fs::write(&input, &payload).unwrap();

    // Encode with a profile the decoder is never told about.
    let out = encode(&input, &carrier, &["--profile", "fast"]);
    assert!(out.status.success(), "encode failed: {}", stderr(&out));

    // No --profile, no tone-plan flags. This is the whole point of recording
    // the plan in the carrier's metadata.
    let out = decode(&carrier, &recovered, &[]);
    assert!(
        out.status.success(),
        "decode should self-configure from metadata: {}",
        stderr(&out)
    );
    assert_eq!(fs::read_to_string(&recovered).unwrap(), payload);
}

#[test]
fn the_original_filename_is_restored_without_an_output_flag() {
    let dir = TempDir::new("autoname");
    let input = dir.join("important-notes.txt");
    let carrier = dir.join("carrier.flac");
    let payload = "restore my name. ".repeat(100);
    fs::write(&input, &payload).unwrap();

    assert!(encode(&input, &carrier, &[]).status.success());

    // Decode into a fresh directory with no -o at all. The stored name decides
    // where the payload lands, relative to the working directory.
    let landing = dir.join("landing");
    fs::create_dir_all(&landing).unwrap();
    let out = Command::new(BIN)
        .args(["decode", carrier.to_str().unwrap()])
        .env("AUDIO_MODEM_PASSPHRASE", PASSPHRASE)
        .current_dir(&landing)
        .output()
        .expect("running decode");

    assert!(out.status.success(), "decode failed: {}", stderr(&out));
    assert_eq!(
        fs::read_to_string(landing.join("important-notes.txt")).unwrap(),
        payload,
        "the payload should land under its original name"
    );
}

#[test]
fn a_carrier_without_a_stored_name_asks_for_an_output_path() {
    let dir = TempDir::new("noname");
    let (input, carrier) = (dir.join("m.txt"), dir.join("m.flac"));
    fs::write(&input, "no name stored").unwrap();

    assert!(encode(&input, &carrier, &["--no-store-name"])
        .status
        .success());

    let out = run(&["decode", carrier.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("-o"),
        "should tell the user to pass -o, got: {}",
        stderr(&out)
    );
}

#[test]
fn info_reports_the_profile_recorded_in_metadata() {
    let dir = TempDir::new("infoplan");
    let (input, carrier) = (dir.join("m.txt"), dir.join("m.flac"));
    fs::write(&input, "hello ".repeat(200)).unwrap();

    assert!(encode(&input, &carrier, &["--profile", "fast"])
        .status
        .success());

    let out = run(&["info", carrier.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("fast"), "got:\n{text}");
    assert!(text.contains("4000 bit/s"), "got:\n{text}");
    assert!(text.contains("filename stored    yes"), "got:\n{text}");
}

#[test]
fn explicit_flags_override_the_recorded_plan() {
    let dir = TempDir::new("override");
    let (input, carrier, recovered) = (dir.join("m.txt"), dir.join("m.flac"), dir.join("out.txt"));
    fs::write(&input, "override me ".repeat(100)).unwrap();

    assert!(encode(&input, &carrier, &["--profile", "fast"])
        .status
        .success());

    // Deliberately wrong: the user's word beats the file's metadata, so this
    // must fail rather than silently self-correct.
    let out = decode(&carrier, &recovered, &["--profile", "standard"]);
    assert!(
        !out.status.success(),
        "an explicit wrong profile must not be silently overridden by metadata"
    );
}

#[test]
fn the_carrier_is_a_tagged_flac_file() {
    let dir = TempDir::new("tags");
    let (input, carrier) = (dir.join("m.txt"), dir.join("m.flac"));
    fs::write(&input, "tag me ".repeat(100)).unwrap();

    assert!(encode(&input, &carrier, &[]).status.success());

    let raw = fs::read(&carrier).unwrap();
    let text = String::from_utf8_lossy(&raw[..raw.len().min(2048)]);

    // Tags are stored as plain UTF-8 in a VORBIS_COMMENT block, so they are
    // findable in the file's opening bytes.
    assert!(text.contains("TITLE=stego-flac carrier"), "no TITLE tag");
    assert!(text.contains("AUDIOMODEM_PLAN=mode=ofdm"), "no plan tag");
    assert!(text.contains("stego-flac"), "no vendor string");
}

/// Validate a carrier with the reference FLAC decoder, if it is installed.
///
/// Skipped rather than failed when `flac` is absent, so the suite still runs on
/// a machine without it. This is the one check that does not rely on the same
/// library we encode with, which is exactly what makes it worth having: an
/// earlier bug produced a file that `symphonia` rejected outright while our own
/// round trip passed.
#[test]
fn the_carrier_validates_against_the_reference_flac_decoder() {
    let Ok(probe) = Command::new("flac").arg("--version").output() else {
        eprintln!("skipping: the reference `flac` tool is not installed");
        return;
    };
    if !probe.status.success() {
        eprintln!("skipping: `flac --version` failed");
        return;
    }

    let dir = TempDir::new("refdec");
    let (input, carrier) = (dir.join("m.txt"), dir.join("m.flac"));
    fs::write(&input, "verify me ".repeat(300)).unwrap();
    assert!(encode(&input, &carrier, &[]).status.success());

    // `flac -t` fully decodes the stream and verifies the STREAMINFO MD5.
    let out = Command::new("flac")
        .args(["-t", carrier.to_str().unwrap()])
        .output()
        .expect("running flac -t");

    assert!(
        out.status.success(),
        "the reference decoder rejected our carrier:\n{}",
        stderr(&out)
    );
}

#[test]
fn the_carrier_is_well_formed_audio() {
    let dir = TempDir::new("levels");
    let (input, carrier) = (dir.join("m.txt"), dir.join("m.flac"));
    fs::write(&input, "level check ".repeat(200)).unwrap();
    assert!(encode(&input, &carrier, &[]).status.success());

    let raw = fs::read(&carrier).unwrap();

    // STREAMINFO sits at a fixed offset and states the stream's own shape, so
    // this asserts what a player will actually be told about the file.
    let info = &raw[8..8 + 34];
    let packed = u64::from_be_bytes(info[10..18].try_into().unwrap());
    let sample_rate = (packed >> 44) & 0xf_ffff;
    let channels = ((packed >> 41) & 0x7) + 1;
    let bits = ((packed >> 36) & 0x1f) + 1;
    let total = packed & 0xf_ffff_ffff;

    assert_eq!(sample_rate, 24_000, "sample rate");
    assert_eq!(channels, 1, "channel count");
    assert_eq!(bits, 16, "bit depth");
    assert!(total > 0, "the stream declares no samples");

    // A non-zero MD5 in STREAMINFO is what lets any decoder verify the audio;
    // leaving it zeroed is legal but means the file cannot be integrity-checked.
    assert_ne!(&info[18..34], &[0u8; 16], "STREAMINFO MD5 was left unset");

    // Blocking must be fixed (min == max) or strict readers reject every frame.
    let min_block = u16::from_be_bytes(info[0..2].try_into().unwrap());
    let max_block = u16::from_be_bytes(info[2..4].try_into().unwrap());
    assert_eq!(
        min_block, max_block,
        "min != max block size makes strict readers infer variable blocking"
    );
}

// ---------------------------------------------------------------------------
// Streaming and scripting
// ---------------------------------------------------------------------------

use std::io::Write;
use std::process::Stdio;

/// Run the binary with `input` on stdin.
fn run_with_stdin(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .env("AUDIO_MODEM_PASSPHRASE", PASSPHRASE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning stego-flac");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("writing stdin");
    child.wait_with_output().expect("waiting for stego-flac")
}

#[test]
fn a_payload_can_be_piped_in_and_out_without_touching_disk() {
    // This is what makes the tool composable: `tar czf - dir | encode -` on one
    // side, `decode -o - | tar xzf -` on the other. Without it, every use needs
    // a scratch file and only single files can be carried.
    let dir = TempDir::new("pipe");
    let carrier = dir.join("c.flac");
    let payload = pseudo_random(30_000, 4242);

    let out = run_with_stdin(
        &[
            "encode",
            "-",
            "-o",
            carrier.to_str().unwrap(),
            "--name",
            "piped.bin",
            "--quiet",
            "--force",
        ],
        &payload,
    );
    assert!(out.status.success(), "encode failed: {}", stderr(&out));
    assert!(
        out.stdout.is_empty(),
        "--quiet should print nothing on stdout"
    );

    let out = run(&["decode", carrier.to_str().unwrap(), "-o", "-", "--quiet"]);
    assert!(out.status.success(), "decode failed: {}", stderr(&out));
    assert_eq!(
        out.stdout, payload,
        "the payload must arrive on stdout byte for byte"
    );
}

#[test]
fn writing_the_payload_to_stdout_keeps_the_summary_off_it() {
    // stdout becomes a data channel, so anything human-readable has to move to
    // stderr or it corrupts whatever is downstream in the pipe.
    let dir = TempDir::new("pipeclean");
    let (input, carrier) = (dir.join("m.bin"), dir.join("c.flac"));
    let payload = pseudo_random(4096, 7);
    fs::write(&input, &payload).unwrap();
    assert!(encode(&input, &carrier, &[]).status.success());

    let out = run(&["decode", carrier.to_str().unwrap(), "-o", "-"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        out.stdout, payload,
        "stdout carried something other than the payload"
    );
    assert!(
        stderr(&out).contains("recovered"),
        "the summary should still appear, on stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_carrier_cannot_be_streamed_to_stdout() {
    // FLAC's STREAMINFO records a total sample count and an MD5 of the audio,
    // neither known until the last sample exists, so the header is rewritten at
    // the end and the sink must be seekable. Better to say so than to emit a
    // file whose header lies.
    let dir = TempDir::new("nostream");
    let input = dir.join("m.txt");
    fs::write(&input, "hello").unwrap();

    let out = run(&["encode", input.to_str().unwrap(), "-o", "-"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("standard output"), "{}", stderr(&out));
}

#[test]
fn stdin_input_requires_an_output_path() {
    let out = run_with_stdin(&["encode", "-"], b"data");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("-o is required"), "{}", stderr(&out));
}

#[test]
fn completions_are_emitted_for_the_common_shells() {
    for shell in ["bash", "zsh", "fish"] {
        let out = run(&["completions", shell]);
        assert!(out.status.success(), "{shell}: {}", stderr(&out));
        let script = String::from_utf8_lossy(&out.stdout);
        assert!(script.len() > 1000, "{shell} script looks empty");
        assert!(
            script.contains("stego-flac"),
            "{shell} script does not mention the binary"
        );
    }
}

#[test]
fn the_compact_profile_is_denser_and_still_exact() {
    let dir = TempDir::new("compact");
    let (input, carrier, recovered) = (dir.join("m.bin"), dir.join("c.flac"), dir.join("out.bin"));
    let payload = pseudo_random(120_000, 31);
    fs::write(&input, &payload).unwrap();

    assert!(encode(&input, &carrier, &["--profile", "compact"])
        .status
        .success());
    let compact = fs::metadata(&carrier).unwrap().len();

    assert!(encode(&input, &carrier, &["--profile", "dense"])
        .status
        .success());
    let dense = fs::metadata(&carrier).unwrap().len();

    assert!(
        compact < dense,
        "compact ({compact}) should beat dense ({dense})"
    );

    // ...and it must still be exact, which is the whole question with a denser
    // constellation.
    assert!(encode(&input, &carrier, &["--profile", "compact"])
        .status
        .success());
    assert!(decode(&carrier, &recovered, &[]).status.success());
    assert_eq!(fs::read(&recovered).unwrap(), payload);
}
