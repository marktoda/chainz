//! The single output vocabulary for chainz. Every user-facing styled line
//! goes through these helpers so glyphs and palette stay coherent.
//! `console` styles only when stdout is detected as a TTY, and honors NO_COLOR.

use console::style;
use std::net::IpAddr;

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

/// Render an endpoint for people without exposing credentials embedded in
/// usernames, hostnames, paths, query strings, or fragments. Machine-readable
/// output deliberately bypasses this helper and keeps its established shape.
pub fn redact_url(value: &str) -> String {
    let Ok(url) = reqwest::Url::parse(value) else {
        return "<redacted endpoint>".to_string();
    };
    let Some(host) = url.host_str() else {
        return "<redacted endpoint>".to_string();
    };

    let bare_host = host.trim_start_matches('[').trim_end_matches(']');
    let local = bare_host.eq_ignore_ascii_case("localhost") || bare_host.parse::<IpAddr>().is_ok();
    let display_host = if local {
        if bare_host.contains(':') {
            format!("[{bare_host}]")
        } else {
            bare_host.to_string()
        }
    } else {
        let labels: Vec<_> = host.split('.').collect();
        match labels.as_slice() {
            [domain, suffix] => format!("{domain}.{suffix}"),
            [.., domain, suffix] => format!("…{domain}.{suffix}"),
            _ => "…".to_string(),
        }
    };
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let has_details = !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some();
    let suffix = if has_details { "/…" } else { "" };

    format!("{}://{}{}{}", url.scheme(), display_host, port, suffix)
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
    fn redact_url_hides_credentials_and_endpoint_details() {
        let redacted = redact_url(
            "https://secret-user:secret-pass@eth-mainnet.g.alchemy.com/v2/secret-token?key=secret",
        );

        assert_eq!(redacted, "https://…alchemy.com/…");
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("eth-mainnet"));
    }

    #[test]
    fn redact_url_keeps_local_endpoints_useful() {
        assert_eq!(redact_url("http://127.0.0.1:8545"), "http://127.0.0.1:8545");
        assert_eq!(
            redact_url("http://localhost:8545/rpc"),
            "http://localhost:8545/…"
        );
        assert_eq!(redact_url("http://[::1]:8545"), "http://[::1]:8545");
    }
}
