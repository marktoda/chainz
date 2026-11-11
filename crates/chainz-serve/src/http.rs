//! HTTP plumbing shared by the gateway and the pinned proxy: response
//! helpers and a loopback TCP server that drains on shutdown.

use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

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
