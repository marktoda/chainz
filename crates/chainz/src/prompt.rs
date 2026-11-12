//! Terminal prompts and stdin secrets. Interrupting a prompt (Ctrl-C, Esc)
//! is [`ui::cancelled`], which exits 130.
//!
//! Interactive flows (pickers, the `chain set` menu, `init`, `chain add`)
//! ask through [`Ask`], so tests drive them with scripted answers; [`Tty`]
//! is the terminal. Core's hidden prompts (keys, passwords) take a
//! `Prompter`: [`Terminal`] on the terminal, or [`AskPrompter`] to route
//! them through an [`Ask`].

use crate::ui;
use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, FuzzySelect, Input, MultiSelect};
use std::io::IsTerminal;
use zeroize::Zeroizing;

/// Whether pickers and prompts may be shown: stdin and stderr are both
/// terminals. Scripts never block on a hidden prompt.
pub fn is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Yes or no.
fn confirm(message: &str, default: bool) -> Result<bool> {
    terminal_prompt(
        Confirm::new()
            .with_prompt(message)
            .default(default)
            .interact(),
    )
}

/// What declining, or having no TTY to ask on, means for [`confirm_or_yes`].
#[derive(Clone, Copy, Debug)]
pub enum Consent<'a> {
    /// Something that can't be undone: off a TTY it is an error naming
    /// `hint` (what to pass instead); declining is [`ui::cancelled`].
    Destructive { hint: &'a str },
    /// An optional extra: off a TTY it is skipped and `note` says how to
    /// get it; declining skips it.
    Proposal { note: &'a str },
}

/// Whether to go ahead: `--yes` accepts without asking; otherwise ask on
/// a TTY (default no), and off a TTY follow `consent`.
pub fn confirm_or_yes(yes: bool, message: &str, consent: Consent<'_>) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !is_tty() {
        return match consent {
            Consent::Destructive { hint } => bail!("{hint}"),
            Consent::Proposal { note } => {
                println!("{}", ui::dim(note));
                Ok(false)
            }
        };
    }
    match (confirm(message, false)?, consent) {
        (true, _) => Ok(true),
        (false, Consent::Destructive { .. }) => Err(ui::cancelled()),
        (false, Consent::Proposal { .. }) => Ok(false),
    }
}

/// The questions an interactive flow asks. Every method is a cancellation
/// point: Ctrl-C or Esc is [`ui::cancelled`].
pub trait Ask {
    /// Whether questions can be asked at all (on the terminal: stdin and
    /// stderr are both terminals). Off a terminal a flow takes its
    /// non-interactive path instead of asking.
    fn is_interactive(&self) -> bool;
    /// Pick one of `items`.
    fn select(&mut self, message: &str, items: &[String], default: usize) -> Result<usize>;
    /// Pick any of `items` (`defaults` says which start picked).
    fn multi_select(
        &mut self,
        message: &str,
        items: &[String],
        defaults: &[bool],
    ) -> Result<Vec<usize>>;
    /// A line of text (`default` when the user just presses enter).
    fn text(&mut self, message: &str, default: Option<&str>) -> Result<String>;
    /// A hidden line (a key).
    fn secret(&mut self, message: &str) -> Result<Zeroizing<String>>;
    /// Yes or no.
    fn confirm(&mut self, message: &str, default: bool) -> Result<bool>;
}

/// [`Ask`] on the terminal.
pub struct Tty;

impl Ask for Tty {
    fn is_interactive(&self) -> bool {
        is_tty()
    }

    fn select(&mut self, message: &str, items: &[String], default: usize) -> Result<usize> {
        select(message, items, default)
    }

    fn multi_select(
        &mut self,
        message: &str,
        items: &[String],
        defaults: &[bool],
    ) -> Result<Vec<usize>> {
        terminal_prompt(
            MultiSelect::new()
                .with_prompt(message)
                .items(items)
                .defaults(defaults)
                .interact_opt(),
        )?
        .ok_or_else(ui::cancelled)
    }

    fn text(&mut self, message: &str, default: Option<&str>) -> Result<String> {
        let mut input = Input::<String>::new()
            .with_prompt(message)
            .allow_empty(true);
        if let Some(default) = default {
            input = input.default(default.to_string());
        }
        terminal_prompt(input.interact_text())
    }

    fn secret(&mut self, message: &str) -> Result<Zeroizing<String>> {
        chainz_core::signer::Prompter::secret(&Terminal, &format!("{message}: "))
    }

    fn confirm(&mut self, message: &str, default: bool) -> Result<bool> {
        confirm(message, default)
    }
}

/// Pick one of `items`.
fn select(message: &str, items: &[String], default: usize) -> Result<usize> {
    terminal_prompt(
        FuzzySelect::new()
            .with_prompt(format!("{message} (ESC to exit)"))
            .items(items)
            .default(default)
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

/// Core's hidden prompts through an [`Ask`] (a flow that asks everything
/// else there, `init` say). The prompt's trailing `": "` is dropped:
/// [`Ask::secret`] adds its own.
pub struct AskPrompter<'a>(std::cell::RefCell<&'a mut dyn Ask>);

impl<'a> AskPrompter<'a> {
    pub fn new(ask: &'a mut dyn Ask) -> Self {
        Self(std::cell::RefCell::new(ask))
    }
}

impl chainz_core::signer::Prompter for AskPrompter<'_> {
    fn is_interactive(&self) -> bool {
        self.0.borrow().is_interactive()
    }

    fn secret(&self, prompt: &str) -> Result<Zeroizing<String>> {
        let message = prompt.trim_end().trim_end_matches(':');
        self.0.borrow_mut().secret(message)
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
        let destructive = Consent::Destructive { hint: "pass --yes" };
        let proposal = Consent::Proposal { note: "skipped" };
        assert!(confirm_or_yes(true, "sure?", destructive).unwrap());
        assert!(confirm_or_yes(true, "sure?", proposal).unwrap());
        if !is_tty() {
            let err = confirm_or_yes(false, "sure?", destructive).unwrap_err();
            assert_eq!(err.to_string(), "pass --yes");
            assert!(!confirm_or_yes(false, "sure?", proposal).unwrap());
        }
    }
}
