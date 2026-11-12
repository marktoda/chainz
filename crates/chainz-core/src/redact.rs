//! Credential-safe presentation of endpoint URLs and header values.
//!
//! The display policy is [`url()`] and [`header()`]: `${VAR}` templates, hosts
//! and ordinary paths print verbatim, only credential-shaped parts are
//! masked as [`REDACTED`], and anything unparseable fails closed. `reveal` prints the value as stored.
//!
//! [`redact`] (every literal part masked) and [`summarize`] (a terse
//! host label) are the stricter views kept for `Debug` output and compact
//! listings.

use crate::template;
use crate::urls::{self, is_local_host as is_local, registrable_domain as public_domain};

/// TOML parse errors quote the offending line, which may hold a secret
/// (an inline credential, a variable value): keep only message and position.
pub(crate) fn toml_error(error: &toml::de::Error) -> String {
    parse_error(error.message(), error.span())
}

/// [`toml_error`] for any TOML error's message and span (the
/// `toml_edit` parser errors quote the source line too).
pub(crate) fn parse_error(message: &str, span: Option<std::ops::Range<usize>>) -> String {
    match span {
        Some(span) => format!("{message} (at byte {})", span.start),
        None => message.to_string(),
    }
}

/// What replaces a masked value.
pub const REDACTED: &str = "[REDACTED]";

/// `value` (an endpoint or explorer URL) for display: verbatim with
/// `reveal`, else with user info, credential-shaped host labels, path
/// segments and query values, known provider keys and the fragment
/// masked. Templates, hosts and ordinary paths stay readable. A value that
/// does not parse as a URL is masked whole.
pub fn url(value: &str, reveal: bool) -> String {
    if reveal {
        return value.to_string();
    }
    let Ok(parsed) = url::Url::parse(value) else {
        return "[REDACTED URL]".to_string();
    };
    let mut out = format!("{}://", parsed.scheme());
    if !parsed.username().is_empty() || parsed.password().is_some() {
        out.push_str(REDACTED);
        out.push('@');
    }
    if let Some(host) = parsed.host_str() {
        let host = urls::unescape_braces(host);
        let labels: Vec<&str> = host
            .split('.')
            .map(|l| if is_secret_text(l) { REDACTED } else { l })
            .collect();
        out.push_str(&labels.join("."));
    }
    if let Some(port) = parsed.port() {
        out.push_str(&format!(":{port}"));
    }
    let known_key = urls::known_key(value).map(|(_, _, _, key)| key);
    let path = urls::unescape_braces(parsed.path());
    let segments: Vec<&str> = path
        .split('/')
        .map(|seg| {
            if !is_public_text(seg) || known_key == Some(seg) {
                REDACTED
            } else {
                seg
            }
        })
        .collect();
    out.push_str(&segments.join("/"));
    if let Some(query) = parsed.query() {
        let query = urls::unescape_braces(query);
        let pairs: Vec<String> = query
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((name, value))
                    if !template::has_templates(value)
                        && !value.is_empty()
                        && (urls::is_credential_param(name) || !is_public_text(value)) =>
                {
                    format!("{name}={REDACTED}")
                }
                Some(_) => pair.to_string(),
                None if !is_public_text(pair) => REDACTED.to_string(),
                None => pair.to_string(),
            })
            .collect();
        out.push('?');
        out.push_str(&pairs.join("&"));
    }
    if parsed.fragment().is_some_and(|f| !f.is_empty()) {
        out.push('#');
        out.push_str(REDACTED);
    }
    out
}

