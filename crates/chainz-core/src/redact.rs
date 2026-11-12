//! Credential-safe endpoint presentation (ported from v1).
//!
//! This is the single policy boundary for displaying endpoint URLs. Callers
//! choose between a detailed redaction and a compact summary; neither view
//! exposes credentials embedded in user info, paths, queries, or fragments.

use crate::urls::{self, is_local_host as is_local, registrable_domain as public_domain};

/// Preserve useful public URL structure while removing credential-bearing
/// values. Malformed input fails closed.
pub fn redact(input: &str) -> String {
    let Ok(mut url) = url::Url::parse(input) else {
        return "[REDACTED URL]".to_string();
    };
    if let Some(host) = url.host_str()
        && !is_local(host)
        && let Some(domain) = public_domain(host)
        && domain != host
    {
        let _ = url.set_host(Some(&format!("redacted.{domain}")));
    }
    if !url.username().is_empty() {
        let _ = url.set_username("REDACTED");
    }
    if url.password().is_some() {
        let _ = url.set_password(Some("REDACTED"));
    }
    if url.query().is_some() {
        let names: Vec<String> = url
            .query_pairs()
            .map(|(name, _)| name.into_owned())
            .collect();
        url.set_query(None);
        for name in names {
            url.query_pairs_mut().append_pair(&name, "REDACTED");
        }
    }
    if url.path() != "/" && !url.path().is_empty() {
        let readable_path = urls::unescape_braces(url.path());
        let templates = variable_templates(&readable_path);
        let redacted_path = if templates.is_empty() {
            "/REDACTED".to_string()
        } else {
            format!("/REDACTED/{}", templates.join("/"))
        };
        url.set_path(&redacted_path);
    }
    if url.fragment().is_some() {
        url.set_fragment(Some("REDACTED"));
    }
    // url::Url percent-encodes braces, but variable names are safe public
    // metadata and keeping ${NAME} intact makes redacted output actionable.
    urls::unescape_braces(url.as_str())
}

/// Produce the terse endpoint label used by compact human listings.
pub fn summarize(value: &str) -> String {
    let Ok(url) = url::Url::parse(value) else {
        return "<redacted endpoint>".to_string();
    };
    let Some(host) = url.host_str() else {
        return "<redacted endpoint>".to_string();
    };

    let bare_host = urls::bare(host);
    let display_host = if is_local(host) {
        if bare_host.contains(':') {
            format!("[{bare_host}]")
        } else {
            bare_host.to_string()
        }
    } else {
        public_domain(host)
            .map(|domain| {
                if domain == host {
                    domain.to_string()
                } else {
                    format!("…{domain}")
                }
            })
            .unwrap_or_else(|| "…".to_string())
    };
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let has_details = !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some();
    let suffix = if has_details { "/…" } else { "" };

    format!("{}://{}{}{}", url.scheme(), display_host, port, suffix)
}

fn variable_templates(input: &str) -> Vec<String> {
    crate::template::spans(input)
        .filter_map(|span| match span {
            crate::template::Span::Var(name) => Some(format!("${{{name}}}")),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detailed_redaction_hides_literal_credentials() {
        let redacted = redact(
            "https://user:password@private.rpc.example.com/v2/path-secret?token=query-secret#fragment-secret",
        );
        for secret in [
            "user",
            "password",
            "path-secret",
            "query-secret",
            "fragment-secret",
            "private",
        ] {
            assert!(!redacted.contains(secret), "{redacted}");
        }
        assert!(redacted.contains("redacted.example.com"));
        assert!(redacted.contains("REDACTED"));
    }

    #[test]
    fn detailed_redaction_preserves_template_names_and_fails_closed() {
        let redacted = redact("https://rpc.example.com/literal-secret/${ALCHEMY_KEY}");
        assert!(redacted.contains("${ALCHEMY_KEY}"));
        assert!(!redacted.contains("literal-secret"));
        assert_eq!(
            redact("not a URL containing literal-secret"),
            "[REDACTED URL]"
        );
        assert_eq!(redact("http://127.0.0.1:8545"), "http://127.0.0.1:8545/");
    }

    #[test]
    fn summary_hides_remote_details_and_keeps_local_endpoints_useful() {
        assert_eq!(
            summarize(
                "https://secret-user:secret-pass@eth-mainnet.g.alchemy.com/v2/token?key=secret"
            ),
            "https://…alchemy.com/…"
        );
        assert_eq!(summarize("http://127.0.0.1:8545"), "http://127.0.0.1:8545");
        assert_eq!(
            summarize("http://localhost:8545/rpc"),
            "http://localhost:8545/…"
        );
        assert_eq!(summarize("http://[::1]:8545"), "http://[::1]:8545");
    }
}
