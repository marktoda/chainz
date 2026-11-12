//! The error type shared by every RPC operation, and its coarse class.
//!
//! No variant ever carries an endpoint URL or a header value.

use std::time::Duration;

/// The JSON-RPC "limit exceeded" error code, which providers use for rate
/// limits inside a 2xx answer.
pub const LIMIT_EXCEEDED: i64 = -32005;

/// An error from talking to one JSON-RPC endpoint.
///
/// The `Display` text is safe to show to users and to log: it never includes
/// the endpoint URL (which may embed an API key) or any header value.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RpcError {
    /// The request (or the probe deadline) timed out.
    #[error("request timed out")]
    Timeout,
    /// The connection could not be established (DNS, refused, TLS, ...).
    #[error("could not connect to endpoint")]
    Connect,
    /// The endpoint answered HTTP 429.
    #[error("rate limited{}", fmt_retry_after(.retry_after))]
    RateLimited {
        /// The parsed `Retry-After` header (delta-seconds form), if any.
        retry_after: Option<Duration>,
    },
    /// The endpoint answered a non-2xx HTTP status other than 429.
    #[error("HTTP status {0}")]
    Http(u16),
    /// The endpoint returned a JSON-RPC error object.
    #[error("RPC error {code}: {message}")]
    Rpc {
        /// The JSON-RPC error code.
        code: i64,
        /// The JSON-RPC error message, as sent by the server.
        message: String,
    },
    /// The endpoint serves a different chain than expected.
    #[error("chain ID mismatch: expected {expected}, endpoint reports {actual}")]
    ChainMismatch {
        /// The chain ID the caller expected.
        expected: u64,
        /// The chain ID the endpoint reported.
        actual: u64,
    },
    /// The response could not be understood. The text never includes the URL.
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    /// The connection died while the response body was being read: the
    /// endpoint answered, but the answer never arrived whole.
    #[error("invalid response: could not read response body")]
    BodyRead,
    /// The endpoint definition itself is unusable (bad URL, bad header).
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    /// The operation is not supported for this endpoint (e.g. websocket
    /// endpoints for HTTP-only operations).
    #[error("unsupported: {0}")]
    Unsupported(String),
}

fn fmt_retry_after(retry_after: &Option<Duration>) -> String {
    match retry_after {
        Some(d) => format!(" (retry after {}s)", d.as_secs()),
        None => String::new(),
    }
}

impl RpcError {
    /// The coarse class of this error, used for health records and metrics.
    pub fn class(&self) -> ErrorClass {
        match self {
            RpcError::Timeout => ErrorClass::Timeout,
            RpcError::Connect => ErrorClass::Connect,
            RpcError::RateLimited { .. } => ErrorClass::RateLimited,
            RpcError::Http(_) => ErrorClass::Http,
            RpcError::Rpc { .. } => ErrorClass::Rpc,
            RpcError::ChainMismatch { .. } => ErrorClass::ChainMismatch,
            RpcError::InvalidResponse(_) | RpcError::BodyRead | RpcError::InvalidEndpoint(_) => {
                ErrorClass::Invalid
            }
            RpcError::Unsupported(_) => ErrorClass::Unsupported,
        }
    }

    /// Whether the endpoint failed in a way that says nothing about the
    /// request itself, so asking again (there or elsewhere) may succeed: a
    /// timeout, a connection error (including a body cut off mid-read) or
    /// HTTP 502, 503 or 504. Rate limits are [`is_rate_limited`](Self::is_rate_limited).
    ///
    /// This is the one definition of "transient": same-endpoint retries
    /// ([`RetryPolicy`](crate::upstream::RetryPolicy)) and the gateway's
    /// failover both use it.
    pub fn is_transient(&self) -> bool {
        match self {
            RpcError::Timeout | RpcError::Connect | RpcError::BodyRead => true,
            RpcError::Http(status) => matches!(status, 502..=504),
            _ => false,
        }
    }

    /// Whether the endpoint asked the caller to slow down: HTTP 429, or
    /// JSON-RPC [`LIMIT_EXCEEDED`].
    pub fn is_rate_limited(&self) -> bool {
        matches!(
            self,
            RpcError::RateLimited { .. }
                | RpcError::Rpc {
                    code: LIMIT_EXCEEDED,
                    ..
                }
        )
    }

