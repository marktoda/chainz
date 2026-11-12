//! The single output vocabulary for chainz. Status and heading lines go
//! through these helpers so glyphs and palette stay coherent; tables and
//! the chain views live in `view`. Every warning is [`warn`] on stderr.
//! `console` styles only when stdout is detected as a TTY, and honors NO_COLOR.
//!
//! Errors that pick their own exit code are typed here: [`Cancelled`]
//! (130) and [`ExitCode`] (for example 127 for a command that does not
//! exist); a `clap::Error` (a handler's usage error) prints as clap's do
//! and exits 2. `main` maps them.

use console::style;

#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub fn cancelled() -> anyhow::Error {
    anyhow::Error::new(Cancelled)
}

pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Cancelled>().is_some()
}

/// An error that exits with a specific code instead of 1.
#[derive(Debug)]
pub struct ExitCode {
    /// The process exit code.
    pub code: i32,
    /// What went wrong.
    pub message: String,
}

impl std::fmt::Display for ExitCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitCode {}

/// The exit code an error asks for: 130 when cancelled, 0 for a closed
/// stdout (the reader went away: not a failure), an [`ExitCode`]'s own,
/// else 1.
pub fn exit_code(error: &anyhow::Error) -> i32 {
    if is_cancelled(error) {
        130
    } else if is_broken_pipe(error) {
        0
    } else if let Some(exit) = error.downcast_ref::<ExitCode>() {
        exit.code
    } else if let Some(usage) = error.downcast_ref::<clap::Error>() {
        usage.exit_code()
    } else {
        1
    }
}

/// Print a command's error on stderr: `Cancelled` for a cancellation,
/// nothing for a closed stdout (`chainz … | head`), else `error: …`.
pub fn report(error: &anyhow::Error) {
    if is_cancelled(error) {
        eprintln!("Cancelled");
    } else if let Some(usage) = error.downcast_ref::<clap::Error>() {
        let _ = usage.print();
    } else if !is_broken_pipe(error) {
        eprintln!("error: {error:#}");
    }
}

/// Whether `error` comes from writing to a closed pipe.
pub fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

/// Make a closed stdout or stderr (`chainz rpc ls | head`) end the process
/// quietly with exit 0, as [`exit_code`] does for an error: `println!`
/// panics on a broken pipe, and this hook turns that panic, wherever it
/// happens, into a silent exit. Every other panic goes to the previous
/// hook. `main` installs it first thing, so it covers every command.
pub fn exit_quietly_on_broken_pipe() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if is_broken_pipe_panic(panic_message(info.payload())) {
            std::process::exit(0);
        }
        previous(info);
    }));
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or_default()
}

/// Whether a panic message is std's "failed printing to stdout: Broken
/// pipe (os error 32)" (or stderr).
fn is_broken_pipe_panic(message: &str) -> bool {
    message.starts_with("failed printing to std") && message.contains("Broken pipe")
}

/// A section title: a blank line, then the title in bold cyan.
pub fn section(title: &str) -> String {
    format!("\n{}", style(title).cyan().bold())
}

pub fn success(msg: &str) -> String {
    format!("{} {}", style("✓").green(), msg)
}

pub fn warn(msg: &str) -> String {
    format!("{} {}", style("⚠").yellow(), msg)
}

pub fn fail(msg: &str) -> String {
    format!("{} {}", style("✗").red(), msg)
}

pub fn item(msg: &str) -> String {
    format!("{} {}", style("▸").cyan(), msg)
}

/// `1 chain`, `17 chains`: a count with its noun (plural with `s`).
pub fn count(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    }
}

pub fn dim(msg: &str) -> String {
    style(msg).dim().to_string()
}

pub fn emph(msg: &str) -> String {
    style(msg).yellow().to_string()
}

/// Say which secrets moved out of `config.toml` (stdout, after the
/// command's own report).
pub fn report_moved(moved: &[chainz_core::extract::Moved]) {
    for m in moved {
        println!("  moved the secret in {m} (secrets.toml)");
    }
}

/// Print a core notice on stderr, `chainz: ` before each line.
pub fn notice(notice: &chainz_core::store::Notice) {
    for line in notice.to_string().lines() {
        eprintln!("chainz: {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_contain_glyph_and_message() {
        assert!(success("done").contains("✓"));
        assert!(success("done").contains("done"));
        assert!(warn("careful").contains("⚠"));
        assert!(fail("broken").contains("✗"));
        assert!(item("step").contains("▸"));
        assert!(section("Keys").contains("Keys"));
    }

    #[test]
    fn counts() {
        assert_eq!(count(1, "chain"), "1 chain");
        assert_eq!(count(0, "chain"), "0 chains");
        assert_eq!(count(17, "endpoint"), "17 endpoints");
    }

    #[test]
    fn cancellation_is_typed() {
        let error = cancelled();
        assert!(is_cancelled(&error));
        assert_eq!(error.to_string(), "Cancelled");
        assert_eq!(exit_code(&error), 130);
    }

    #[test]
    fn only_broken_pipe_panics_exit_quietly() {
        assert!(is_broken_pipe_panic(
            "failed printing to stdout: Broken pipe (os error 32)"
        ));
        assert!(is_broken_pipe_panic(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
        assert!(!is_broken_pipe_panic("index out of bounds"));
        assert!(!is_broken_pipe_panic(
            "failed printing to stdout: No space left on device (os error 28)"
        ));
    }

    #[test]
    fn exit_codes_are_typed() {
        let error = anyhow::Error::new(ExitCode {
            code: 127,
            message: "no such command".into(),
        });
        assert_eq!(exit_code(&error), 127);
        assert_eq!(exit_code(&error.context("outer")), 127);
        assert_eq!(exit_code(&anyhow::anyhow!("plain")), 1);
    }
}
