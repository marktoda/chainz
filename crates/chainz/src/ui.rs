//! The single output vocabulary for chainz. Status and heading lines go
//! through these helpers so glyphs and palette stay coherent; tables and
//! the chain views live in `view`. Every warning is [`warn`] on stderr.
//! `console` styles only when stdout is detected as a TTY, and honors NO_COLOR.
//!
//! Errors that pick their own exit code are typed here: [`Cancelled`]
//! (130) and [`ExitCode`] (for example 127 for a command that does not
//! exist). `main` maps them.

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
    } else {
        1
    }
}

/// Print a command's error on stderr: `Cancelled` for a cancellation,
/// nothing for a closed stdout (`chainz … | head`), else `Error: …`.
pub fn report(error: &anyhow::Error) {
    if is_cancelled(error) {
        eprintln!("Cancelled");
    } else if !is_broken_pipe(error) {
        eprintln!("Error: {error:#}");
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
    fn cancellation_is_typed() {
        let error = cancelled();
        assert!(is_cancelled(&error));
        assert_eq!(error.to_string(), "Cancelled");
        assert_eq!(exit_code(&error), 130);
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
