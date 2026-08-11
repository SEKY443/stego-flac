//! Passphrase acquisition.
//!
//! # There is deliberately no `--passphrase` flag
//!
//! A secret passed as an argument is visible in `ps` output to every other user
//! on the machine, is written to shell history, and is inherited by child
//! processes. Those are not theoretical: `ps aux | grep` is the first thing an
//! attacker with a shell runs. The three supported sources are, in order of
//! preference:
//!
//! 1. an interactive prompt (never touches disk or the process table),
//! 2. `--passphrase-file` (readable only if the file's permissions allow it),
//! 3. `AUDIO_MODEM_PASSPHRASE` (convenient for scripts, still visible in
//!    `/proc/<pid>/environ` on Linux — the least safe of the three).

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use zeroize::Zeroizing;

/// Environment variable consulted when no file is given and stdin is not a TTY.
pub const ENV_VAR: &str = "AUDIO_MODEM_PASSPHRASE";

/// A passphrase that scrubs itself on drop.
pub type Passphrase = Zeroizing<Vec<u8>>;

/// Obtain a passphrase from a file, the environment, or an interactive prompt.
///
/// `confirm` re-prompts and compares, which matters for `encode`: a typo in a
/// write-side passphrase produces a carrier nobody can ever open, and there is
/// no recovery path.
pub fn acquire(file: Option<&Path>, confirm: bool) -> Result<Passphrase> {
    if let Some(path) = file {
        let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let trimmed = strip_trailing_newline(&raw);
        if trimmed.is_empty() {
            bail!("{} is empty", path.display());
        }
        return Ok(Zeroizing::new(trimmed.to_vec()));
    }

    if let Ok(value) = std::env::var(ENV_VAR) {
        if !value.is_empty() {
            return Ok(Zeroizing::new(value.into_bytes()));
        }
    }

    let first = Zeroizing::new(
        rpassword::prompt_password("Passphrase: ")
            .context("reading passphrase from the terminal")?,
    );
    if first.is_empty() {
        bail!("passphrase must not be empty");
    }

    if confirm {
        let again = Zeroizing::new(
            rpassword::prompt_password("Confirm passphrase: ")
                .context("reading passphrase confirmation")?,
        );
        if *first != *again {
            bail!("passphrases do not match");
        }
    }

    Ok(Zeroizing::new(first.as_bytes().to_vec()))
}

/// Strip a single trailing `\n` or `\r\n`, so a passphrase file written by a
/// text editor behaves as the user expects.
fn strip_trailing_newline(raw: &[u8]) -> &[u8] {
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    raw.strip_suffix(b"\r").unwrap_or(raw)
}
