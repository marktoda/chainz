//! Credential-safe presentation of endpoint URLs and header values.
//!
//! The display policy is [`url()`] and [`header()`]: `${VAR}` templates, hosts
//! and ordinary paths print verbatim, only credential-shaped parts are
//! masked as [`REDACTED`], and anything unparseable fails closed. Every
//! view of a URL that is not revealed on request, `Debug` output included,
//! goes through them.
//!
//! [`summarize`] (a terse host label) is the stricter view for compact
//! listings.

use crate::template;
use crate::urls;

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

/// What replaces a URL that does not parse.
const REDACTED_URL: &str = "[REDACTED URL]";

/// `value` (an endpoint or explorer URL) for display: user info,
/// credential-shaped host labels, path segments and query values, known
/// provider keys and the fragment masked. Templates, hosts and ordinary
/// paths stay readable. A value that does not parse as a URL is masked
/// whole.
pub fn url(value: &str) -> String {
    let Ok(parsed) = url::Url::parse(value) else {
        return REDACTED_URL.to_string();
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
            .map(|l| {
                if urls::is_credential_shaped(l) {
                    REDACTED
                } else {
                    l
                }
            })
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
    // The parser adds the root path to `http://host`; print it only when
    // the value has it, so the display matches the URL chainz exports.
    if parsed.path() != "/" || has_explicit_path(value) {
        out.push_str(&segments.join("/"));
    }
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

/// Whether `value` (a URL) spells out a path after its authority, even
/// just `/`.
fn has_explicit_path(value: &str) -> bool {
    let rest = value.split_once("://").map_or(value, |(_, rest)| rest);
    rest.find(['/', '?', '#'])
        .is_some_and(|at| rest.as_bytes()[at] == b'/')
}

/// A header value for display: `${VAR}` templates and a fixed list of
/// auth schemes and media types kept, and every other literal word masked
/// (fails closed).
pub fn header(value: &str) -> String {
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

/// Produce the terse endpoint label used by compact human listings.
pub fn summarize(value: &str) -> String {
    let Ok(url) = url::Url::parse(value) else {
        return "<redacted endpoint>".to_string();
    };
    let Some(host) = url.host_str() else {
        return "<redacted endpoint>".to_string();
    };

    let bare_host = urls::bare(host);
    let display_host = if urls::is_local_host(host) {
        if bare_host.contains(':') {
            format!("[{bare_host}]")
        } else {
            bare_host.to_string()
        }
    } else {
        urls::registrable_domain(host)
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The display is the exported URL, masked: no root path is added.
    #[test]
    fn display_keeps_the_urls_own_path() {
        assert_eq!(url("http://127.0.0.1:11"), "http://127.0.0.1:11");
        assert_eq!(url("http://127.0.0.1:11/"), "http://127.0.0.1:11/");
        assert_eq!(url("http://127.0.0.1:11?x=1"), "http://127.0.0.1:11?x=1");
        assert_eq!(url("https://rpc.example/v1"), "https://rpc.example/v1");
    }

    #[test]
    fn display_policy_masks_only_credential_shapes() {
        let shown = url("https://mainnet.infura.io/v3/0123456789abcdef0123456789abcdef");
        assert_eq!(shown, "https://mainnet.infura.io/v3/[REDACTED]");
        assert_eq!(
            url("https://mainnet.infura.io/v3/${INFURA_KEY}"),
            "https://mainnet.infura.io/v3/${INFURA_KEY}"
        );
        assert_eq!(url("https://eth.llamarpc.com"), "https://eth.llamarpc.com");
        assert_eq!(
            url("https://u:p@rpc.example/api?apikey=abc&chain=1#frag"),
            "https://[REDACTED]@rpc.example/api?apikey=[REDACTED]&chain=1#[REDACTED]"
        );
        assert_eq!(
            url("https://x.example/?chainid=${CHAIN_ID}&key=${K}"),
            "https://x.example/?chainid=${CHAIN_ID}&key=${K}"
        );
        assert_eq!(url("not a url secret"), "[REDACTED URL]");
        // Short or letters-only literal keys never show: anything that is
        // not a number, version or short lowercase word is masked.
        assert_eq!(
            url("https://rpc.example/ext/bc/C/rpc/Ab3xYz9"),
            "https://rpc.example/ext/bc/[REDACTED]/rpc/[REDACTED]"
        );
        assert_eq!(
            url("https://rpc.example/KJHGFDSAPOIUYTREWQ?token=s3cr3t&page=2"),
            "https://rpc.example/[REDACTED]?token=[REDACTED]&page=2"
        );
        assert_eq!(
            url("https://x.example/v3/abc12?k=Zz"),
            "https://x.example/v3/[REDACTED]?k=[REDACTED]"
        );
        // A known provider key is masked even when short.
        assert_eq!(
            url("https://eth-mainnet.g.alchemy.com/v2/abcdefgh"),
            "https://eth-mainnet.g.alchemy.com/v2/[REDACTED]"
        );
    }

    #[test]
    fn header_policy_keeps_words_and_templates() {
        assert_eq!(header("Bearer ${TOKEN}"), "Bearer ${TOKEN}");
        assert_eq!(header("application/json"), "application/json");
        assert_eq!(header("Bearer abc123"), "Bearer [REDACTED]");
        assert_eq!(header("sk-live-verylongsecretvalue"), "[REDACTED]");
        assert_eq!(header("a, b1"), "[REDACTED], [REDACTED]");
        // Letters-only literal keys are masked too.
        assert_eq!(header("abcdefghijkl"), "[REDACTED]");
        assert_eq!(header("basic ${CREDS}"), "basic ${CREDS}");
    }
}
