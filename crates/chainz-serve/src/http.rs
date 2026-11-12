//! HTTP plumbing shared by the gateway and the pinned proxy: response
//! helpers, the JSON-RPC replies both proxies send when no upstream
//! answered, and a loopback TCP server that drains on shutdown.

use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use chainz_rpc::RpcError;
use chainz_rpc::error::LIMIT_EXCEEDED;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use serde_json::{Value, json};

/// The `Retry-After` sent with a 429 when the upstream named no wait: as
/// long as the gateway itself waits out rate limits on one endpoint.
pub(crate) const DEFAULT_RETRY_AFTER: Duration = crate::router::MAX_WAIT;

pub(crate) type BoxBody = http_body_util::combinators::UnsyncBoxBody<Bytes, Infallible>;

pub(crate) fn full(body: impl Into<Bytes>) -> BoxBody {
    Full::new(body.into()).boxed_unsync()
}

pub(crate) fn plain(status: StatusCode, text: &'static str) -> Response<BoxBody> {
    let mut response = Response::new(full(text));
    *response.status_mut() = status;
    response
}

pub(crate) fn json(status: StatusCode, body: impl Into<Bytes>) -> Response<BoxBody> {
    let mut response = Response::new(full(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

/// A JSON-RPC answer on its way to the client.
#[derive(Debug, Clone)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// JSON body.
    pub body: Bytes,
    /// `Retry-After`, on a 429.
    pub retry_after: Option<Duration>,
}

impl Reply {
    /// A JSON-RPC error made by the gateway itself.
    pub fn error(status: u16, id: &Value, code: i64, message: &str) -> Reply {
        Reply::rpc_error(status, id, code, format!("chainz serve: {message}"))
    }

    /// A JSON-RPC error object with `message` as it is.
    fn rpc_error(status: u16, id: &Value, code: i64, message: String) -> Reply {
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        });
        Reply {
            status,
            body: Bytes::from(body.to_string()),
            retry_after: None,
        }
    }

    /// The reply for a request no upstream answered, the same from both
    /// proxies: a 429 is `-32005` with a `Retry-After` (the upstream's, or
    /// [`DEFAULT_RETRY_AFTER`]); an upstream 4xx or 5xx status is passed
    /// on; a timeout is 504; anything else is 502. Always an HTTP error,
    /// so a client never mistakes it for chain state. `message` is the
    /// whole error message; [`RpcError`]'s text in it never includes a URL
    /// or a header value.
    pub fn failure(error: &RpcError, id: &Value, message: String) -> Reply {
        let (status, code) = match error {
            RpcError::RateLimited { .. } => (429, LIMIT_EXCEEDED),
            RpcError::Timeout => (504, -32000),
            RpcError::Http(status) if (400..600).contains(status) => (*status, -32000),
            _ => (502, -32000),
        };
        let mut reply = Reply::rpc_error(status, id, code, message);
        if let RpcError::RateLimited { retry_after } = error {
            reply.retry_after = Some(retry_after.unwrap_or(DEFAULT_RETRY_AFTER));
        }
        reply
    }

    /// As an HTTP response.
    pub fn into_response(self) -> Response<BoxBody> {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut response = json(status, self.body);
        if let Some(wait) = self.retry_after {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(retry_after_secs(wait)));
        }
        response
    }
}

/// Whether a `Content-Type` is `application/json` (parameters allowed).
pub(crate) fn is_json(content_type: Option<&HeaderValue>) -> bool {
    content_type
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// Whole seconds for `Retry-After`, rounded up (a client told 2 s for a
/// 2.9 s wait comes back too early), and at least 1.
pub(crate) fn retry_after_secs(wait: Duration) -> u64 {
    (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1)
}

/// Serve HTTP/1.1 on `listener` with `handler` until `stopped`, then let
/// open connections finish for up to `drain`, cut off the rest, and wait
/// until every connection task has ended.
pub(crate) async fn serve_tcp<H, F>(
    listener: TcpListener,
    handler: H,
    mut stopped: watch::Receiver<bool>,
    drain: Duration,
) where
    H: Fn(Request<Incoming>) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = Response<BoxBody>> + Send + 'static,
{
    let graceful = GracefulShutdown::new();
    let mut builder = http1::Builder::new();
    builder.timer(TokioTimer::new());
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    // Out of descriptors, most likely: don't spin.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let _ = stream.set_nodelay(true);
                let handler = handler.clone();
                let service = service_fn(move |request| {
                    let response = handler(request);
                    async move { Ok::<_, Infallible>(response.await) }
                });
                let connection = builder.serve_connection(TokioIo::new(stream), service);
                let connection = graceful.watch(connection);
                connections.spawn(async move {
                    let _ = connection.await;
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            _ = stopped.changed() => break,
        }
    }
    drop(listener);
    drain_connections(graceful, connections, drain).await;
}

/// Let open connections finish their requests for up to `drain`, then
/// cut off the rest and wait until every connection task has ended.
pub(crate) async fn drain_connections(
    graceful: GracefulShutdown,
    mut connections: JoinSet<()>,
    drain: Duration,
) {
    let _ = tokio::time::timeout(drain, graceful.shutdown()).await;
    connections.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_map_the_same_for_both_proxies() {
        let id = json!(7);
        let reply = |e: RpcError| Reply::failure(&e, &id, "x".into());
        let limited = reply(RpcError::RateLimited { retry_after: None });
        assert_eq!(limited.status, 429);
        assert_eq!(limited.retry_after, Some(DEFAULT_RETRY_AFTER));
        let body: Value = serde_json::from_slice(&limited.body).unwrap();
        assert_eq!((body["id"].clone(), body["error"]["code"].clone()), (id.clone(), json!(LIMIT_EXCEEDED)));
        let waited = reply(RpcError::RateLimited {
            retry_after: Some(Duration::from_secs(9)),
        });
        assert_eq!(waited.retry_after, Some(Duration::from_secs(9)));
        assert_eq!(reply(RpcError::Http(404)).status, 404);
        assert_eq!(reply(RpcError::Http(503)).status, 503);
        assert_eq!(reply(RpcError::Http(302)).status, 502);
        assert_eq!(reply(RpcError::Timeout).status, 504);
        assert_eq!(reply(RpcError::Connect).status, 502);
        assert_eq!(reply(RpcError::BodyRead).status, 502);
        assert_eq!(reply(RpcError::Timeout).retry_after, None);
    }

    #[test]
    fn retry_after_rounds_up() {
        let secs = |ms| retry_after_secs(Duration::from_millis(ms));
        assert_eq!(secs(2900), 3, "2.9 s is not 2 s");
        assert_eq!(secs(2001), 3);
        assert_eq!(secs(2000), 2);
        assert_eq!(secs(30_000), 30);
        assert_eq!(secs(1), 1);
        assert_eq!(secs(0), 1, "never 0");
    }
}
