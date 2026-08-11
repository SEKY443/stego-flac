//! Round trips over the file formats people actually carry.
//!
//! The rest of the suite proves the modem is correct on random bytes. This one
//! asks a different question: does the *tool* behave on a PDF, a PNG, an
//! archive, a spreadsheet — files with real structure, real entropy and real
//! names. Every fixture is generated in-process and is genuinely valid; the
//! first test proves that against the operating system's own utilities.

mod fixtures;

use std::fs;

use fixtures::{decode, encode, have, run, stderr, TempDir};

/// Every format the matrix covers, with a generator and the extension it is
/// stored under.
fn corpus() -> Vec<(&'static str, String, Vec<u8>)> {
    vec![
        ("pdf", "report.pdf".into(), fixtures::pdf(6)),
        ("png", "diagram.png".into(), fixtures::png(320, 240)),
        (
            "gzip",
            "logs.json.gz".into(),
            fixtures::gzip(&fixtures::json(500), "logs.json"),
        ),
        (
            "zip",
            "bundle.zip".into(),
            fixtures::zip(&[
                ("readme.md", fixtures::markdown(5)),
                ("data.csv", fixtures::csv(200)),
                ("lib.rs", fixtures::source_code(10)),
            ]),
        ),
        ("json", "records.json".into(), fixtures::json(2_000)),
        ("csv", "payroll.csv".into(), fixtures::csv(5_000)),
        ("markdown", "NOTES.md".into(), fixtures::markdown(60)),
        ("rust", "lib.rs".into(), fixtures::source_code(80)),
        (
            "media-like",
            "clip.bin".into(),
            fixtures::incompressible(300_000, 7),
        ),
        ("empty", "empty.dat".into(), Vec::new()),
        ("one-byte", "tiny.dat".into(), vec![0x42]),
    ]
}