    /// Classify a `reqwest` transport error without ever formatting it
    /// (its `Display` includes the URL).
    pub(crate) fn from_reqwest(err: &reqwest::Error) -> Self {
        if err.is_timeout() {
            RpcError::Timeout
        } else if err.is_connect() {
            RpcError::Connect
        } else if err.is_decode() || err.is_body() {
            RpcError::BodyRead
        } else if err.is_builder() {
            RpcError::InvalidEndpoint("could not build request".into())
        } else {
            // Request errors after connecting (reset, protocol errors, ...).
            RpcError::Connect
        }
    }
}

/// The coarse class of an [`RpcError`], stable for serialization.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// See [`RpcError::Timeout`].
    Timeout,
    /// See [`RpcError::Connect`].
    Connect,
    /// See [`RpcError::RateLimited`].
    RateLimited,
    /// See [`RpcError::Http`].
    Http,
    /// See [`RpcError::Rpc`].
    Rpc,
    /// See [`RpcError::ChainMismatch`].
    ChainMismatch,
    /// [`RpcError::InvalidResponse`] or [`RpcError::InvalidEndpoint`].
    Invalid,
    /// See [`RpcError::Unsupported`].
    Unsupported,
}

impl ErrorClass {
    /// A short stable lowercase label, matching the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::Timeout => "timeout",
            ErrorClass::Connect => "connect",
            ErrorClass::RateLimited => "rate_limited",
            ErrorClass::Http => "http",
            ErrorClass::Rpc => "rpc",
            ErrorClass::ChainMismatch => "chain_mismatch",
            ErrorClass::Invalid => "invalid",
            ErrorClass::Unsupported => "unsupported",
        }
    }
}

impl ErrorClass {
    /// The class as words for a sentence ("the endpoint: connection
    /// failed"), where [`as_str`](Self::as_str) is the stable label.
    pub fn phrase(self) -> &'static str {
        match self {
            ErrorClass::Timeout => "timed out",
            ErrorClass::Connect => "connection failed",
            ErrorClass::RateLimited => "rate limited",
            ErrorClass::Http => "HTTP error",
            ErrorClass::Rpc => "RPC error",
            ErrorClass::ChainMismatch => "serves another chain",
            ErrorClass::Invalid => "invalid response",
            ErrorClass::Unsupported => "unsupported",
        }
    }
}

impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_mapping() {
        assert_eq!(RpcError::Timeout.class(), ErrorClass::Timeout);
        assert_eq!(
            RpcError::RateLimited { retry_after: None }.class(),
            ErrorClass::RateLimited
        );
        assert_eq!(
            RpcError::InvalidEndpoint("x".into()).class(),
            ErrorClass::Invalid
        );
        assert_eq!(
            serde_json::to_string(&ErrorClass::ChainMismatch).unwrap(),
            "\"chain_mismatch\""
        );
    }

    #[test]
    fn transient_and_rate_limited_are_the_one_definition() {
        for e in [
            RpcError::Timeout,
            RpcError::Connect,
            RpcError::BodyRead,
            RpcError::Http(502),
            RpcError::Http(503),
            RpcError::Http(504),
        ] {
            assert!(e.is_transient(), "{e:?}");
            assert!(!e.is_rate_limited(), "{e:?}");
        }
        for e in [
            RpcError::Http(500),
            RpcError::Http(404),
            RpcError::InvalidResponse("x".into()),
            RpcError::ChainMismatch {
                expected: 1,
                actual: 2,
            },
            RpcError::RateLimited { retry_after: None },
        ] {
            assert!(!e.is_transient(), "{e:?}");
        }
        assert!(RpcError::RateLimited { retry_after: None }.is_rate_limited());
        assert!(
            RpcError::Rpc {
                code: LIMIT_EXCEEDED,
                message: "slow down".into()
            }
            .is_rate_limited()
        );
        assert!(
            !RpcError::Rpc {
                code: 3,
                message: "execution reverted".into()
            }
            .is_rate_limited()
        );
        // A cut-off body keeps its old class and text on the wire.
        assert_eq!(RpcError::BodyRead.class(), ErrorClass::Invalid);
        assert_eq!(
            RpcError::BodyRead.to_string(),
            "invalid response: could not read response body"
        );
    }

    #[test]
    fn display_text() {
        let e = RpcError::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        };
        assert_eq!(e.to_string(), "rate limited (retry after 3s)");
        assert_eq!(RpcError::Http(503).to_string(), "HTTP status 503");
    }
}
