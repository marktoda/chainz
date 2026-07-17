//! The single output vocabulary for chainz. Every user-facing styled line
//! goes through these helpers so glyphs and palette stay coherent.
//! `console` styles only when stdout is detected as a TTY, and honors NO_COLOR.

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

pub fn header(title: &str) -> String {
    format!(
        "\n{}\n{}",
        style(title).cyan().bold(),
        style("═".repeat(50)).dim()
    )
}

/// A section title without header's separator rule.
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
        assert!(header("Section").contains("Section"));
        assert!(header("Section").contains("═"));
        assert!(section("Keys").contains("Keys"));
        assert!(!section("Keys").contains("═"));
    }

    #[test]
    fn cancellation_is_typed() {
        let error = cancelled();
        assert!(is_cancelled(&error));
        assert_eq!(error.to_string(), "Cancelled");
    }
}
