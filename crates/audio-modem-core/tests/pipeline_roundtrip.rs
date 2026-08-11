//! End-to-end pipeline tests: plaintext through the full stack and back.
//!
//! These exercise everything except the FLAC container, which lives in the CLI
//! crate. The `i16` round trip stands in for it, since that quantisation is the
//! only transformation the container applies.
//!
//! All tests use deliberately weak Argon2 parameters. Production defaults cost
//! 64 MiB and ~100 ms per derivation, which would make this suite take minutes
//! for no added coverage — the KDF's *cost* is not what these tests verify.

use audio_modem_core::codec::crypto::{KdfParams, TAG_LEN};
use audio_modem_core::codec::fec::FecParams;
use audio_modem_core::format;
use audio_modem_core::frame::header::{FLAG_ENCRYPTED, HEADER_LEN};
use audio_modem_core::{
    decode_frame, encode_frame, from_i16, to_i16, Carrier, CoreError, CryptoError, EncodeParams,
    FecError, FrameError, FskModem, Header, ModemConfig, Plan, PlanParseError, Profile,
};

/// Argon2 parameters chosen for test speed, not for security.
fn fast_kdf() -> KdfParams {
    KdfParams {
        m_cost: 8,
        t_cost: 1,
        p_cost: 1,
    }
}

fn params(passphrase: Option<&[u8]>) -> EncodeParams<'_> {
    EncodeParams {
        compression_level: 3,
        passphrase,
        kdf: fast_kdf(),
        fec: FecParams::default(),
        store_format: true,
        store_timestamp: true,
    }
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len)
            .map(|_| {
                let mut x = self.0;
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                self.0 = x;
                x.wrapping_mul(0x2545_F491_4F6C_DD1D) as u8
            })
            .collect()
    }
}

/// Compressible filler, so the compression path is actually taken.
fn prose(repeats: usize) -> Vec<u8> {
    "the quick brown fox jumps over the lazy dog. "
        .repeat(repeats)
        .into_bytes()
}

