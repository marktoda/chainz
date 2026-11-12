//! URL facts shared by naming, redaction, credential extraction and
//! explorer detection: which hosts are local, a host's registrable domain
//! and label, websocket schemes, and the `${NAME}` braces `url::Url`
//! percent-encodes.

use std::net::IpAddr;

/// The lowercase host of `url`, without IPv6 brackets.
pub(crate) fn host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(bare(host).to_ascii_lowercase())
}

/// `host` without IPv6 brackets.
pub(crate) fn bare(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}

/// `localhost` or an IP address (bracketed or not).
pub(crate) fn is_local_host(host: &str) -> bool {
    let host = bare(host);
    host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok()
}

/// The last two labels of `host` (`eth.llamarpc.com` → `llamarpc.com`);
/// `None` for a single-label host.
pub(crate) fn registrable_domain(host: &str) -> Option<&str> {
    let mut labels = host.rsplitn(3, '.');
    let suffix = labels.next()?;
    let domain = labels.next()?;
    let start = host.len().checked_sub(domain.len() + suffix.len() + 1)?;
    Some(&host[start..])
}

/// The registrable label of `host` (`mainnet.infura.io` → `infura`), the
/// only label of a single-label host, `None` for an empty one.
pub(crate) fn domain_label(host: &str) -> Option<&str> {
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    match labels.len() {
        0 => None,
        1 => Some(labels[0]),
        n => Some(labels[n - 2]),
    }
}

/// Whether any dot-separated label of `url`'s host is `label`.
pub(crate) fn host_has_label(url: &str, label: &str) -> bool {
    host(url).is_some_and(|host| host.split('.').any(|l| l == label))
}

/// `ws://` or `wss://` (case-insensitive, leading whitespace ignored).
pub(crate) fn is_websocket(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("ws://") || lower.starts_with("wss://")
}

/// `http://` or `https://` (case-insensitive).
pub(crate) fn is_http(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Undo `url::Url`'s percent-encoding of `{` and `}`, so `${NAME}`
/// templates read as written.
pub(crate) fn unescape_braces(text: &str) -> String {
    text.replace("%7B", "{").replace("%7D", "}")
}

/// Find a literal Infura or Alchemy key in `url`: returns the text before
/// and after the key, the base variable name, and the key.
pub(crate) fn known_key(url: &str) -> Option<(&str, &str, &'static str, &str)> {
    let (marker, base) = if host_has_label(url, "infura") {
        ("/v3/", "INFURA_KEY")
    } else if host_has_label(url, "alchemy") {
        ("/v2/", "ALCHEMY_KEY")
    } else {
        return None;
    };
    let start = url.find(marker)? + marker.len();
    let rest = &url[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let key = &rest[..end];
    if key.len() < 8
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((&url[..start], &rest[end..], base, key))
}

/// Query parameter names that carry a credential.
const CREDENTIAL_PARAMS: [&str; 6] = ["key", "token", "secret", "auth", "password", "apikey"];

/// Whether query parameter `name` names a credential (`apikey`, `token`…).
pub(crate) fn is_credential_param(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    CREDENTIAL_PARAMS.iter().any(|k| name.contains(k))
}

/// How long a literal must be to be taken for a credential (a URL part
/// here, a header value in `extract`).
pub(crate) const CREDENTIAL_MIN_LEN: usize = 16;

/// Whether `text` looks like a credential: [`CREDENTIAL_MIN_LEN`] or more
/// `[A-Za-z0-9_-]` characters mixing letters and digits, with no template.
pub(crate) fn is_credential_shaped(text: &str) -> bool {
    text.len() >= CREDENTIAL_MIN_LEN
        && !crate::template::has_templates(text)
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && text.bytes().any(|b| b.is_ascii_digit())
        && text.bytes().any(|b| b.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_domains_and_schemes() {
        assert_eq!(host("https://[::1]:8545").as_deref(), Some("::1"));
        assert!(is_local_host("[::1]"));
        assert!(is_local_host("LOCALHOST"));
        assert!(!is_local_host("example.com"));
        assert_eq!(registrable_domain("eth.llamarpc.com"), Some("llamarpc.com"));
        assert_eq!(registrable_domain("localhost"), None);
        assert_eq!(domain_label("mainnet.infura.io"), Some("infura"));
        assert_eq!(domain_label("gateway"), Some("gateway"));
        assert!(host_has_label("https://api.etherscan.io/v2", "etherscan"));
        assert!(is_websocket(" WSS://x"));
        assert!(is_http("HTTPS://x"));
        assert!(!is_http("ws://x"));
        assert_eq!(unescape_braces("/%7BKEY%7D"), "/{KEY}");
    }
}
