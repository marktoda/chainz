//! Small adapter around terminal prompting.
//!
//! Interactive command workflows depend on this interface instead of terminal
//! I/O directly, so their branching and cancellation behavior can be tested.

use crate::ui;
use anyhow::{Context, Result};
use dialoguer::{Confirm, FuzzySelect};
use std::io::IsTerminal;
use zeroize::Zeroizing;

pub trait Prompt {
    fn confirm(&mut self, message: &str, default: bool) -> Result<bool>;
    fn select(&mut self, message: &str, items: &[String], default: usize) -> Result<usize>;
    fn multi_select(
        &mut self,
        message: &str,
        items: &[String],
        defaults: &[bool],
    ) -> Result<Vec<usize>>;
}

pub struct SystemPrompt;

/// Whether pickers and prompts may be shown: stdin and stderr are both
/// terminals. Scripts never block on a hidden prompt.
pub fn is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

impl Prompt for SystemPrompt {
    fn multi_select(
        &mut self,
        message: &str,
        items: &[String],
        defaults: &[bool],
    ) -> Result<Vec<usize>> {
        terminal_prompt(
            dialoguer::MultiSelect::new()
                .with_prompt(message)
                .items(items)
                .defaults(defaults)
                .interact_opt(),
        )?
        .ok_or_else(ui::cancelled)
    }

    fn confirm(&mut self, message: &str, default: bool) -> Result<bool> {
        terminal_prompt(
            Confirm::new()
                .with_prompt(message)
                .default(default)
                .interact(),
        )
    }

    fn select(&mut self, message: &str, items: &[String], default: usize) -> Result<usize> {
        terminal_prompt(
            FuzzySelect::new()
                .with_prompt(format!("{} (ESC to exit)", message))
                .items(items)
                .default(default)
                .interact_opt(),
        )?
        .ok_or_else(ui::cancelled)
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
        anyhow::bail!("{label} from stdin was empty");
    }
    Ok(Zeroizing::new(normalized.to_string()))
}

fn terminal_prompt<T>(result: dialoguer::Result<T>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(dialoguer::Error::IO(error)) => terminal_io(Err(error)),
    }
}

fn terminal_io<T>(result: std::io::Result<T>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => Err(ui::cancelled()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::Prompt;
    use crate::ui;
    use anyhow::{Result, anyhow};
    use std::collections::VecDeque;

    /// Scripted answers for testing interactive handlers.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub(crate) enum Answer {
        Confirm(bool),
        Select(usize),
        Multi(Vec<usize>),
        Cancel,
    }

    pub(crate) struct ScriptedPrompt {
        answers: VecDeque<Answer>,
    }

    impl ScriptedPrompt {
        pub(crate) fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
            Self {
                answers: answers.into_iter().collect(),
            }
        }

        fn next(&mut self) -> Result<Answer> {
            self.answers
                .pop_front()
                .ok_or_else(|| anyhow!("scripted prompt ran out of answers"))
        }
    }

    impl Prompt for ScriptedPrompt {
        fn confirm(&mut self, _: &str, _: bool) -> Result<bool> {
            match self.next()? {
                Answer::Confirm(value) => Ok(value),
                Answer::Cancel => Err(ui::cancelled()),
                answer => Err(anyhow!("expected confirm answer, got {answer:?}")),
            }
        }

        fn select(&mut self, _: &str, _: &[String], _: usize) -> Result<usize> {
            match self.next()? {
                Answer::Select(value) => Ok(value),
                Answer::Cancel => Err(ui::cancelled()),
                answer => Err(anyhow!("expected select answer, got {answer:?}")),
            }
        }

        fn multi_select(&mut self, _: &str, _: &[String], _: &[bool]) -> Result<Vec<usize>> {
            match self.next()? {
                Answer::Multi(value) => Ok(value),
                Answer::Cancel => Err(ui::cancelled()),
                answer => Err(anyhow!("expected multi-select answer, got {answer:?}")),
            }
        }
    }

    #[test]
    fn stdin_normalization_modes() {
        use super::{StdinTrim, normalize_stdin};
        let whitespace = normalize_stdin("  token \n", "API key", StdinTrim::Whitespace).unwrap();
        assert_eq!(whitespace.as_str(), "token");
        let newlines =
            normalize_stdin("  value \r\n", "Value", StdinTrim::TrailingNewlines).unwrap();
        assert_eq!(newlines.as_str(), "  value ");
        let error = normalize_stdin(" \n", "Value", StdinTrim::Whitespace).unwrap_err();
        assert_eq!(error.to_string(), "Value from stdin was empty");
    }

    #[test]
    fn scripted_prompt_models_selection_and_cancellation() {
        let mut prompt = ScriptedPrompt::new([Answer::Select(1), Answer::Cancel]);
        assert_eq!(
            prompt.select("pick", &["a".into(), "b".into()], 0).unwrap(),
            1
        );
        assert!(crate::ui::is_cancelled(
            &prompt.confirm("sure?", true).unwrap_err()
        ));
    }
}
