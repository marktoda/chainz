//! A resolved endpoint ready to be called.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::error::RpcError;

/// A resolved endpoint: URL and headers with all `${VAR}` already expanded.
///
/// `Debug` redacts the URL and all header values (they may embed API keys);
/// only the chain ID, the endpoint name, the URL scheme and header names are
/// shown.
#[derive(Clone)]
pub struct Target {
    chain_id: u64,
    name: String,
    url: url::Url,
    headers: HeaderMap,
}

impl Target {
    /// The URL schemes an endpoint may use.
    pub const SCHEMES: [&str; 4] = ["http", "https", "ws", "wss"];

    /// Build a target from a URL string and `(name, value)` header pairs.
    ///
    /// The URL must use one of [`Target::SCHEMES`]. Errors never echo the
    /// URL or a header value; an invalid header *value* names its header.
    pub fn new(
        chain_id: u64,
        name: impl Into<String>,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Self, RpcError> {
        let url = url::Url::parse(url.trim())
            .map_err(|_| RpcError::InvalidEndpoint("invalid URL".into()))?;
        if !Self::SCHEMES.contains(&url.scheme()) {
            return Err(RpcError::InvalidEndpoint(
                "unsupported URL scheme (expected http, https, ws or wss)".into(),
            ));
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err(RpcError::InvalidEndpoint("invalid URL".into()));
        }
        let mut map = HeaderMap::with_capacity(headers.len());
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.trim().as_bytes())
                .map_err(|_| RpcError::InvalidEndpoint("invalid header name".into()))?;
            let mut value = HeaderValue::from_str(v).map_err(|_| {
                RpcError::InvalidEndpoint(format!("invalid value for header '{name}'"))
            })?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        Ok(Target {
            chain_id,
            name: name.into(),
            url,
            headers: map,
        })
    }

    /// The chain ID this endpoint is expected to serve.
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// The endpoint's name (e.g. `infura`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The full endpoint URL. Treat as secret: it may embed an API key.
    pub fn url(&self) -> &url::Url {
        &self.url
    }

    /// The request headers. Values are marked sensitive; treat as secret.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Whether the URL scheme is `ws://` or `wss://`.
    pub fn is_websocket(&self) -> bool {
        matches!(self.url.scheme(), "ws" | "wss")
    }

    /// `Err(Unsupported)` for websocket targets, which HTTP operations can't use.
    pub(crate) fn require_http(&self) -> Result<(), RpcError> {
        if self.is_websocket() {
            Err(RpcError::Unsupported(
                "websocket endpoints are not supported for HTTP requests".into(),
            ))
        } else {
            Ok(())
        }
    }
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header_names: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        f.debug_struct("Target")
            .field("chain_id", &self.chain_id)
            .field("name", &self.name)
            .field(
                "url",
                &format_args!("<redacted {}:// URL>", self.url.scheme()),
            )
            .field("headers", &header_names)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET_URL: &str = "https://mainnet.example.io/v3/sekritkey123";

    #[test]
    fn debug_redacts_url_and_header_values() {
        let t = Target::new(
            1,
            "infura",
            SECRET_URL,
            &[("X-Api-Key".into(), "hdrsecret456".into())],
        )
        .unwrap();
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("sekritkey123"), "{dbg}");
        assert!(!dbg.contains("mainnet.example.io"), "{dbg}");
        assert!(!dbg.contains("hdrsecret456"), "{dbg}");
        assert!(dbg.contains("x-api-key"), "{dbg}");
        assert!(dbg.contains("infura"), "{dbg}");
        // The header map's own Debug also redacts sensitive values.
        assert!(!format!("{:?}", t.headers()).contains("hdrsecret456"));
        assert_eq!(t.headers()["x-api-key"], "hdrsecret456");
    }

    #[test]
    fn invalid_inputs_do_not_echo_secrets() {
        let e = Target::new(1, "a", "not a url sekritkey123", &[]).unwrap_err();
        assert_eq!(e, RpcError::InvalidEndpoint("invalid URL".into()));
        assert!(!format!("{e} {e:?}").contains("sekritkey123"));

        let e = Target::new(1, "a", "ftp://host/sekritkey123", &[]).unwrap_err();
        assert!(!format!("{e} {e:?}").contains("sekritkey123"));

        let e = Target::new(
            1,
            "a",
            SECRET_URL,
            &[("x-foo".into(), "bad\nsekrit".into())],
        )
        .unwrap_err();
        assert_eq!(
            e,
            RpcError::InvalidEndpoint("invalid value for header 'x-foo'".into())
        );
        assert!(!format!("{e} {e:?}").contains("sekrit"));

        let e = Target::new(1, "a", SECRET_URL, &[("bad name".into(), "v".into())]).unwrap_err();
        assert!(matches!(e, RpcError::InvalidEndpoint(_)));
    }

    #[test]
    fn websocket_detection() {
        assert!(
            Target::new(1, "a", "wss://h/x", &[])
                .unwrap()
                .is_websocket()
        );
        assert!(Target::new(1, "a", "ws://h/x", &[]).unwrap().is_websocket());
        assert!(
            !Target::new(1, "a", "http://h/x", &[])
                .unwrap()
                .is_websocket()
        );
    }
}
