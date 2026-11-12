//! Terminal prompts and stdin secrets. Interrupting a prompt (Ctrl-C, Esc)
//! is [`ui::cancelled`], which exits 130.
//!
//! Interactive flows (pickers, the `update` menu, `init`, `add`)
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

/// Whether to go ahead: `--yes` accepts without asking; otherwise ask
/// through `ask` when it is interactive (default no), and off a terminal
/// follow `consent`.
pub fn confirm_or_yes(
    ask: &mut dyn Ask,
    yes: bool,
    message: &str,
    consent: Consent<'_>,
) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !ask.is_interactive() {
        return match consent {
            Consent::Destructive { hint } => bail!("{hint}"),
            Consent::Proposal { note } => {
                println!("{}", ui::dim(note));
                Ok(false)
            }
        };
    }
    match (ask.confirm(message, false)?, consent) {
        (true, _) => Ok(true),
        (false, Consent::Destructive { .. }) => Err(ui::cancelled()),
        (false, Consent::Proposal { .. }) => Ok(false),
    }
}

/// The one confirmation every `rm` goes through (`rm`,
/// `addr rm`, `var rm`, `signer rm`): `--yes` removes without asking; on a
/// terminal, ask "Remove {what}?" (default no; declining cancels); off
/// one, fail naming `--yes`. `command` is the command as typed after
/// `chainz`, for that error.
pub fn confirm_removal(ask: &mut dyn Ask, yes: bool, command: &str, what: &str) -> Result<()> {
    let hint = format!("`chainz {command}` removes {what}; pass --yes to confirm");
    confirm_or_yes(
        ask,
        yes,
        &format!("Remove {what}?"),
        Consent::Destructive { hint: &hint },
    )
    .map(drop)
}

/// What Esc means in a picker ([`Ask::choose`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Esc {
    /// Leave the flow (the picker's hint says "ESC to exit").
    Exit,
    /// Go back one step, as in a `update` menu's sub-picker ("ESC:
    /// back").
    Back,
}

impl Esc {
    fn hint(self) -> &'static str {
        match self {
            Esc::Exit => "ESC to exit",
            Esc::Back => "ESC: back",
        }
    }
}

/// The questions an interactive flow asks. Every method is a cancellation
/// point: Ctrl-C or Esc is [`ui::cancelled`].
pub trait Ask {
    /// Whether questions can be asked at all (on the terminal: stdin and
    /// stderr are both terminals). Off a terminal a flow takes its
    /// non-interactive path instead of asking.
    fn is_interactive(&self) -> bool;
    /// Pick one of `items`, starting on `default`; `None` when the user
    /// pressed Esc (`esc` says what that means, for the hint).
    fn choose(
        &mut self,
        message: &str,
        items: &[String],
        default: usize,
        esc: Esc,
    ) -> Result<Option<usize>>;
    /// Pick one of `items`, starting on `default`; Esc is
    /// [`ui::cancelled`].
    fn select(&mut self, message: &str, items: &[String], default: usize) -> Result<usize> {
        self.choose(message, items, default, Esc::Exit)?
            .ok_or_else(ui::cancelled)
    }
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

    fn choose(
        &mut self,
        message: &str,
        items: &[String],
        default: usize,
        esc: Esc,
    ) -> Result<Option<usize>> {
        terminal_prompt(
            FuzzySelect::new()
                .with_prompt(format!("{message} ({})", esc.hint()))
                .items(items)
                .default(default)
                .interact_opt(),
        )
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
    fn removals_ask_or_need_yes() {
        use script::{Answer::Yes, Script};
        confirm_removal(&mut Script::off_tty(), true, "var rm X", "variable 'X'").unwrap();
        let err =
            confirm_removal(&mut Script::off_tty(), false, "var rm X", "variable 'X'").unwrap_err();
        assert_eq!(
            err.to_string(),
            "`chainz var rm X` removes variable 'X'; pass --yes to confirm"
        );
        let mut script = Script::new([Yes(true), Yes(false)]);
        confirm_removal(&mut script, false, "var rm X", "variable 'X'").unwrap();
        let err = confirm_removal(&mut script, false, "var rm X", "variable 'X'").unwrap_err();
        assert!(ui::is_cancelled(&err));
        assert_eq!(
            script.asked,
            ["Remove variable 'X'?", "Remove variable 'X'?"]
        );
    }

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
        use script::{Answer::Yes, Script};
        let destructive = Consent::Destructive { hint: "pass --yes" };
        let proposal = Consent::Proposal { note: "skipped" };
        let off_tty = &mut Script::off_tty();
        assert!(confirm_or_yes(off_tty, true, "sure?", destructive).unwrap());
        assert!(confirm_or_yes(off_tty, true, "sure?", proposal).unwrap());
        let err = confirm_or_yes(off_tty, false, "sure?", destructive).unwrap_err();
        assert_eq!(err.to_string(), "pass --yes");
        assert!(!confirm_or_yes(off_tty, false, "sure?", proposal).unwrap());
        // On a terminal: asked; declining a destructive step cancels.
        let mut script = Script::new([Yes(true), Yes(false), Yes(false)]);
        assert!(confirm_or_yes(&mut script, false, "sure?", destructive).unwrap());
        let err = confirm_or_yes(&mut script, false, "sure?", destructive).unwrap_err();
        assert!(ui::is_cancelled(&err));
        assert!(!confirm_or_yes(&mut script, false, "sure?", proposal).unwrap());
    }
}

