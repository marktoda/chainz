//! A pinned proxy: one loopback URL, `http://127.0.0.1:<port>/<token>`,
//! that forwards every JSON-RPC body to **one** endpoint and nowhere else.
//!
//! This is what a fork needs: anvil fetches state lazily, so every request
//! must reach the same node, and anvil's argv (visible in `ps`) must carry
//! no endpoint secret. Retries stay on that endpoint, under the
//! [`RetryPolicy`] given; there is no failover, no rate-limit smoothing and
//! no inspector.
//!
//! The same loopback rules as the gateway apply ([`crate::auth`]): the
//! token, a `Host` naming this server, `POST` with `application/json`, no
//! CORS headers.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chainz_rpc::{ErrorClass, RetryPolicy, RpcError, Target, Upstream};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::header::{ALLOW, CONTENT_TYPE, HOST, HeaderValue, RETRY_AFTER};
use hyper::{Method, Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::auth;
use crate::http::{self, BoxBody, is_json, json, plain, retry_after_secs};

/// Largest request body accepted.
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;
/// How long in-flight requests may take to finish on shutdown.
const DRAIN: Duration = Duration::from_secs(5);

/// A running pinned proxy. [`shutdown`](Self::shutdown) stops it cleanly;
/// dropping it stops it at once.
pub struct PinnedProxy {
    port: u16,
    token: String,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PinnedProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedProxy")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

struct State {
    upstream: Arc<dyn Upstream>,
    target: Target,
    policy: RetryPolicy,
    token: String,
    port: u16,
}

impl PinnedProxy {
    /// Bind a free loopback port and serve: each request body goes to
    /// `target` through `upstream`, retried there per `policy`. The
    /// proxy's tasks live on the runtime that calls this.
    pub async fn start(
        upstream: Arc<dyn Upstream>,
        target: Target,
        policy: RetryPolicy,
    ) -> io::Result<PinnedProxy> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let port = listener.local_addr()?.port();
        let token = auth::new_token();
        let state = Arc::new(State {
            upstream,
            target,
            policy,
            token: token.clone(),
            port,
        });
        let (stop, stopped) = watch::channel(false);
        let handler = move |request| {
            let state = state.clone();
            async move { handle(&state, request).await }
        };
        let task = tokio::spawn(http::serve_tcp(listener, handler, stopped, DRAIN));
        Ok(PinnedProxy {
            port,
            token,
            stop,
            task: Some(task),
        })
    }

    /// The URL to hand out (for example to `anvil --fork-url`). Contains
    /// the token: treat it as a secret from other local users.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, self.token)
    }

    /// The loopback port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop accepting, let in-flight requests finish (up to 5 s), and wait
    /// until every task has ended.
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for PinnedProxy {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// One request: auth, then forwarding.
async fn handle(state: &State, request: Request<Incoming>) -> Response<BoxBody> {
    let host = request.headers().get(HOST).and_then(|h| h.to_str().ok());
    let path = request.uri().path();
    let token = path.strip_prefix('/').unwrap_or(path).trim_end_matches('/');
    if !auth::host_allowed(host, state.port) || !auth::token_matches(token, &state.token) {
        return plain(StatusCode::FORBIDDEN, "forbidden\n");
    }
    if request.method() != Method::POST {
        let mut response = plain(StatusCode::METHOD_NOT_ALLOWED, "POST only\n");
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("POST"));
        return response;
    }
    if !is_json(request.headers().get(CONTENT_TYPE)) {
        return plain(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content-type must be application/json\n",
        );
    }
    let body = match Limited::new(request.into_body(), MAX_REQUEST_BODY)
        .collect()
        .await
    {
        Ok(body) => body.to_bytes(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large\n"),
    };
    match state
        .upstream
        .send(&state.target, body, &state.policy)
        .await
    {
        Ok(response) => json(StatusCode::OK, response.body),
        // An HTTP error, so a client (anvil) treats it as a transport
        // failure and never as chain state.
        Err(error) => failure(&error),
    }
}

fn failure(error: &RpcError) -> Response<BoxBody> {
    let status = match error.class() {
        ErrorClass::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ErrorClass::Timeout => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::BAD_GATEWAY,
    };
    // `RpcError`'s text never includes a URL or a header value.
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {"code": -32000, "message": format!("chainz proxy: upstream {error}")},
    });
    let mut response = json(status, Bytes::from(body.to_string()));
    if let RpcError::RateLimited {
        retry_after: Some(wait),
    } = error
    {
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(retry_after_secs(*wait)));
    }
    response
}
