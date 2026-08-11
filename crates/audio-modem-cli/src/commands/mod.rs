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
/// Only active when stderr is a terminal. Redirected output stays clean, which
/// matters because the carriage-return rewriting would otherwise litter logs
/// and break tests that match on stderr.
pub struct Stage {
    enabled: bool,
    width: RefCell<usize>,
}

impl Stage {
    pub fn new() -> Self {
        Self {
            enabled: std::io::stderr().is_terminal(),
            width: RefCell::new(0),
        }
    }

    /// Replace the current message.
    pub fn begin(&self, what: &str) {
        if !self.enabled {
            return;
        }
        let text = format!("  {what}...");
        let mut stderr = std::io::stderr();
        let _ = write!(stderr, "\r{text:width$}", width = *self.width.borrow());
        let _ = stderr.flush();
        *self.width.borrow_mut() = text.len();
    }

    /// Erase the message.
    pub fn done(&self) {
        if !self.enabled {
            return;
        }
        let mut stderr = std::io::stderr();
        let _ = write!(stderr, "\r{:width$}\r", "", width = *self.width.borrow());
        let _ = stderr.flush();
    }
}

impl Default for Stage {
    fn default() -> Self {
        Self::new()
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
