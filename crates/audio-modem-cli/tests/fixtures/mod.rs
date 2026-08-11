//! Generators for genuinely valid files in common formats.
//!
//! Round-tripping random bytes proves the modem works; it does not prove the
//! *tool* works on the things people actually carry. These build real files —
//! a PDF a viewer will open, a PNG an image library will decode, archives the
//! system `unzip` and `gzip` accept — so the format matrix exercises realistic
//! entropy, sizes and filenames rather than a uniform random blob.
//!
//! Everything here is generated in-process so the suite stays hermetic. The
//! `formats` test separately validates the output against the operating
//! system's own tools when they are present, which is what makes "valid" a
//! claim rather than an assertion.

#![allow(dead_code)]

/// Adler-32, as required by the zlib wrapper inside PNG.
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// DEFLATE using stored (uncompressed) blocks.
///
/// A stored block is legal DEFLATE — type 00, length, one's-complement length,
/// then raw bytes — so this produces a stream any inflater accepts without
/// needing a compressor. Blocks cap at 65535 bytes.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
        return out;
    }
    for (index, chunk) in data.chunks(65_535).enumerate() {
        let last = (index + 1) * 65_535 >= data.len();
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out
}

/// zlib stream: 2-byte header, stored DEFLATE, Adler-32 trailer.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend_from_slice(&deflate_stored(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn png_chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(kind);
    hasher.update(body);
    out.extend_from_slice(&hasher.finalize().to_be_bytes());
    out
}

/// A real 8-bit RGB PNG containing a smooth gradient.
///
/// Stored DEFLATE means the pixel data is not actually compressed, so this
/// behaves like a *lightly* compressed image: the payload still has plenty of
/// structure for zstd to find, unlike a photographic JPEG.
pub fn png(width: u32, height: u32) -> Vec<u8> {
    let mut raw = Vec::with_capacity((height * (1 + width * 3)) as usize);
    for y in 0..height {
        raw.push(0); // filter: none
        for x in 0..width {
            raw.push((x * 255 / width.max(1)) as u8);
            raw.push((y * 255 / height.max(1)) as u8);
            raw.push(((x ^ y) & 0xff) as u8);
        }
    }

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolour

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    out.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    out.extend_from_slice(&png_chunk(b"IDAT", &zlib_stored(&raw)));
    out.extend_from_slice(&png_chunk(b"IEND", &[]));
    out
}

/// A real gzip member wrapping `data`, genuinely DEFLATE-compressed.
///
/// Real compression matters here rather than a stored block: the point of
/// having an archive in the corpus is to exercise a payload that zstd cannot
/// shrink further, and a stored-DEFLATE archive would compress like the text
/// inside it and quietly test nothing.
pub fn gzip(data: &[u8], name: &str) -> Vec<u8> {
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).expect("deflate");
    let deflated = encoder.finish().expect("deflate finish");

    let mut out = vec![0x1f, 0x8b, 0x08, 0x08];
    out.extend_from_slice(&0u32.to_le_bytes()); // mtime
    out.extend_from_slice(&[0x02, 0x03]); // XFL = best, OS = unix
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(&deflated);
    out.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// A real ZIP archive with DEFLATE-compressed entries.
pub fn zip(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut out = Vec::new();
    let mut central = Vec::new();

    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32fast::hash(data);
        let size = data.len() as u32;

        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(data).expect("deflate");
        let deflated = encoder.finish().expect("deflate finish");
        let packed = deflated.len() as u32;

        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&[20, 0, 0, 0, 8, 0, 0, 0, 0, 0]); // ver, flags, deflate, time, date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&packed.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&deflated);

        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 8, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&packed.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }

    let central_offset = out.len() as u32;
    let count = entries.len() as u16;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// A real, openable PDF with `pages` pages of text.