/// A header value for display: verbatim with `reveal`, else `${VAR}`
/// templates and a fixed list of auth schemes and media types kept, and
/// every other literal word masked (fails closed).
pub fn header(value: &str, reveal: bool) -> String {
    if reveal {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    for span in template::spans(value) {
        match span {
            template::Span::Var(name) => {
                out.push_str("${");
                out.push_str(name);
                out.push('}');
            }
            template::Span::Text(text) => {
                let mut word = String::new();
                for c in text.chars().chain(std::iter::once(' ')) {
                    if c.is_whitespace() || c == ',' {
                        out.push_str(if is_plain_word(&word) {
                            &word
                        } else {
                            REDACTED
                        });
                        word.clear();
                        out.push(c);
                    } else {
                        word.push(c);
                    }
                }
                out.pop();
            }
            template::Span::Unterminated(_) => out.push_str(REDACTED),
        }
    }
    out
}

/// A word a header shows: empty, an auth scheme or a media type. Any
/// other literal word may be a key.
fn is_plain_word(word: &str) -> bool {
    const SHOWN: &[&str] = &[
        "Bearer",
        "Basic",
        "Token",
        "Digest",
        "application/json",
        "text/plain",
        "gzip",
        "identity",
    ];
    word.is_empty() || SHOWN.iter().any(|w| w.eq_ignore_ascii_case(word))
}

/// Literal text that is credential-shaped (templates never are).
fn is_secret_text(text: &str) -> bool {
    urls::is_credential_shaped(text)
}

/// A path segment or query value that is safe to show: empty, a `${VAR}`
/// template, a number, a version (`v2`), or a short lowercase word
/// (`rpc`, `eth-mainnet`, `ext`). Everything else may be a key, so it
/// fails closed: a short or letters-only token is masked too.
fn is_public_text(text: &str) -> bool {
    if text.is_empty() || template::has_templates(text) {
        return true;
    }
    let bytes = text.as_bytes();
    let number = bytes.iter().all(u8::is_ascii_digit);
    let version = bytes.len() <= 4
        && bytes[0] == b'v'
        && bytes.len() > 1
        && bytes[1..].iter().all(u8::is_ascii_digit);
    let word = bytes.len() <= 24
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || matches!(b, b'-' | b'_' | b'.'));
    number || version || word
}

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
    template::spans(input)
        .filter_map(|span| match span {
            template::Span::Var(name) => Some(format!("${{{name}}}")),
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

    #[test]
    fn display_policy_masks_only_credential_shapes() {
        let shown = url(
            "https://mainnet.infura.io/v3/0123456789abcdef0123456789abcdef",
            false,
        );
        assert_eq!(shown, "https://mainnet.infura.io/v3/[REDACTED]");
        assert_eq!(
            url("https://mainnet.infura.io/v3/${INFURA_KEY}", false),
            "https://mainnet.infura.io/v3/${INFURA_KEY}"
        );
        assert_eq!(
            url("https://eth.llamarpc.com", false),
            "https://eth.llamarpc.com/"
        );
        assert_eq!(
            url("https://u:p@rpc.example/api?apikey=abc&chain=1#frag", false),
            "https://[REDACTED]@rpc.example/api?apikey=[REDACTED]&chain=1#[REDACTED]"
        );
        assert_eq!(
            url("https://x.example/?chainid=${CHAIN_ID}&key=${K}", false),
            "https://x.example/?chainid=${CHAIN_ID}&key=${K}"
        );
        assert_eq!(url("not a url secret", false), "[REDACTED URL]");
        assert_eq!(url("not a url secret", true), "not a url secret");
        // Short or letters-only literal keys never show: anything that is
        // not a number, version or short lowercase word is masked.
        assert_eq!(
            url("https://rpc.example/ext/bc/C/rpc/Ab3xYz9", false),
            "https://rpc.example/ext/bc/[REDACTED]/rpc/[REDACTED]"
        );
        assert_eq!(
            url(
                "https://rpc.example/KJHGFDSAPOIUYTREWQ?token=s3cr3t&page=2",
                false
            ),
            "https://rpc.example/[REDACTED]?token=[REDACTED]&page=2"
        );
        assert_eq!(
            url("https://x.example/v3/abc12?k=Zz", false),
            "https://x.example/v3/[REDACTED]?k=[REDACTED]"
        );
        // A known provider key is masked even when short.
        assert_eq!(
            url("https://eth-mainnet.g.alchemy.com/v2/abcdefgh", false),
            "https://eth-mainnet.g.alchemy.com/v2/[REDACTED]"
        );
    }

    #[test]
    fn header_policy_keeps_words_and_templates() {
        assert_eq!(header("Bearer ${TOKEN}", false), "Bearer ${TOKEN}");
        assert_eq!(header("application/json", false), "application/json");
        assert_eq!(header("Bearer abc123", false), "Bearer [REDACTED]");
        assert_eq!(header("sk-live-verylongsecretvalue", false), "[REDACTED]");
        assert_eq!(header("a, b1", false), "[REDACTED], [REDACTED]");
        // Letters-only literal keys are masked too.
        assert_eq!(header("abcdefghijkl", false), "[REDACTED]");
        assert_eq!(header("basic ${CREDS}", false), "basic ${CREDS}");
        assert_eq!(header("Bearer abc123", true), "Bearer abc123");
    }
}
