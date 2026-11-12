//! Loopback auth for local JSON-RPC servers: the gateway, and anything else
//! that hands a tokenised `http://127.0.0.1:<port>/<token>…` URL to a child
//! process (the fork proxy).
//!
//! Three checks, all needed: a random per-start token in the path (other
//! local users can't guess it), a `Host` naming this loopback server (a web
//! page can't reach it through DNS rebinding), and a constant-time compare
//! (the token can't be found byte by byte).
//!
//! The servers listen on `127.0.0.1` only, so `Host: [::1]` (a client that
//! connected over IPv6) can't be one of their own requests and is refused.

/// Random bytes in a token (256 bits).
pub const TOKEN_BYTES: usize = 32;

/// A fresh token: [`TOKEN_BYTES`] random bytes as lowercase hex (64
/// characters).
pub fn new_token() -> String {
    let hex = chainz_rpc::hex::encode(&rand::random::<[u8; TOKEN_BYTES]>());
    hex.strip_prefix("0x").unwrap_or(&hex).to_string()
}

/// Whether `given` equals `expected`, in time that does not depend on
/// where they first differ.
pub fn token_matches(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Whether a `Host` header value names this server: `127.0.0.1` or
/// `localhost` (any case), with `port` or with no port. A missing header
/// (or one that isn't text) is refused.
pub fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        return false;
    };
    let (name, host_port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    (name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost"))
        && host_port.is_none_or(|p| p.parse() == Ok(port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_256_bit_hex_and_fresh() {
        let a = new_token();
        assert_eq!(a.len(), 2 * TOKEN_BYTES);
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_ne!(a, new_token());
    }

    #[test]
    fn token_compare() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abd", "abc"));
        assert!(!token_matches("ab", "abc"));
        assert!(!token_matches("", "abc"));
    }

    #[test]
    fn only_this_loopback_server_is_a_valid_host() {
        for ok in ["127.0.0.1", "127.0.0.1:8545", "localhost", "LocalHost:8545"] {
            assert!(host_allowed(Some(ok), 8545), "{ok}");
        }
        for bad in [
            "127.0.0.1:8546",
            "localhost:",
            "evil.example",
            "evil.example:8545",
            "localhost.evil.example",
            "127.0.0.2",
            "[::1]",
            "[::1]:8545",
            "0.0.0.0:8545",
            "",
        ] {
            assert!(!host_allowed(Some(bad), 8545), "{bad}");
        }
        assert!(!host_allowed(None, 8545));
    }
}
