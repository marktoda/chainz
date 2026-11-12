//! Terminal prompts and stdin secrets. Interrupting a prompt (Ctrl-C, Esc)
//! is [`ui::cancelled`], which exits 130.

use crate::ui;
use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, FuzzySelect, MultiSelect};
use std::io::IsTerminal;
use zeroize::Zeroizing;

/// Whether pickers and prompts may be shown: stdin and stderr are both
/// terminals. Scripts never block on a hidden prompt.
pub fn is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Yes or no.
pub fn confirm(message: &str, default: bool) -> Result<bool> {
    terminal_prompt(
        Confirm::new()
            .with_prompt(message)
            .default(default)
            .interact(),
    )
}

/// Go ahead with something destructive or batch-editing: `--yes` says so
/// up front; otherwise ask on a TTY, and off a TTY fail with `hint` (what
/// to pass instead). Declining is [`ui::cancelled`].
pub fn confirm_or_yes(yes: bool, message: &str, hint: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !is_tty() {
        bail!("{hint}");
    }
    if confirm(message, false)? {
        Ok(())
    } else {
        Err(ui::cancelled())
    }
}

/// Pick one of `items`.
pub fn select(message: &str, items: &[String], default: usize) -> Result<usize> {
    terminal_prompt(
        FuzzySelect::new()
            .with_prompt(format!("{message} (ESC to exit)"))
            .items(items)
            .default(default)
            .interact_opt(),
    )?
    .ok_or_else(ui::cancelled)
}

/// Pick any of `items`.
pub fn multi_select(message: &str, items: &[String], defaults: &[bool]) -> Result<Vec<usize>> {
    terminal_prompt(
        MultiSelect::new()
            .with_prompt(message)
            .items(items)
            .defaults(defaults)
            .interact_opt(),
    )?
    .ok_or_else(ui::cancelled)
}

/// How a value piped on stdin is normalized before use.
#[derive(Clone, Copy, Debug)]
pub enum StdinTrim {
    /// Trim surrounding whitespace (keys, API tokens).
    Whitespace,
    /// Strip only the trailing newline(s) `echo` adds, preserving any other
    /// intentional whitespace (variable values).
    TrailingNewlines,
}

/// Read a secret from stdin for non-interactive use. The raw buffer is
/// zeroized; an empty result is an error naming `label`.
pub fn read_stdin_secret(label: &str, trim: StdinTrim) -> Result<Zeroizing<String>> {
    use std::io::Read;
    let mut raw = Zeroizing::new(String::new());
    std::io::stdin()
        .read_to_string(&mut raw)
        .with_context(|| format!("Failed to read {label} from stdin"))?;
    normalize_stdin(&raw, label, trim)
}

fn normalize_stdin(raw: &str, label: &str, trim: StdinTrim) -> Result<Zeroizing<String>> {
    let normalized = match trim {
        StdinTrim::Whitespace => raw.trim(),
        StdinTrim::TrailingNewlines => raw.trim_end_matches(['\r', '\n']),
    };
    if normalized.is_empty() {
        bail!("{label} from stdin was empty");
    }
    Ok(Zeroizing::new(normalized.to_string()))
}

/// A secret given as a command-line argument lands in shell history and is
/// visible to other local users in `ps`: say so.
pub fn warn_secret_argument(what: &str, instead: &str) {
    eprintln!(
        "{}",
        ui::warn(&format!(
            "{what} given as an argument is visible in shell history and `ps`; prefer {instead}"
        ))
    );
}

/// Hidden prompts on the terminal (stderr), for passwords and keys.
pub struct Terminal;

impl chainz_core::signer::Prompter for Terminal {
    fn is_interactive(&self) -> bool {
        std::io::stderr().is_terminal()
    }

    fn secret(&self, prompt: &str) -> Result<Zeroizing<String>> {
        Ok(Zeroizing::new(
            rpassword::prompt_password(prompt).context("failed to read the password")?,
        ))
    }
}

fn terminal_prompt<T>(result: dialoguer::Result<T>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(dialoguer::Error::IO(error)) if error.kind() == std::io::ErrorKind::Interrupted => {
            Err(ui::cancelled())
        }
        Err(dialoguer::Error::IO(error)) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_normalization_modes() {
        let whitespace = normalize_stdin("  token \n", "API key", StdinTrim::Whitespace).unwrap();
        assert_eq!(whitespace.as_str(), "token");
        let newlines =
            normalize_stdin("  value \r\n", "Value", StdinTrim::TrailingNewlines).unwrap();
        assert_eq!(newlines.as_str(), "  value ");
        let error = normalize_stdin(" \n", "Value", StdinTrim::Whitespace).unwrap_err();
        assert_eq!(error.to_string(), "Value from stdin was empty");
    }

    #[test]
    fn yes_skips_the_prompt_and_no_tty_needs_it() {
        assert!(confirm_or_yes(true, "sure?", "pass --yes").is_ok());
        if !is_tty() {
            let err = confirm_or_yes(false, "sure?", "pass --yes").unwrap_err();
            assert_eq!(err.to_string(), "pass --yes");
        }
    }
}
