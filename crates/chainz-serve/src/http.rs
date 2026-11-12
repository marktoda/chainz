//! HTTP plumbing shared by the gateway and the pinned proxy: response
//! helpers, the JSON-RPC replies both proxies send when no upstream
//! answered, and the HTTP/1.1 server every listener runs, which drains on
//! shutdown.

use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use chainz_rpc::RpcError;
use chainz_rpc::error::{LIMIT_EXCEEDED, SERVER_ERROR};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{ALLOW, CONTENT_TYPE, HOST, HeaderValue, RETRY_AFTER};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use serde_json::{Value, json};

use crate::auth;

/// The `Retry-After` sent with a 429 when the upstream named no wait.
pub(crate) const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(2);

/// How long open connections get to finish once a server stops: the pinned
/// proxy's drain, and the gateway's by default.
pub(crate) const DRAIN: Duration = Duration::from_secs(5);

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

/// Who made an error reply: the prefix of its message.
pub(crate) const GATEWAY: &str = "chainz serve";
/// The pinned proxy's prefix.
pub(crate) const PINNED: &str = "chainz proxy";

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
        Reply::rpc_error(status, id, code, format!("{GATEWAY}: {message}"))
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
    /// so a client never mistakes it for chain state. The message is
    /// `<who>: <upstream>: <error>`, with `who` [`GATEWAY`] or [`PINNED`];
    /// [`RpcError`]'s text never includes a URL or a header value.
    pub fn failure(error: &RpcError, id: &Value, who: &str, upstream: &str) -> Reply {
        let (status, code) = match error {
            RpcError::RateLimited { .. } => (429, LIMIT_EXCEEDED),
            RpcError::Timeout => (504, SERVER_ERROR),
            RpcError::Http(status) if (400..600).contains(status) => (*status, SERVER_ERROR),
            _ => (502, SERVER_ERROR),
        };
        let message = format!("{who}: {upstream}: {error}");
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

/// Largest request body either proxy accepts.
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

/// The checks every proxied request passes, in order: a loopback `Host`
/// naming `port` (403), `POST` (405), `application/json` (415), then
/// `authorize(path)` (its own answer, usually 403), then a body of at most
/// 64 MiB (413). No CORS headers are ever sent.
pub(crate) async fn guard<T>(
    request: Request<Incoming>,
    port: u16,
    authorize: impl FnOnce(&str) -> Result<T, Refusal>,
) -> Result<(T, Bytes), Refusal> {
    let host = request.headers().get(HOST).and_then(|h| h.to_str().ok());
    if !auth::host_allowed(host, port) {
        return Err(forbidden());
    }
    if request.method() != Method::POST {
        let mut response = plain(StatusCode::METHOD_NOT_ALLOWED, "POST only\n");
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("POST"));
        return Err(Box::new(response));
    }
    if !is_json(request.headers().get(CONTENT_TYPE)) {
        return Err(Box::new(plain(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content-type must be application/json\n",
        )));
    }
    let authorized = authorize(request.uri().path())?;
    match Limited::new(request.into_body(), MAX_REQUEST_BODY)
        .collect()
        .await
    {
        Ok(body) => Ok((authorized, body.to_bytes())),
        Err(_) => Err(Box::new(plain(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too large\n",
        ))),
    }
}

/// The answer to a request [`guard`] turned away.
pub(crate) type Refusal = Box<Response<BoxBody>>;

/// The answer to a request that isn't allowed in.
pub(crate) fn forbidden() -> Refusal {
    Box::new(plain(StatusCode::FORBIDDEN, "forbidden\n"))
}

/// Whether a `Content-Type` is `application/json` (parameters allowed).
fn is_json(content_type: Option<&HeaderValue>) -> bool {
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

/// A socket that hands out connections: the gateway's TCP port, the
/// pinned proxy's, and the control socket.
pub(crate) trait Listener: Send + 'static {
    /// One accepted connection.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    /// The next connection.
    fn accept(&self) -> impl Future<Output = std::io::Result<Self::Stream>> + Send;
}

impl Listener for TcpListener {
    type Stream = TcpStream;
    async fn accept(&self) -> std::io::Result<TcpStream> {
        let (stream, _) = TcpListener::accept(self).await?;
        let _ = stream.set_nodelay(true);
        Ok(stream)
    }
}

impl Listener for UnixListener {
    type Stream = UnixStream;
    async fn accept(&self) -> std::io::Result<UnixStream> {
        Ok(UnixListener::accept(self).await?.0)
    }
}

/// Serve HTTP/1.1 on `listener` with `handler` until `stopped`, then let
/// open connections finish for up to `drain`, cut off the rest, and wait
/// until every connection task has ended.
pub(crate) async fn serve<L, H, F>(
    listener: L,
    handler: H,
    mut stopped: watch::Receiver<bool>,
    drain: Duration,
) where
    L: Listener,
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
                let Ok(stream) = accepted else {
                    // Out of descriptors, most likely: don't spin.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
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
    let _ = tokio::time::timeout(drain, graceful.shutdown()).await;
    connections.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_map_the_same_for_both_proxies() {
        let id = json!(7);
        let reply = |e: RpcError| Reply::failure(&e, &id, PINNED, "upstream");
        let limited = reply(RpcError::RateLimited { retry_after: None });
        assert_eq!(limited.status, 429);
        assert_eq!(limited.retry_after, Some(DEFAULT_RETRY_AFTER));
        let body: Value = serde_json::from_slice(&limited.body).unwrap();
        assert_eq!(
            (body["id"].clone(), body["error"]["code"].clone()),
            (id.clone(), json!(LIMIT_EXCEEDED))
        );
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