#[test]
fn the_fixtures_are_files_the_system_recognises() {
    // "Valid" has to be a claim about something other than our own code, so the
    // generated files are handed to the operating system's tools. Each check is
    // skipped rather than failed when its tool is missing.
    let dir = TempDir::new("valid");

    let pdf = dir.join("doc.pdf");
    fs::write(&pdf, fixtures::pdf(6)).unwrap();
    let png = dir.join("image.png");
    fs::write(&png, fixtures::png(160, 120)).unwrap();
    let gz = dir.join("data.gz");
    fs::write(&gz, fixtures::gzip(&fixtures::csv(300), "data.csv")).unwrap();
    let zip = dir.join("bundle.zip");
    fs::write(
        &zip,
        fixtures::zip(&[("a.txt", b"hello".to_vec()), ("b.txt", b"world".to_vec())]),
    )
    .unwrap();

    if have("gzip") {
        let out = std::process::Command::new("gzip")
            .args(["-t", gz.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success(), "gzip rejected our archive: {out:?}");
    }

    if have("unzip") {
        let out = std::process::Command::new("unzip")
            .args(["-tq", zip.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success(), "unzip rejected our archive: {out:?}");
    }

    // `file` parses structure rather than sniffing extensions, so a wrong page
    // count or a broken PNG header shows up here.
    if have("file") {
        for (path, expected) in [(&pdf, "PDF document"), (&png, "PNG image")] {
            let out = std::process::Command::new("file")
                .arg(path.to_str().unwrap())
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(
                text.contains(expected),
                "expected {expected:?} in `file` output, got: {text}"
            );
        }
    }
}

#[test]
fn every_format_survives_a_round_trip() {
    let dir = TempDir::new("formats");
    let mut rows = Vec::new();

    for (label, name, data) in corpus() {
        let input = dir.join(&name);
        let carrier = dir.join(&format!("{label}.flac"));
        let landing = dir.join(&format!("{label}.out"));
        fs::write(&input, &data).unwrap();

        let out = encode(&input, &carrier, &[]);
        assert!(
            out.status.success(),
            "encoding {label} failed: {}",
            stderr(&out)
        );

        let out = decode(&carrier, &landing, &[]);
        assert!(
            out.status.success(),
            "decoding {label} failed: {}",
            stderr(&out)
        );

        let recovered = fs::read(&landing).unwrap();
        assert_eq!(recovered, data, "{label} did not round-trip byte for byte");

        let carrier_len = fs::metadata(&carrier).unwrap().len();
        rows.push((label, data.len(), carrier_len));
    }

    println!(
        "\n  {:<12} {:>10} {:>12} {:>10}",
        "format", "input", "carrier", "ratio"
    );
    for (label, input, carrier) in &rows {
        let ratio = if *input == 0 {
            String::from("-")
        } else {
            format!("{:.2}x", *carrier as f64 / *input as f64)
        };
        println!("  {label:<12} {input:>10} {carrier:>12} {ratio:>10}");
    }
}

#[test]
fn awkward_filenames_survive_and_stay_confined() {
    // The stored name decides where `decode` writes, so it is untrusted input.
    // These are the names that break naive handling: spaces, non-ASCII, no
    // extension, and a path that tries to escape the working directory.
    let dir = TempDir::new("names");
    let landing = dir.join("landing");
    fs::create_dir_all(&landing).unwrap();

    for name in [
        "report 2024 final.pdf",
        "計画書-v2 (最終).txt",
        "LICENSE",
        "archive.tar.gz",
        "déjà-vu.md",
    ] {
        let input = dir.join(name);
        let carrier = dir.join("c.flac");
        let data = fixtures::markdown(4);
        fs::write(&input, &data).unwrap();

        assert!(
            encode(&input, &carrier, &[]).status.success(),
            "encoding {name:?} failed"
        );

        let out = std::process::Command::new(fixtures::BIN)
            .args(["decode", carrier.to_str().unwrap(), "--force"])
            .env("AUDIO_MODEM_PASSPHRASE", fixtures::PASSPHRASE)
            .current_dir(&landing)
            .output()
            .unwrap();
        assert!(out.status.success(), "decoding {name:?} failed: {out:?}");

        let restored = landing.join(name);
        assert_eq!(
            fs::read(&restored).unwrap(),
            data,
            "{name:?} did not land under its own name"
        );
    }
}

#[test]
fn a_traversal_in_the_stored_name_cannot_escape() {
    let dir = TempDir::new("traversal");
    let input = dir.join("payload.txt");
    let carrier = dir.join("c.flac");
    let landing = dir.join("landing");
    fs::create_dir_all(&landing).unwrap();
    fs::write(&input, b"not going anywhere").unwrap();

    // The name is stored inside the encrypted payload, so it is chosen by
    // whoever made the carrier -- not necessarily by whoever opens it.
    assert!(encode(&input, &carrier, &["--name", "../../escaped.txt"])
        .status
        .success());

    let out = std::process::Command::new(fixtures::BIN)
        .args(["decode", carrier.to_str().unwrap(), "--force"])
        .env("AUDIO_MODEM_PASSPHRASE", fixtures::PASSPHRASE)
        .current_dir(&landing)
        .output()
        .unwrap();
    assert!(out.status.success(), "decode failed: {out:?}");

    assert!(
        landing.join("escaped.txt").exists(),
        "the name should be reduced to its final component and land here"
    );
    assert!(
        !dir.join("escaped.txt").exists() && !dir.0.parent().unwrap().join("escaped.txt").exists(),
        "a stored name escaped the working directory"
    );
}

#[test]
fn a_pdf_survives_every_profile() {
    // Profiles change the waveform entirely, so each needs a real payload run
    // through it end to end rather than only the synthetic sweeps.
    let dir = TempDir::new("profiles");
    let input = dir.join("report.pdf");
    let data = fixtures::pdf(12);
    fs::write(&input, &data).unwrap();

    for profile in ["standard", "fast", "dense", "compact"] {
        let carrier = dir.join(&format!("{profile}.flac"));
        let landing = dir.join(&format!("{profile}.pdf"));

        let out = encode(&input, &carrier, &["--profile", profile]);
        assert!(
            out.status.success(),
            "{profile} encode failed: {}",
            stderr(&out)
        );
        let out = decode(&carrier, &landing, &[]);
        assert!(
            out.status.success(),
            "{profile} decode failed: {}",
            stderr(&out)
        );
        assert_eq!(
            fs::read(&landing).unwrap(),
            data,
            "{profile} corrupted the PDF"
        );
    }
}

#[test]
fn a_recovered_pdf_is_still_a_pdf_to_the_system() {
    // Byte equality already implies this, but it is the claim a user actually
    // cares about, and it catches anything that mangles the file on the way out
    // -- a stray newline, a truncated tail, a text-mode write.
    if !have("file") {
        eprintln!("skipping: `file` is not installed");
        return;
    }

    let dir = TempDir::new("pdfvalid");
    let input = dir.join("report.pdf");
    let carrier = dir.join("c.flac");
    let landing = dir.join("out.pdf");
    fs::write(&input, fixtures::pdf(9)).unwrap();

    assert!(encode(&input, &carrier, &[]).status.success());
    assert!(decode(&carrier, &landing, &[]).status.success());

    let out = std::process::Command::new("file")
        .arg(landing.to_str().unwrap())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("PDF document"), "got: {text}");
    assert!(
        text.contains("9 pages"),
        "page tree did not survive: {text}"
    );
}

#[test]
fn an_archive_still_extracts_after_a_round_trip() {
    if !have("unzip") {
        eprintln!("skipping: `unzip` is not installed");
        return;
    }

    let dir = TempDir::new("zipvalid");
    let input = dir.join("bundle.zip");
    let carrier = dir.join("c.flac");
    let landing = dir.join("out.zip");
    fs::write(
        &input,
        fixtures::zip(&[
            ("notes.md", fixtures::markdown(8)),
            ("rows.csv", fixtures::csv(400)),
        ]),
    )
    .unwrap();

    assert!(encode(&input, &carrier, &[]).status.success());
    assert!(decode(&carrier, &landing, &[]).status.success());

    let out = std::process::Command::new("unzip")
        .args(["-tq", landing.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "unzip rejected the recovered archive: {out:?}"
    );
}

#[test]
fn info_reports_the_stored_name_extension_for_each_format() {
    // `info` never decrypts, so it must not leak the name -- but it should still
    // say that one is there, which is what tells a user the file will land with
    // its original identity.
    let dir = TempDir::new("infofmt");
    let input = dir.join("quarterly report.pdf");
    let carrier = dir.join("c.flac");
    fs::write(&input, fixtures::pdf(3)).unwrap();
    assert!(encode(&input, &carrier, &[]).status.success());

    let out = run(&["info", carrier.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);

    assert!(text.contains("filename stored    yes"), "got:\n{text}");
    assert!(
        !text.contains("quarterly report"),
        "info leaked the encrypted filename:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Automatic naming from detected format
// ---------------------------------------------------------------------------

#[test]
fn a_piped_payload_is_named_from_its_detected_format() {
    // Nothing on the command line says what this is: no filename, no --name,
    // no extension anywhere. The content identifies itself.
    let dir = TempDir::new("autoname-fmt");
    let carrier = dir.join("c.flac");
    let landing = dir.join("landing");
    fs::create_dir_all(&landing).unwrap();

    let pdf = fixtures::pdf(4);
    let out = fixtures::run_with_stdin(
        &["encode", "-", "-o", carrier.to_str().unwrap(), "--force"],
        &pdf,
    );
    assert!(out.status.success(), "encode failed: {}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("PDF document"),
        "the summary should name the detected format"
    );

    let out = std::process::Command::new(fixtures::BIN)
        .args(["decode", carrier.to_str().unwrap(), "--force"])
        .env("AUDIO_MODEM_PASSPHRASE", fixtures::PASSPHRASE)
        .current_dir(&landing)
        .output()
        .unwrap();
    assert!(out.status.success(), "decode failed: {out:?}");

    let landed = landing.join("payload.pdf");
    assert!(
        landed.exists(),
        "expected payload.pdf, got {:?}",
        fs::read_dir(&landing).unwrap().count()
    );
    assert_eq!(fs::read(&landed).unwrap(), pdf);

    if have("file") {
        let probe = std::process::Command::new("file")
            .arg(landed.to_str().unwrap())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).into_owned();
        assert!(text.contains("PDF document"), "got: {text}");
    }
}

#[test]
fn each_format_is_recognised_end_to_end() {
    let dir = TempDir::new("detect-matrix");
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("pdf", fixtures::pdf(2), "PDF document"),
        ("png", fixtures::png(32, 32), "PNG image"),
        (
            "gzip",
            fixtures::gzip(&fixtures::csv(50), "d.csv"),
            "gzip archive",
        ),
        (
            "zip",
            fixtures::zip(&[("a.md", fixtures::markdown(2))]),
            "ZIP archive",
        ),
    ];

    for (label, data, description) in cases {
        let carrier = dir.join(&format!("{label}.flac"));
        let out = fixtures::run_with_stdin(
            &["encode", "-", "-o", carrier.to_str().unwrap(), "--force"],
            &data,
        );
        assert!(out.status.success(), "{label}: {}", stderr(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(description),
            "{label} was not reported as {description:?}"
        );

        let landing = dir.join(&format!("{label}.out"));
        let out = decode(&carrier, &landing, &[]);
        assert!(out.status.success(), "{label}: {}", stderr(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(description),
            "{label} decode did not report {description:?}"
        );
        assert_eq!(fs::read(&landing).unwrap(), data);
    }
}

#[test]
fn an_extensionless_name_comes_back_unchanged() {
    // PDF bytes stored under a name with no extension. Returning `LICENSE.pdf`
    // would be a guess about the user's intent; returning `LICENSE` is what
    // they gave us.
    let dir = TempDir::new("verbatim");
    let input = dir.join("LICENSE");
    let carrier = dir.join("c.flac");
    let landing = dir.join("landing");
    fs::create_dir_all(&landing).unwrap();
    fs::write(&input, fixtures::pdf(2)).unwrap();

    assert!(encode(&input, &carrier, &[]).status.success());

    let out = std::process::Command::new(fixtures::BIN)
        .args(["decode", carrier.to_str().unwrap(), "--force"])
        .env("AUDIO_MODEM_PASSPHRASE", fixtures::PASSPHRASE)
        .current_dir(&landing)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    assert!(landing.join("LICENSE").exists(), "the name was altered");
    assert!(
        !landing.join("LICENSE.pdf").exists(),
        "an extension was invented"
    );
    // ...but the user is still told what it is.
    assert!(String::from_utf8_lossy(&out.stdout).contains("PDF document"));
}

#[test]
fn no_store_name_suppresses_the_format_as_well() {
    let dir = TempDir::new("nometa");
    let input = dir.join("secret.pdf");
    let carrier = dir.join("c.flac");
    fs::write(&input, fixtures::pdf(2)).unwrap();

    let out = encode(&input, &carrier, &["--no-store-name"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("not stored"));

    let out = run(&["info", carrier.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("filename stored    no"), "got:\n{text}");
    assert!(text.contains("format stored      no"), "got:\n{text}");
}

// ---------------------------------------------------------------------------
// Multichannel carriers
// ---------------------------------------------------------------------------

#[test]
fn extra_channels_divide_duration_and_leave_size_alone() {
    // The trade this feature exists to make. Channel is the only axis of the
    // storage tensor not already spoken for -- time, frequency and I/Q are
    // three views of the same `N` samples -- so it adds real capacity per
    // second, and real bytes per second, in equal measure.
    let dir = TempDir::new("mc-trade");
    let input = dir.join("blob.bin");
    let payload = fixtures::incompressible(400_000, 5);
    fs::write(&input, &payload).unwrap();

    let mut first_duration = 0.0f64;
    let mut first_size = 0u64;

    for channels in [1usize, 2, 4, 8] {
        let carrier = dir.join(&format!("c{channels}.flac"));
        let landing = dir.join(&format!("c{channels}.out"));

        let out = encode(&input, &carrier, &["--channels", &channels.to_string()]);
        assert!(out.status.success(), "{channels}ch: {}", stderr(&out));

        let out = decode(&carrier, &landing, &[]);
        assert!(out.status.success(), "{channels}ch: {}", stderr(&out));
        assert_eq!(
            fs::read(&landing).unwrap(),
            payload,
            "{channels}ch did not round-trip"
        );

        // Duration comes from the container's own header.
        let raw = fs::read(&carrier).unwrap();
        let info = &raw[8..8 + 34];
        let packed = u64::from_be_bytes(info[10..18].try_into().unwrap());
        let declared_channels = ((packed >> 41) & 0x7) + 1;
        let frames = packed & 0xf_ffff_ffff;
        assert_eq!(
            declared_channels as usize, channels,
            "channel count in STREAMINFO"
        );

        let duration = frames as f64 / 24_000.0;
        let size = fs::metadata(&carrier).unwrap().len();

        if channels == 1 {
            first_duration = duration;
            first_size = size;
        } else {
            // Not exactly `channels`: every lane rounds up to a whole symbol
            // and the container pads to a whole block of frames, so the
            // shortfall grows slightly with lane count (7.85x at eight). A
            // proportional tolerance still catches a real regression.
            let speedup = first_duration / duration;
            assert!(
                (speedup - channels as f64).abs() < 0.1 * channels as f64,
                "{channels}ch shortened the carrier {speedup:.2}x, expected ~{channels}x"
            );
            // Size must stay in the same neighbourhood. If this ever fires,
            // channels have started buying or costing bytes and the feature's
            // whole justification needs revisiting.
            let ratio = size as f64 / first_size as f64;
            assert!(
                (0.8..1.25).contains(&ratio),
                "{channels}ch changed the file size {ratio:.3}x; the trade was supposed \
                 to be duration only"
            );
        }
    }
}

#[test]
fn a_multichannel_carrier_is_still_a_playable_flac() {
    let dir = TempDir::new("mc-valid");
    let input = dir.join("m.bin");
    fs::write(&input, fixtures::incompressible(60_000, 11)).unwrap();

    for channels in ["2", "8"] {
        let carrier = dir.join(&format!("c{channels}.flac"));
        assert!(encode(&input, &carrier, &["--channels", channels])
            .status
            .success());

        if have("flac") {
            let out = std::process::Command::new("flac")
                .args(["-t", carrier.to_str().unwrap()])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "the reference decoder rejected a {channels}-channel carrier"
            );
        }
    }
}

#[test]
fn decoding_needs_no_channel_flag() {
    // The channel count is read from the FLAC header, so nothing has to be
    // recorded in the metadata or remembered by whoever opens the file.
    let dir = TempDir::new("mc-auto");
    let input = dir.join("m.bin");
    let carrier = dir.join("c.flac");
    let landing = dir.join("out.bin");
    let payload = fixtures::incompressible(50_000, 3);
    fs::write(&input, &payload).unwrap();

    assert!(encode(&input, &carrier, &["--channels", "4"])
        .status
        .success());

    let out = decode(&carrier, &landing, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fs::read(&landing).unwrap(), payload);

    let out = run(&["info", carrier.to_str().unwrap()]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("4 channel(s)"),
        "info should report the channel count"
    );
}

#[test]
fn multichannel_carries_a_real_pdf_intact() {
    let dir = TempDir::new("mc-pdf");
    let input = dir.join("report.pdf");
    let carrier = dir.join("c.flac");
    let landing = dir.join("out.pdf");
    let pdf = fixtures::pdf(8);
    fs::write(&input, &pdf).unwrap();

    assert!(encode(&input, &carrier, &["--channels", "8"])
        .status
        .success());
    assert!(decode(&carrier, &landing, &[]).status.success());
    assert_eq!(fs::read(&landing).unwrap(), pdf);

    if have("file") {
        let out = std::process::Command::new("file")
            .arg(landing.to_str().unwrap())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(text.contains("8 pages"), "got: {text}");
    }
}

// ---------------------------------------------------------------------------
// Cover audio
// ---------------------------------------------------------------------------

/// A 44.1 kHz stereo WAV of voice-like tones, to exercise downmix and resample.
fn voice_wav(seconds: usize) -> Vec<u8> {
    let rate = 44_100usize;
    let frames = rate * seconds;
    let mut pcm = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let t = i as f32 / rate as f32;
        let envelope = 0.5 + 0.5 * (std::f32::consts::TAU * 2.5 * t).sin();
        let value = envelope
            * (0.6 * (std::f32::consts::TAU * 210.0 * t).sin()
                + 0.35 * (std::f32::consts::TAU * 880.0 * t).sin()
                + 0.2 * (std::f32::consts::TAU * 2600.0 * t).sin())
            * 0.7;
        let sample = (value.clamp(-1.0, 1.0) * 32767.0) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
        pcm.extend_from_slice(&sample.to_le_bytes());
    }

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
    wav.extend_from_slice(&(rate as u32).to_le_bytes());
    wav.extend_from_slice(&((rate * 4) as u32).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(&pcm);
    wav
}

#[test]
fn a_covered_carrier_round_trips_and_needs_no_flags_to_open() {
    let dir = TempDir::new("cover");
    let (input, cover, carrier, landing) = (
        dir.join("secret.bin"),
        dir.join("voice.wav"),
        dir.join("radio.flac"),
        dir.join("out.bin"),
    );
    let payload = fixtures::incompressible(120_000, 21);
    fs::write(&input, &payload).unwrap();
    fs::write(&cover, voice_wav(4)).unwrap();

    let out = encode(&input, &carrier, &["--cover", cover.to_str().unwrap()]);
    assert!(out.status.success(), "encode failed: {}", stderr(&out));
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(report.contains("cover audio"), "got:\n{report}");

    // The cover band is recorded in the plan, so decode needs nothing.
    let out = decode(&carrier, &landing, &[]);
    assert!(out.status.success(), "decode failed: {}", stderr(&out));
    assert_eq!(fs::read(&landing).unwrap(), payload);

    if have("flac") {
        let probe = std::process::Command::new("flac")
            .args(["-t", carrier.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "reference decoder rejected the carrier"
        );
    }
}

#[test]
fn cover_and_multichannel_are_refused_together() {
    // Individually both work; combined they silently produced a mono carrier
    // written as N interleaved channels, which is garbage. Better to say so.
    let dir = TempDir::new("cover-mc");
    let (input, cover, carrier) = (dir.join("m.bin"), dir.join("v.wav"), dir.join("c.flac"));
    fs::write(&input, fixtures::incompressible(40_000, 3)).unwrap();
    fs::write(&cover, voice_wav(2)).unwrap();

    let out = encode(
        &input,
        &carrier,
        &["--cover", cover.to_str().unwrap(), "--channels", "8"],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--cover"), "{}", stderr(&out));
}

#[test]
fn cover_is_refused_on_a_waveform_with_no_spectrum_to_share() {
    let dir = TempDir::new("cover-fsk");
    let (input, cover, carrier) = (dir.join("m.bin"), dir.join("v.wav"), dir.join("c.flac"));
    fs::write(&input, fixtures::incompressible(4_000, 3)).unwrap();
    fs::write(&cover, voice_wav(1)).unwrap();

    let out = encode(
        &input,
        &carrier,
        &["--cover", cover.to_str().unwrap(), "--profile", "standard"],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("one \n                 tone")
            || stderr(&out).contains("tone at a time"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn auto_channels_keeps_small_payloads_on_one_channel() {
    let dir = TempDir::new("autoch");
    let small = dir.join("small.bin");
    let large = dir.join("large.bin");
    fs::write(&small, fixtures::incompressible(3_000, 1)).unwrap();
    fs::write(&large, fixtures::incompressible(600_000, 2)).unwrap();

    let out = encode(&small, &dir.join("s.flac"), &["--channels", "auto"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("across"),
        "a small payload should stay on one channel"
    );

    let out = encode(&large, &dir.join("l.flac"), &["--channels", "auto"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("across"),
        "a large payload should use several channels"
    );
}

#[test]
fn cover_mode_spread_plays_the_recording_to_its_end() {
    // A short payload against a long cover. `cut` stops when the data does,
    // truncating the recording; `spread` deals the data out across the whole
    // thing so it finishes.
    let dir = TempDir::new("covermode");
    let input = dir.join("small.bin");
    let cover = dir.join("v.wav");
    fs::write(&input, fixtures::incompressible(20_000, 77)).unwrap();
    fs::write(&cover, voice_wav(20)).unwrap();

    let mut lengths = Vec::new();
    for mode in ["cut", "spread"] {
        let carrier = dir.join(&format!("{mode}.flac"));
        let landing = dir.join(&format!("{mode}.out"));

        let out = encode(
            &input,
            &carrier,
            &["--cover", cover.to_str().unwrap(), "--cover-mode", mode],
        );
        assert!(out.status.success(), "{mode}: {}", stderr(&out));

        let out = decode(&carrier, &landing, &[]);
        assert!(out.status.success(), "{mode}: {}", stderr(&out));
        assert_eq!(
            fs::read(&landing).unwrap(),
            fs::read(&input).unwrap(),
            "{mode} did not round-trip"
        );

        let raw = fs::read(&carrier).unwrap();
        let packed = u64::from_be_bytes(raw[18..26].try_into().unwrap());
        lengths.push(packed & 0xf_ffff_ffff);
    }

    let (cut, spread) = (lengths[0], lengths[1]);
    assert!(
        spread > cut * 4,
        "spread should stretch the carrier well beyond cut, got {spread} vs {cut}"
    );
    // ...and it should land near the cover's own length, ~20 s at 24 kHz.
    assert!(
        (spread as f64 / 24_000.0 - 20.0).abs() < 1.5,
        "spread carrier is {:.1} s, expected ~20 s",
        spread as f64 / 24_000.0
    );
}

#[test]
fn cover_mode_requires_a_cover() {
    let dir = TempDir::new("covermodeflag");
    let input = dir.join("m.bin");
    fs::write(&input, b"hello").unwrap();

    let out = encode(&input, &dir.join("c.flac"), &["--cover-mode", "spread"]);
    assert!(
        !out.status.success(),
        "--cover-mode alone should be refused"
    );
}
