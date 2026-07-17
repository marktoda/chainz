//! Small adapter around terminal prompting.
//!
//! Interactive command workflows depend on this interface instead of terminal
//! I/O directly, so their branching and cancellation behavior can be tested.

use crate::ui;
use anyhow::Result;
use dialoguer::{Confirm, FuzzySelect, Input};
use std::io::IsTerminal;

pub(crate) trait Prompt {
    fn is_interactive(&self) -> bool;
    fn text(&mut self, message: &str, default: Option<&str>, allow_empty: bool) -> Result<String>;
    fn secret(&mut self, message: &str) -> Result<String>;
    fn confirm(&mut self, message: &str, default: bool) -> Result<bool>;
    fn select(&mut self, message: &str, items: &[String], default: usize) -> Result<usize>;
}

pub(crate) struct SystemPrompt;

impl Prompt for SystemPrompt {
    fn is_interactive(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn text(&mut self, message: &str, default: Option<&str>, allow_empty: bool) -> Result<String> {
        let mut input = Input::new().with_prompt(message).allow_empty(allow_empty);
        if let Some(default) = default {
            input = input.default(default.to_string());
        }
        terminal_prompt(input.interact_text())
    }

    fn secret(&mut self, message: &str) -> Result<String> {
        terminal_io(rpassword::prompt_password(message))
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

    #[derive(Debug)]
    pub(crate) enum Answer {
        Text(String),
        Secret(String),
        Confirm(bool),
        Select(usize),
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
        fn is_interactive(&self) -> bool {
            true
        }

        fn text(&mut self, _: &str, _: Option<&str>, _: bool) -> Result<String> {
            match self.next()? {
                Answer::Text(value) => Ok(value),
                Answer::Cancel => Err(ui::cancelled()),
                answer => Err(anyhow!("expected text answer, got {answer:?}")),
            }
        }

        fn secret(&mut self, _: &str) -> Result<String> {
            match self.next()? {
                Answer::Secret(value) => Ok(value),
                Answer::Cancel => Err(ui::cancelled()),
                answer => Err(anyhow!("expected secret answer, got {answer:?}")),
            }
        }

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
    }

    #[test]
    fn scripted_prompt_models_selection_and_cancellation() {
        let mut prompt = ScriptedPrompt::new([Answer::Select(1), Answer::Cancel]);
        assert_eq!(
            prompt.select("pick", &["a".into(), "b".into()], 0).unwrap(),
            1
        );
        assert!(crate::ui::is_cancelled(
            &prompt.text("value", None, true).unwrap_err()
        ));
    }
}