/// Scripted answers for tests of interactive flows.
#[cfg(test)]
pub(crate) mod script {
    use super::{Ask, Esc};
    use anyhow::Result;
    use std::collections::VecDeque;
    use zeroize::Zeroizing;

    /// One answer.
    #[derive(Debug)]
    pub(crate) enum Answer {
        /// Pick item `i` (a select).
        Pick(usize),
        /// Press enter on a select: its default.
        Enter,
        /// Press Esc on a select.
        Esc,
        /// Type this (a text or a secret).
        Text(&'static str),
        /// Yes or no (a confirm).
        Yes(bool),
    }

    /// Answers every question in order; running out, or a question of
    /// the wrong kind, is a test failure. Records what was asked.
    pub(crate) struct Script {
        answers: VecDeque<Answer>,
        interactive: bool,
        /// Every question asked, in order.
        pub(crate) asked: Vec<String>,
        /// The default each select started on, in order.
        pub(crate) defaults: Vec<usize>,
    }

    impl Script {
        pub(crate) fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
            Script {
                answers: answers.into_iter().collect(),
                interactive: true,
                asked: Vec::new(),
                defaults: Vec::new(),
            }
        }

        /// No terminal: nothing may be asked.
        pub(crate) fn off_tty() -> Self {
            Script {
                interactive: false,
                ..Script::new([])
            }
        }

        /// Whether every answer was used.
        pub(crate) fn done(&self) -> bool {
            self.answers.is_empty()
        }

        fn next(&mut self, message: &str) -> Answer {
            assert!(self.interactive, "'{message}' asked off a terminal");
            self.asked.push(message.to_string());
            self.answers
                .pop_front()
                .unwrap_or_else(|| panic!("no scripted answer for '{message}'"))
        }
    }

    impl Ask for Script {
        fn is_interactive(&self) -> bool {
            self.interactive
        }
        fn choose(
            &mut self,
            message: &str,
            items: &[String],
            default: usize,
            _: Esc,
        ) -> Result<Option<usize>> {
            self.defaults.push(default);
            match self.next(message) {
                Answer::Pick(i) => {
                    assert!(i < items.len(), "'{message}' has no item {i}: {items:?}");
                    Ok(Some(i))
                }
                Answer::Enter => Ok(Some(default)),
                Answer::Esc => Ok(None),
                other => panic!("'{message}' is a select, not {other:?}"),
            }
        }
        fn multi_select(&mut self, message: &str, _: &[String], _: &[bool]) -> Result<Vec<usize>> {
            panic!("'{message}' is a multi-select: not scripted")
        }
        fn text(&mut self, message: &str, _: Option<&str>) -> Result<String> {
            match self.next(message) {
                Answer::Text(t) => Ok(t.to_string()),
                other => panic!("'{message}' is a text, not {other:?}"),
            }
        }
        fn secret(&mut self, message: &str) -> Result<Zeroizing<String>> {
            match self.next(message) {
                Answer::Text(t) => Ok(Zeroizing::new(t.to_string())),
                other => panic!("'{message}' is a secret, not {other:?}"),
            }
        }
        fn confirm(&mut self, message: &str, _: bool) -> Result<bool> {
            match self.next(message) {
                Answer::Yes(yes) => Ok(yes),
                other => panic!("'{message}' is a confirm, not {other:?}"),
            }
        }
    }
}
