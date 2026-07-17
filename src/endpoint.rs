//! Credential-safe endpoint presentation.
//!
//! This is the single policy boundary for displaying endpoint URLs. Callers
//! choose between a detailed redaction and a compact summary; neither view
//! exposes credentials embedded in user info, paths, queries, or fragments.

use std::net::IpAddr;

/// Preserve useful public URL structure while removing credential-bearing
/// values. Malformed input fails closed.
pub(crate) fn redact(input: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(input) else {
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
        let readable_path = url.path().replace("%7B", "{").replace("%7D", "}");
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
    url.to_string().replace("%7B", "{").replace("%7D", "}")
}

/// Produce the terse endpoint label used by compact human listings.
pub(crate) fn summarize(value: &str) -> String {
    let Ok(url) = reqwest::Url::parse(value) else {
        return "<redacted endpoint>".to_string();
    };
    let Some(host) = url.host_str() else {
        return "<redacted endpoint>".to_string();
    };

    let bare_host = host.trim_start_matches('[').trim_end_matches(']');
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

fn public_domain(host: &str) -> Option<&str> {
    let mut labels = host.rsplitn(3, '.');
    let suffix = labels.next()?;
    let domain = labels.next()?;
    let start = host.len().checked_sub(domain.len() + suffix.len() + 1)?;
    Some(&host[start..])
}

fn is_local(host: &str) -> bool {
    let bare_host = host.trim_start_matches('[').trim_end_matches(']');
    bare_host.eq_ignore_ascii_case("localhost") || bare_host.parse::<IpAddr>().is_ok()
}

fn variable_templates(input: &str) -> Vec<String> {
    let mut templates = Vec::new();
    let mut remainder = input;
    while let Some(start) = remainder.find("${") {
        let Some(relative_end) = remainder[start..].find('}') else {
            break;
        };
        let end = start + relative_end + 1;
        templates.push(remainder[start..end].to_string());
        remainder = &remainder[end..];
    }
    templates
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
