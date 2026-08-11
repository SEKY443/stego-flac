//! Content-based file format detection.
//!
//! The payload's format is derived from its bytes rather than from its
//! filename, because the filename is frequently absent — a piped payload has
//! none at all — and occasionally wrong. Detection lets `encode` give a
//! nameless stream a sensible name, and lets `decode` report what it handed
//! back.
//!
//! # Magic numbers only, deliberately
//!
//! Every rule here is a fixed byte signature at a fixed offset. There is no
//! statistical sniffing, no "looks like UTF-8 so call it text" — those
//! heuristics are wrong often enough that a confident wrong answer would be
//! worse than no answer, since the result ends up in the recovered filename.
//! Unrecognised input simply has no format, and the payload keeps whatever name
//! it came with.
//!
//! The detected format is stored *inside* the encrypted envelope, alongside the
//! name and for the same reason: "this carrier holds a PDF" is exactly the kind
//! of thing the encryption is meant to hide.

/// A recognised file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFormat {
    /// Short stable identifier, stored in the payload envelope.
    pub id: &'static str,
    /// Conventional extension, without the dot.
    pub extension: &'static str,
    /// Human-readable name, for reporting.
    pub description: &'static str,
}

/// A signature: bytes that must appear at a byte offset.
struct Signature {
    offset: usize,
    magic: &'static [u8],
    format: FileFormat,
}

macro_rules! signature {
    ($offset:expr, $magic:expr, $id:expr, $ext:expr, $desc:expr) => {
        Signature {
            offset: $offset,
            magic: $magic,
            format: FileFormat {
                id: $id,
                extension: $ext,
                description: $desc,
            },
        }
    };
}

/// Ordered most-specific first: a WebP is also a RIFF, and a JAR is also a ZIP,
/// so the narrower rule has to be tried before the broader one.
static SIGNATURES: &[Signature] = &[
    // --- documents ---
    signature!(0, b"%PDF-", "pdf", "pdf", "PDF document"),
    signature!(0, b"{\\rtf", "rtf", "rtf", "Rich Text Format"),
    // --- images ---
    signature!(0, b"\x89PNG\r\n\x1a\n", "png", "png", "PNG image"),
    signature!(0, b"\xff\xd8\xff", "jpeg", "jpg", "JPEG image"),
    signature!(0, b"GIF89a", "gif", "gif", "GIF image"),
    signature!(0, b"GIF87a", "gif", "gif", "GIF image"),
    signature!(0, b"BM", "bmp", "bmp", "BMP image"),
    signature!(0, b"II*\x00", "tiff", "tiff", "TIFF image"),
    signature!(0, b"MM\x00*", "tiff", "tiff", "TIFF image"),
    signature!(0, b"\x00\x00\x01\x00", "ico", "ico", "Windows icon"),
    // --- audio and video (RIFF/ftyp forms must precede their containers) ---
    signature!(8, b"WEBP", "webp", "webp", "WebP image"),
    signature!(8, b"WAVE", "wav", "wav", "WAV audio"),
    signature!(8, b"AVI ", "avi", "avi", "AVI video"),
    signature!(4, b"ftyp", "mp4", "mp4", "MP4/QuickTime media"),
    signature!(0, b"fLaC", "flac", "flac", "FLAC audio"),
    signature!(0, b"OggS", "ogg", "ogg", "Ogg container"),
    signature!(0, b"ID3", "mp3", "mp3", "MP3 audio"),
    signature!(0, b"\xff\xfb", "mp3", "mp3", "MP3 audio"),
    signature!(0, b"\x1a\x45\xdf\xa3", "matroska", "mkv", "Matroska/WebM"),
    // --- archives and compression ---
    signature!(0, b"\x1f\x8b", "gzip", "gz", "gzip archive"),
    signature!(0, b"BZh", "bzip2", "bz2", "bzip2 archive"),
    signature!(0, b"\xfd7zXZ\x00", "xz", "xz", "xz archive"),
    signature!(0, b"\x28\xb5\x2f\xfd", "zstd", "zst", "Zstandard archive"),
    signature!(0, b"7z\xbc\xaf\x27\x1c", "7z", "7z", "7-Zip archive"),
    signature!(0, b"Rar!\x1a\x07", "rar", "rar", "RAR archive"),
    signature!(257, b"ustar", "tar", "tar", "tar archive"),
    signature!(0, b"PK\x03\x04", "zip", "zip", "ZIP archive"),
    // --- executables and databases ---
    signature!(0, b"\x7fELF", "elf", "elf", "ELF executable"),
    signature!(
        0,
        b"\xcf\xfa\xed\xfe",
        "macho",
        "macho",
        "Mach-O executable"
    ),
    signature!(
        0,
        b"\xca\xfe\xba\xbe",
        "macho-fat",
        "macho",
        "Mach-O universal binary"
    ),
    signature!(0, b"MZ", "pe", "exe", "Windows executable"),
    signature!(0, b"\x00asm", "wasm", "wasm", "WebAssembly module"),
    signature!(
        0,
        b"SQLite format 3\x00",
        "sqlite",
        "sqlite",
        "SQLite database"
    ),
    // --- markup ---
    signature!(0, b"<?xml", "xml", "xml", "XML document"),
    signature!(0, b"<!DOCTYPE html", "html", "html", "HTML document"),
    signature!(0, b"<html", "html", "html", "HTML document"),
];

/// Identify `data` by its leading bytes, or `None` if nothing matches.
pub fn detect(data: &[u8]) -> Option<FileFormat> {
    SIGNATURES
        .iter()
        .find(|signature| {
            data.len() >= signature.offset + signature.magic.len()
                && &data[signature.offset..signature.offset + signature.magic.len()]
                    == signature.magic
        })
        .map(|signature| signature.format)
}

/// Look a format up by the identifier stored in a payload envelope.
///
/// Unknown identifiers return `None` rather than erroring, so a carrier written
/// by a newer version that knows more formats still decodes here — it simply
/// cannot name the format it found.
pub fn by_id(id: &str) -> Option<FileFormat> {
    SIGNATURES
        .iter()
        .map(|signature| signature.format)
        .find(|format| format.id == id)
}

/// A default filename for a payload that arrived without one.
///
/// # Why a stored name is never "corrected"
///
/// It is tempting to also repair a stored name that lacks an extension —
/// `download` becoming `download.pdf`. That was implemented and then removed,
/// because the rule has no safe form. Unix executables are conventionally
/// extensionless, so an ELF or Mach-O binary named `myprogram` would be handed
/// back as `myprogram.elf`; and a PDF a user deliberately named `LICENSE` would
/// come back as something they never chose.
///
/// A tool that returns exactly what it was given is easier to trust than one
/// that is usually right. The detected format is reported in the decode summary
/// instead, which conveys the same information without altering anything.
pub fn default_name(format: Option<FileFormat>) -> String {
    match format {
        Some(format) => format!("payload.{}", format.extension),
        None => "payload".to_string(),
    }
}