///
/// Written by hand rather than pulled from the system, so the suite does not
/// depend on a file that may not exist. The cross-reference table carries true
/// byte offsets, which is the part that decides whether a viewer will open it
/// at all — the `formats` test checks that against a real PDF parser.
pub fn pdf(pages: usize) -> Vec<u8> {
    let pages = pages.max(1);
    let mut objects: Vec<Vec<u8>> = Vec::new();

    // 1: catalogue, 2: page tree, 3: font. Pages and their content streams
    // follow in pairs, so page i is object 4 + 2i and its stream 5 + 2i.
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();

    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objects.push(
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            pages
        )
        .into_bytes(),
    );
    objects.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());

    for page in 0..pages {
        let stream_id = 5 + 2 * page;
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {stream_id} 0 R >>"
            )
            .into_bytes(),
        );

        let mut text = String::from("BT /F1 11 Tf 54 738 Td 14 TL\n");
        for line in 0..48 {
            text += &format!(
                "(Page {} line {:02}: the quick brown fox jumps over the lazy dog.) Tj T*\n",
                page + 1,
                line + 1
            );
        }
        text += "ET\n";

        let mut stream = format!("<< /Length {} >>\nstream\n", text.len()).into_bytes();
        stream.extend_from_slice(text.as_bytes());
        stream.extend_from_slice(b"endstream");
        objects.push(stream);
    }

    let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }

    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            objects.len() + 1,
            xref_at
        )
        .as_bytes(),
    );
    out
}

/// Newline-delimited JSON records; highly compressible, like real log output.
pub fn json(records: usize) -> Vec<u8> {
    let mut out = String::from("[\n");
    for i in 0..records {
        out += &format!(
            "  {{\"id\": {i}, \"name\": \"item-{i}\", \"active\": {}, \
             \"tags\": [\"alpha\", \"beta\"], \"score\": {}.{}}}{}\n",
            i % 2 == 0,
            i % 100,
            i % 10,
            if i + 1 == records { "" } else { "," }
        );
    }
    out += "]\n";
    out.into_bytes()
}

pub fn csv(rows: usize) -> Vec<u8> {
    let mut out = String::from("id,name,department,salary,started\n");
    let departments = ["engineering", "design", "support", "research"];
    for i in 0..rows {
        out += &format!(
            "{i},Person {i},{},{},2020-{:02}-{:02}\n",
            departments[i % departments.len()],
            50_000 + (i % 50) * 1_000,
            1 + i % 12,
            1 + i % 28
        );
    }
    out.into_bytes()
}

pub fn markdown(sections: usize) -> Vec<u8> {
    let mut out = String::from("# Report\n\n");
    for i in 0..sections {
        out += &format!(
            "## Section {i}\n\nSome prose about section {i}. It repeats a good \
             deal, as documents tend to.\n\n- point one\n- point two\n\n```rust\n\
             fn section_{i}() -> usize {{ {i} }}\n```\n\n"
        );
    }
    out.into_bytes()
}

pub fn source_code(functions: usize) -> Vec<u8> {
    let mut out = String::from("//! Generated module.\n\nuse std::collections::HashMap;\n\n");
    for i in 0..functions {
        out += &format!(
            "/// Does the {i}th thing.\npub fn operation_{i}(input: &str) -> HashMap<String, usize> {{\n    \
             let mut out = HashMap::new();\n    for (index, word) in input.split_whitespace().enumerate() {{\n        \
             out.insert(word.to_string(), index + {i});\n    }}\n    out\n}}\n\n"
        );
    }
    out.into_bytes()
}

/// Deterministic pseudo-random bytes, standing in for already-compressed media.
pub fn incompressible(len: usize, seed: u64) -> Vec<u8> {
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

// ---------------------------------------------------------------------------
// CLI harness
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const BIN: &str = env!("CARGO_BIN_EXE_stego-flac");
pub const PASSPHRASE: &str = "correct horse battery staple";

/// A scratch directory that removes itself.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let unique = format!(
            "audio-modem-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("creating the scratch directory");
        Self(path)
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("AUDIO_MODEM_PASSPHRASE", PASSPHRASE)
        .output()
        .expect("running stego-flac")
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn encode(input: &Path, output: &Path, extra: &[&str]) -> Output {
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

pub fn decode(input: &Path, output: &Path, extra: &[&str]) -> Output {
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

/// Run the binary with `input` on stdin.
pub fn run_with_stdin(args: &[&str], input: &[u8]) -> Output {
    use std::io::Write;
    let mut child = Command::new(BIN)
        .args(args)
        .env("AUDIO_MODEM_PASSPHRASE", PASSPHRASE)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
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

/// Whether an external tool exists, so validation can be skipped rather than
/// failing on a machine that lacks it.
pub fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