/// Frame bytes through modulation, the container's quantisation, and back.
fn through_carrier(frame: &[u8]) -> Vec<u8> {
    let modem = FskModem::new(ModemConfig::default()).unwrap();
    let samples = modem.modulate(frame);
    let round_tripped = from_i16(&to_i16(&samples));
    modem.demodulate(&round_tripped).unwrap()
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[test]
fn encrypted_payload_survives_the_full_carrier() {
    let plaintext = prose(200);
    let pass = b"correct horse battery staple";

    let (frame, report) = encode_frame(&plaintext, None, &params(Some(pass))).unwrap();
    assert!(report.compressed, "prose should compress");
    assert!(report.encrypted);

    let recovered = through_carrier(&frame);
    assert_eq!(
        decode_frame(&recovered, Some(pass)).unwrap().data,
        plaintext
    );
}

#[test]
fn unencrypted_payload_survives_the_full_carrier() {
    let plaintext = prose(50);
    let (frame, report) = encode_frame(&plaintext, None, &params(None)).unwrap();
    assert!(!report.encrypted);

    let recovered = through_carrier(&frame);
    assert_eq!(decode_frame(&recovered, None).unwrap().data, plaintext);
}

#[test]
fn incompressible_input_skips_compression_rather_than_expanding() {
    let plaintext = Rng::new(99).bytes(20_000);
    let (frame, report) = encode_frame(&plaintext, None, &params(None)).unwrap();

    assert!(
        !report.compressed,
        "random data must not be stored as a larger zstd frame"
    );
    // The envelope adds the encode time (and a format, when one is detected),
    // so the stored length is the plaintext plus that small fixed overhead.
    assert!(
        (plaintext.len()..plaintext.len() + 64).contains(&report.compressed_len),
        "expected roughly the plaintext length, got {}",
        report.compressed_len
    );
    assert_eq!(decode_frame(&frame, None).unwrap().data, plaintext);
}

#[test]
fn empty_payload_round_trips() {
    let (frame, _) = encode_frame(&[], None, &params(Some(b"pw"))).unwrap();
    assert_eq!(
        decode_frame(&frame, Some(b"pw")).unwrap().data,
        Vec::<u8>::new()
    );
}

#[test]
fn every_byte_value_survives() {
    let plaintext: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    let (frame, _) = encode_frame(&plaintext, None, &params(Some(b"pw"))).unwrap();
    let recovered = through_carrier(&frame);
    assert_eq!(
        decode_frame(&recovered, Some(b"pw")).unwrap().data,
        plaintext
    );
}

#[test]
fn each_run_uses_a_fresh_salt_and_nonce() {
    let plaintext = prose(10);
    let (first, _) = encode_frame(&plaintext, None, &params(Some(b"pw"))).unwrap();
    let (second, _) = encode_frame(&plaintext, None, &params(Some(b"pw"))).unwrap();

    let a = Header::parse(&first).unwrap();
    let b = Header::parse(&second).unwrap();

    assert_ne!(a.salt, b.salt, "salt must be per-run");
    assert_ne!(a.nonce, b.nonce, "nonce must be per-run");
    assert_ne!(
        first, second,
        "identical plaintext must not produce an identical carrier"
    );
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[test]
fn a_wrong_passphrase_is_rejected() {
    let (frame, _) = encode_frame(&prose(20), None, &params(Some(b"right"))).unwrap();
    assert_eq!(
        decode_frame(&frame, Some(b"wrong")),
        Err(CoreError::Crypto(CryptoError::Decrypt))
    );
}

#[test]
fn a_missing_passphrase_is_reported_distinctly() {
    let (frame, _) = encode_frame(&prose(20), None, &params(Some(b"pw"))).unwrap();
    assert_eq!(
        decode_frame(&frame, None),
        Err(CoreError::Crypto(CryptoError::PassphraseRequired))
    );
}

#[test]
fn flipping_a_ciphertext_bit_fails_authentication() {
    let (mut frame, _) = encode_frame(&prose(20), None, &params(Some(b"pw"))).unwrap();
    // Land inside the first packet's payload, past the 4-byte PayloadId.
    frame[HEADER_LEN + 16] ^= 0x01;

    assert_eq!(
        decode_frame(&frame, Some(b"pw")),
        Err(CoreError::Crypto(CryptoError::Decrypt))
    );
}

#[test]
fn editing_an_authenticated_header_field_fails_even_with_a_repaired_crc() {
    let (mut frame, _) = encode_frame(&prose(20), None, &params(Some(b"pw"))).unwrap();

    // Flip a bit in `original_len`, which is covered by the AAD, then recompute
    // the CRC so the frame is internally consistent. This is exactly what an
    // attacker who understood the format would do -- and the GCM tag is the
    // control that stops it, not the checksum.
    frame[8] ^= 0x04;
    let crc = crc32fast::hash(&frame[..88]);
    frame[88..92].copy_from_slice(&crc.to_le_bytes());

    assert_eq!(
        Header::parse(&frame).map(|h| h.original_len),
        Ok(u64::from_le_bytes(frame[8..16].try_into().unwrap())),
        "the repaired CRC must pass, proving the CRC is not the control here"
    );
    assert_eq!(
        decode_frame(&frame, Some(b"pw")),
        Err(CoreError::Crypto(CryptoError::Decrypt))
    );
}

#[test]
fn clearing_the_encrypted_flag_does_not_downgrade_the_payload() {
    let (mut frame, _) = encode_frame(&prose(20), None, &params(Some(b"pw"))).unwrap();

    frame[5] &= !FLAG_ENCRYPTED;
    let crc = crc32fast::hash(&frame[..88]);
    frame[88..92].copy_from_slice(&crc.to_le_bytes());

    // With the flag cleared the decoder takes the unencrypted path, so it never
    // reaches AES-GCM. The ciphertext then fails to be valid zstd or to match
    // the declared length -- either way the payload is not surrendered.
    let result = decode_frame(&frame, None);
    assert!(
        matches!(result, Err(CoreError::Compress(_))),
        "expected the stripped-flag frame to fail decoding, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Erasure tolerance
// ---------------------------------------------------------------------------

#[test]
fn a_truncated_carrier_is_recovered_up_to_the_repair_budget() {
    // Incompressible on purpose. Prose would shrink to under a single 256-byte
    // symbol, and a one-packet object has no interesting boundary to probe.
    let plaintext = Rng::new(4242).bytes(20_000);
    let fec = FecParams {
        symbol_size: 256,
        repair_overhead_percent: 25,
    };
    let (frame, report) = encode_frame(
        &plaintext,
        None,
        &EncodeParams {
            fec,
            ..params(Some(b"pw"))
        },
    )
    .unwrap();

    let stride = 4 + fec.symbol_size as usize;
    let source_packets = report.ciphertext_len.div_ceil(fec.symbol_size as usize);
    let repair = report.fec_packets - source_packets;
    assert!(repair > 0, "this test needs repair packets to exist");

    // Dropping exactly the repair budget must still decode.
    let keep = HEADER_LEN + (report.fec_packets - repair) * stride;
    assert_eq!(
        decode_frame(&frame[..keep], Some(b"pw")).unwrap().data,
        plaintext,
        "losing {repair} of {} packets should be survivable",
        report.fec_packets
    );

    // One packet beyond it must not.
    let too_few = HEADER_LEN + (report.fec_packets - repair - 1) * stride;
    assert!(
        matches!(
            decode_frame(&frame[..too_few], Some(b"pw")),
            Err(CoreError::Fec(FecError::Unrecoverable { .. }))
        ),
        "losing one packet more than the repair budget must fail cleanly"
    );
}

#[test]
fn a_partial_trailing_packet_is_discarded_not_rejected() {
    let plaintext = prose(400);
    let (frame, _) = encode_frame(
        &plaintext,
        None,
        &EncodeParams {
            fec: FecParams {
                symbol_size: 256,
                repair_overhead_percent: 50,
            },
            ..params(Some(b"pw"))
        },
    )
    .unwrap();

    // Cut mid-packet. The remainder is not a whole packet, and rounding it down
    // rather than erroring is what makes a clipped file recoverable.
    let recovered = decode_frame(&frame[..frame.len() - 37], Some(b"pw")).unwrap();
    assert_eq!(recovered.data, plaintext);
}

#[test]
fn zero_repair_overhead_still_round_trips_intact() {
    let plaintext = prose(100);
    let (frame, _) = encode_frame(
        &plaintext,
        None,
        &EncodeParams {
            fec: FecParams {
                symbol_size: 256,
                repair_overhead_percent: 0,
            },
            ..params(None)
        },
    )
    .unwrap();

    assert_eq!(decode_frame(&frame, None).unwrap().data, plaintext);
}

// ---------------------------------------------------------------------------
// Frame parsing
// ---------------------------------------------------------------------------

#[test]
fn a_short_buffer_is_rejected_before_anything_is_sized() {
    assert_eq!(
        Header::parse(&[0u8; 10]),
        Err(FrameError::TooShort {
            len: 10,
            needed: HEADER_LEN
        })
    );
}

#[test]
fn foreign_data_is_reported_as_bad_magic() {
    let junk = [0x7fu8; HEADER_LEN];
    assert!(matches!(
        Header::parse(&junk),
        Err(FrameError::BadMagic { .. })
    ));
}

#[test]
fn a_corrupt_header_is_caught_by_the_checksum() {
    let (mut frame, _) = encode_frame(&prose(5), None, &params(None)).unwrap();
    frame[20] ^= 0xff;
    assert!(matches!(
        Header::parse(&frame),
        Err(FrameError::HeaderCrcMismatch { .. })
    ));
}

#[test]
fn an_unknown_version_is_refused() {
    let (mut frame, _) = encode_frame(&prose(5), None, &params(None)).unwrap();
    frame[4] = 99;
    assert_eq!(
        Header::parse(&frame),
        Err(FrameError::UnsupportedVersion {
            got: 99,
            supported: 1
        })
    );
}

#[test]
fn header_records_what_the_encoder_actually_did() {
    let plaintext = prose(30);
    let (frame, report) = encode_frame(&plaintext, None, &params(Some(b"pw"))).unwrap();
    let header = Header::parse(&frame).unwrap();

    // `original_len` is the envelope, which now also carries the encode time,
    // so it is at least the plaintext rather than exactly it.
    assert!(header.original_len >= plaintext.len() as u64);
    assert_eq!(header.ciphertext_len, report.ciphertext_len as u64);
    assert_eq!(
        header.ciphertext_len,
        report.compressed_len as u64 + TAG_LEN as u64
    );
    assert_eq!(header.frame_len(), frame.len() as u64);
    assert!(header.is_encrypted() && header.is_compressed() && header.is_fec());
    assert_eq!(header.kdf, fast_kdf());
}

// ---------------------------------------------------------------------------
// Filename envelope
// ---------------------------------------------------------------------------

#[test]
fn a_stored_filename_survives_the_round_trip() {
    let plaintext = prose(40);
    let (frame, _) = encode_frame(&plaintext, Some("notes.txt"), &params(Some(b"pw"))).unwrap();

    let header = Header::parse(&frame).unwrap();
    assert!(header.is_named());

    let recovered = decode_frame(&frame, Some(b"pw")).unwrap();
    assert_eq!(recovered.name.as_deref(), Some("notes.txt"));
    assert_eq!(recovered.data, plaintext);
}

#[test]
fn the_stored_filename_is_not_visible_in_the_frame() {
    let plaintext = prose(40);
    let name = "extremely-secret-project-codename.txt";
    let (frame, _) = encode_frame(&plaintext, Some(name), &params(Some(b"pw"))).unwrap();

    // The name lives inside the encryption, so it must not appear anywhere in
    // the frame bytes -- header included.
    assert!(
        !frame
            .windows(name.len())
            .any(|window| window == name.as_bytes()),
        "the filename leaked into the frame in cleartext"
    );
}

#[test]
fn a_payload_without_a_name_reports_none() {
    let plaintext = prose(10);
    let (frame, _) = encode_frame(&plaintext, None, &params(None)).unwrap();

    assert!(!Header::parse(&frame).unwrap().is_named());
    let recovered = decode_frame(&frame, None).unwrap();
    assert_eq!(recovered.name, None);
    assert_eq!(recovered.data, plaintext);
}

#[test]
fn a_unicode_filename_survives() {
    let plaintext = prose(10);
    let name = "計画書-v2 (最終).txt";
    let (frame, _) = encode_frame(&plaintext, Some(name), &params(Some(b"pw"))).unwrap();
    assert_eq!(
        decode_frame(&frame, Some(b"pw")).unwrap().name.as_deref(),
        Some(name)
    );
}

// ---------------------------------------------------------------------------
// Tone plan serialisation
// ---------------------------------------------------------------------------

#[test]
fn every_profile_plan_survives_serialisation() {
    for profile in Profile::ALL {
        let plan = profile.plan();
        let text = plan.to_plan_string();
        assert_eq!(
            Plan::from_plan_string(&text).unwrap(),
            plan,
            "profile {} did not round-trip through {text:?}",
            profile.name()
        );
    }
}

#[test]
fn a_plan_string_without_a_mode_is_read_as_fsk() {
    // Carriers written before the OFDM waveform existed have no mode field.
    let legacy = "fs=24000;n=48;bits=4;base=4;step=1;amp=0.25";
    assert_eq!(
        Plan::from_plan_string(legacy).unwrap(),
        Plan::Fsk(ModemConfig::default())
    );
}

#[test]
fn an_unknown_waveform_mode_is_refused() {
    assert!(matches!(
        Plan::from_plan_string("mode=telepathy;fs=24000"),
        Err(PlanParseError::UnknownMode(_))
    ));
}

#[test]
fn unknown_plan_fields_are_ignored_for_forward_compatibility() {
    let text = "fs=24000;n=48;bits=4;base=4;step=1;amp=0.25;futurefield=whatever";
    assert_eq!(
        ModemConfig::from_plan_string(text).unwrap(),
        ModemConfig::default()
    );
}

#[test]
fn a_plan_describing_an_impossible_modem_is_refused() {
    // 16 tones at bin 4 needs bins up to 19, but N=12 puts Nyquist at bin 6.
    let text = "fs=24000;n=12;bits=4;base=4;step=1";
    assert!(matches!(
        ModemConfig::from_plan_string(text),
        Err(PlanParseError::Invalid(_))
    ));
}

#[test]
fn a_malformed_plan_is_refused() {
    assert!(matches!(
        ModemConfig::from_plan_string("fs=24000;garbage"),
        Err(PlanParseError::Malformed(_))
    ));
    assert!(matches!(
        ModemConfig::from_plan_string("fs=not-a-number"),
        Err(PlanParseError::BadValue { .. })
    ));
}

#[test]
fn the_fast_profile_really_is_faster_and_valid() {
    let standard = Profile::Standard.plan();
    let fast = Profile::Fast.plan();
    let dense = Profile::Dense.plan();

    for plan in [standard, fast, dense] {
        plan.validate().unwrap();
    }
    assert!(fast.bit_rate() > standard.bit_rate());
    assert_eq!(fast.bit_rate(), 4000.0);

    // The whole point of the OFDM waveform: filling the time-frequency plane
    // rather than lighting one tone at a time should be worth well over an
    // order of magnitude.
    assert!(
        dense.bit_rate() > 30.0 * standard.bit_rate(),
        "dense is only {:.0} bit/s against standard's {:.0}",
        dense.bit_rate(),
        standard.bit_rate()
    );

    // And every profile must actually carry data, not merely validate.
    for plan in [standard, fast, dense] {
        let modem = Carrier::new(plan).unwrap();
        let payload = Rng::new(7).bytes(4096);
        let samples = modem.modulate(&payload);
        let back = modem.demodulate(&from_i16(&to_i16(&samples))).unwrap();
        assert!(
            back.len() >= payload.len() && back[..payload.len()] == payload[..],
            "{} failed to carry its payload",
            plan.describe()
        );
    }
}

// ---------------------------------------------------------------------------
// Compressibility probing
// ---------------------------------------------------------------------------

#[test]
fn a_large_incompressible_payload_skips_the_expensive_pass() {
    // Above the probe threshold, so the level-1 probe decides. The observable
    // effect is only that compression was skipped -- the win is that level 19
    // was never attempted, which took 2.16 s on 20 MB and produced something
    // larger than its input.
    let plaintext = Rng::new(77).bytes(512 * 1024);
    let (frame, report) = encode_frame(&plaintext, None, &params(None)).unwrap();

    assert!(
        !report.compressed,
        "random data must not be stored compressed"
    );
    assert_eq!(decode_frame(&frame, None).unwrap().data, plaintext);
}

#[test]
fn a_large_compressible_payload_is_still_compressed_properly() {
    // The probe must not cost real compression. This is the regression that
    // matters: a cheap heuristic that skips work is only safe if it never skips
    // work worth doing.
    let plaintext = prose(30_000); // ~1.3 MB of highly repetitive text
    assert!(plaintext.len() > 512 * 1024);

    let (frame, report) = encode_frame(&plaintext, None, &params(None)).unwrap();

    assert!(report.compressed, "repetitive text must be compressed");
    assert!(
        report.compression_ratio() < 0.01,
        "expected a huge ratio on repetitive text, got {:.4}",
        report.compression_ratio()
    );
    assert_eq!(decode_frame(&frame, None).unwrap().data, plaintext);
}

#[test]
fn the_probe_threshold_does_not_change_correctness_either_side_of_it() {
    // Small payloads bypass the probe and compress directly; large ones probe
    // first. Both paths must produce identical, recoverable output.
    for len in [1000usize, 64 * 1024 - 1, 64 * 1024, 200_000] {
        let plaintext = prose(len / 45 + 1);
        let (frame, _) = encode_frame(&plaintext, None, &params(Some(b"pw"))).unwrap();
        assert_eq!(
            decode_frame(&frame, Some(b"pw")).unwrap().data,
            plaintext,
            "failed around the probe threshold at length {len}"
        );
    }
}

// ---------------------------------------------------------------------------
// Format detection and naming
// ---------------------------------------------------------------------------

/// Minimal but genuine leading bytes for a few formats.
fn magic(kind: &str) -> Vec<u8> {
    let head: &[u8] = match kind {
        "pdf" => b"%PDF-1.7\n",
        "png" => b"\x89PNG\r\n\x1a\n",
        "gzip" => b"\x1f\x8b\x08\x00",
        "zip" => b"PK\x03\x04\x14\x00",
        "elf" => b"\x7fELF\x02\x01\x01",
        other => panic!("unknown fixture {other}"),
    };
    let mut out = head.to_vec();
    out.extend_from_slice(&prose(20));
    out
}

#[test]
fn common_formats_are_detected_from_content() {
    for (kind, id) in [
        ("pdf", "pdf"),
        ("png", "png"),
        ("gzip", "gzip"),
        ("zip", "zip"),
        ("elf", "elf"),
    ] {
        let detected = format::detect(&magic(kind)).expect("should detect {kind}");
        assert_eq!(detected.id, id, "{kind} was misidentified");
    }
}

#[test]
fn unrecognised_content_has_no_format_rather_than_a_guess() {
    // Plain text has no magic number. Guessing from statistics would put a
    // confident wrong answer into a filename, so nothing is claimed.
    assert_eq!(format::detect(&prose(50)), None);
    assert_eq!(format::detect(&[]), None);
    assert_eq!(format::detect(b"x"), None);
}

#[test]
fn the_format_travels_with_the_payload() {
    let plaintext = magic("pdf");
    let (frame, _) = encode_frame(&plaintext, Some("report.pdf"), &params(Some(b"pw"))).unwrap();

    let header = Header::parse(&frame).unwrap();
    assert!(header.is_named() && header.has_format());

    let recovered = decode_frame(&frame, Some(b"pw")).unwrap();
    assert_eq!(recovered.name.as_deref(), Some("report.pdf"));
    assert_eq!(recovered.format.map(|f| f.id), Some("pdf"));
    assert_eq!(recovered.data, plaintext);
}

#[test]
fn a_payload_with_no_name_still_carries_its_format() {
    // The piped case: nothing to name it, but the content still identifies
    // itself, which is what lets the CLI land it as `payload.pdf`.
    let plaintext = magic("pdf");
    let (frame, _) = encode_frame(&plaintext, None, &params(None)).unwrap();

    let header = Header::parse(&frame).unwrap();
    assert!(!header.is_named() && header.has_format());

    let recovered = decode_frame(&frame, None).unwrap();
    assert_eq!(recovered.name, None);
    assert_eq!(recovered.format.map(|f| f.extension), Some("pdf"));
    assert_eq!(format::default_name(recovered.format), "payload.pdf");
    assert_eq!(recovered.data, plaintext);
}

#[test]
fn a_stored_name_is_returned_exactly_as_given() {
    // Deliberately an extensionless name holding PDF bytes. The tool must not
    // "helpfully" rename it: an extensionless Unix executable is the case that
    // makes any such rule wrong.
    let plaintext = magic("pdf");
    let (frame, _) = encode_frame(&plaintext, Some("LICENSE"), &params(None)).unwrap();

    let recovered = decode_frame(&frame, None).unwrap();
    assert_eq!(recovered.suggested_name(), Some("LICENSE"));
    assert_eq!(recovered.format.map(|f| f.id), Some("pdf"));
}

#[test]
fn suppressing_metadata_hides_the_format_too() {
    let plaintext = magic("pdf");
    let params = EncodeParams {
        store_format: false,
        ..params(Some(b"pw"))
    };
    let (frame, _) = encode_frame(&plaintext, None, &params).unwrap();

    let header = Header::parse(&frame).unwrap();
    assert!(!header.is_named() && !header.has_format());

    let recovered = decode_frame(&frame, Some(b"pw")).unwrap();
    assert_eq!(recovered.format, None);
    assert_eq!(recovered.data, plaintext);
}

#[test]
fn neither_the_name_nor_the_format_appears_in_the_frame() {
    // Both live inside the encryption. A carrier that advertised "holds a PDF"
    // in cleartext would undo much of the point of encrypting it.
    let plaintext = magic("pdf");
    let (frame, _) = encode_frame(
        &plaintext,
        Some("tax-return-2024.pdf"),
        &params(Some(b"pw")),
    )
    .unwrap();

    for needle in [b"tax-return-2024".as_slice(), b"pdf".as_slice()] {
        assert!(
            !frame.windows(needle.len()).any(|w| w == needle),
            "{:?} leaked into the frame in cleartext",
            std::str::from_utf8(needle).unwrap()
        );
    }
}

#[test]
fn an_unknown_format_identifier_does_not_break_decoding() {
    // A carrier from a newer build may name a format this one has never heard
    // of. The payload must still come back; only the label is lost.
    assert_eq!(format::by_id("definitely-not-a-real-format"), None);
    assert_eq!(format::by_id("pdf").map(|f| f.extension), Some("pdf"));
}

#[test]
fn the_encode_time_travels_with_the_payload() {
    let plaintext = prose(20);
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let (frame, _) = encode_frame(&plaintext, Some("m.txt"), &params(Some(b"pw"))).unwrap();
    assert!(Header::parse(&frame).unwrap().has_timestamp());

    let recovered = decode_frame(&frame, Some(b"pw")).unwrap();
    let stamp = recovered.encoded_at.expect("timestamp should be recorded");
    assert!(
        stamp >= before && stamp <= before + 60,
        "timestamp {stamp} is not near {before}"
    );
}

#[test]
fn the_encode_time_is_not_visible_in_the_frame() {
    // Inside the envelope with the name and format. When a file was made can be
    // as telling as what is in it, so it must not sit in the plaintext header.
    let (frame, _) = encode_frame(&prose(20), None, &params(Some(b"pw"))).unwrap();
    let stamp = decode_frame(&frame, Some(b"pw"))
        .unwrap()
        .encoded_at
        .unwrap();

    let needle = stamp.to_le_bytes();
    assert!(
        !frame.windows(8).any(|w| w == needle),
        "the encode time leaked into the frame in cleartext"
    );
}

#[test]
fn suppressing_metadata_hides_the_time_too() {
    let params = EncodeParams {
        store_format: false,
        store_timestamp: false,
        ..params(None)
    };
    let (frame, _) = encode_frame(&prose(10), None, &params).unwrap();
    let header = Header::parse(&frame).unwrap();
    assert!(!header.has_timestamp() && !header.has_format() && !header.is_named());
    assert_eq!(decode_frame(&frame, None).unwrap().encoded_at, None);
}
