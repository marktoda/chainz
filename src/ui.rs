//! The single output vocabulary for chainz. Every user-facing styled line
//! goes through these helpers so glyphs and palette stay coherent.
//! `console` styles only when stderr/stdout is a TTY and honors NO_COLOR.

use console::style;

pub fn header(title: &str) -> String {
    format!(
        "\n{}\n{}",
        style(title).cyan().bold(),
        style("═".repeat(50)).dim()
    )
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
    }
}
