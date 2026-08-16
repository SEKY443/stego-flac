//! Subcommand implementations.

pub mod decode;
pub mod encode;
pub mod info;
pub mod plan;

use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Result};

/// Transient stage messages on stderr.
///
/// Encoding a large payload spends most of its time inside FLAC compression
/// with no output at all, which reads as a hang. This prints what is happening
/// and erases itself when done, so the final report is the only thing left on
/// screen.
///
/// Two independent modes, chosen once at construction:
///
/// * **Terminal**: a self-erasing carriage-return line, active only when
///   stderr is a terminal. Redirected output stays clean, which matters
///   because the rewriting would otherwise litter logs and break tests that
///   match on stderr.
/// * **JSON**: one `{"stage": "..."}` line per stage, regardless of whether
///   stderr is a terminal. This is for a caller that launched the process
///   itself — a GUI wrapper, a script — and therefore never has a TTY on the
///   other end of the pipe, but still wants to know a multi-minute encode is
///   moving rather than hung. Selected by `--json`, independent of whatever
///   `--json` does to the final report.
pub enum Stage {
    Terminal(RefCell<usize>),
    Json,
    Off,
}

impl Stage {
    pub fn new(json: bool) -> Self {
        if json {
            Stage::Json
        } else if std::io::stderr().is_terminal() {
            Stage::Terminal(RefCell::new(0))
        } else {
            Stage::Off
        }
    }

    /// Announce the next stage, replacing whatever the last one printed.
    pub fn begin(&self, what: &str) {
        let mut stderr = std::io::stderr();
        match self {
            Stage::Off => {}
            Stage::Json => {
                let _ = writeln!(stderr, "{}", serde_json::json!({ "stage": what }));
                let _ = stderr.flush();
            }
            Stage::Terminal(width) => {
                let text = format!("  {what}...");
                let _ = write!(stderr, "\r{text:w$}", w = *width.borrow());
                let _ = stderr.flush();
                *width.borrow_mut() = text.len();
            }
        }
    }

    /// Erase the message. A no-op outside the terminal mode: JSON stages are
    /// a log, not a status line, so there is nothing to take back.
    pub fn done(&self) {
        if let Stage::Terminal(width) = self {
            let mut stderr = std::io::stderr();
            let _ = write!(stderr, "\r{:w$}\r", "", w = *width.borrow());
            let _ = stderr.flush();
        }
    }
}

/// Read the tone plan recorded in a carrier's FLAC metadata.
///
/// Returns `None` when the tags are absent, unparseable, or when the user gave
/// explicit tone-plan flags — in which case their choice takes precedence and
/// reading the file's opinion would only muddy the resolution order.
///
/// A malformed plan is reported and ignored rather than raised: the carrier is
/// still perfectly decodable if the reader supplies the plan by hand, so
/// refusing to open the file would be a worse outcome than losing the shortcut.
pub fn plan_from_tags(raw: &[u8], user_was_explicit: bool) -> Option<audio_modem_core::Plan> {
    if user_was_explicit {
        return None;
    }

    let tags = crate::flac_tags::read_tags(raw).ok()?;
    let recorded = tags.get(crate::flac_tags::PLAN_TAG)?;

    match audio_modem_core::Plan::from_plan_string(recorded) {
        Ok(config) => Some(config),
        Err(error) => {
            eprintln!("note: ignoring unreadable plan in metadata ({error})");
            None
        }
    }
}

/// Refuse to clobber an existing file unless `force` was given.
pub fn guard_output(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }
    Ok(())
}

/// Format a byte count for humans.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Format a duration in seconds for humans.
pub fn human_duration(seconds: f64) -> String {
    if seconds < 60.0 {
        return format!("{seconds:.1} s");
    }
    let total = seconds.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h} h {m:02} m {s:02} s")
    } else {
        format!("{m} m {s:02} s")
    }
}
