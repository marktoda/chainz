//! A pinned proxy: one loopback URL, `http://127.0.0.1:<port>/<token>`,
//! that forwards every JSON-RPC body to **one** endpoint and nowhere else.
//!
//! This is what a fork needs: anvil fetches state lazily, so every request
//! must reach the same node, and anvil's argv (visible in `ps`) must carry
//! no endpoint secret. Retries stay on that endpoint
//! ([`RetryPolicy::same_endpoint_transient`], a `-32005` answer included);
//! there is no failover, no rate-limit smoothing and no inspector.
//!
//! The same loopback rules as the gateway apply: the
//! token, a `Host` naming this server, `POST` with `application/json`, no
//! CORS headers.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use chainz_core::runtime::{GatewayAddr, LOOPBACK};
use chainz_rpc::error::LIMIT_EXCEEDED;
use chainz_rpc::upstream::rpc_error_code;
use chainz_rpc::{HttpUpstream, RetryPolicy, RpcClient, RpcError, Target};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::auth;
use crate::http::{self, BoxBody, DRAIN, PINNED, Reply, json};

/// A running pinned proxy. [`shutdown`](Self::shutdown) stops it cleanly;
/// dropping it stops it at once.
pub struct PinnedProxy {
    addr: GatewayAddr,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PinnedProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedProxy")
            .field("port", &self.addr.port)
            .finish_non_exhaustive()
    }
}

struct State {
    upstream: HttpUpstream,
    target: Target,
    addr: GatewayAddr,
}

impl PinnedProxy {
    /// Bind a free loopback port and serve: each request body goes to
    /// `target` through `client`'s pool, and is retried there. The proxy's
    /// tasks live on the runtime that calls this.
    pub async fn start(client: RpcClient, target: Target) -> io::Result<PinnedProxy> {
        let listener = TcpListener::bind(SocketAddr::from((LOOPBACK, 0))).await?;
        let addr = GatewayAddr {
            port: listener.local_addr()?.port(),
            token: auth::new_token(),
        };
        let state = Arc::new(State {
            upstream: HttpUpstream::new(client),
            target,
            addr: addr.clone(),
        });
        let (stop, stopped) = watch::channel(false);
        let handler = move |request| {
            let state = state.clone();
            async move { handle(&state, request).await }
        };
        let task = tokio::spawn(http::serve(listener, handler, stopped, DRAIN));
        Ok(PinnedProxy {
            addr,
            stop,
            task: Some(task),
        })
    }

    /// The URL to hand out (for example to `anvil --fork-url`). Contains
    /// the token: treat it as a secret from other local users.
    pub fn url(&self) -> String {
        self.addr.base_url()
    }

    /// Port and token: the proxy's URL. The token is a secret.
    pub fn addr(&self) -> &GatewayAddr {
        &self.addr
    }

    /// Stop accepting, let in-flight requests finish (for the gateway's
    /// default drain time), and wait until every task has ended.
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
    // Only `/<token>`: a chain or endpoint segment means nothing here.
    let authorize = |path: &str| {
        let token = GatewayAddr::parse_path(path)
            .filter(|path| path.chain.is_none())
            .map(|path| path.token);
        match token.is_some_and(|token| auth::token_matches(token, &state.addr.token)) {
            true => Ok(()),
            false => Err(http::forbidden()),
        }
    };
    let body = match http::guard(request, state.addr.port, authorize).await {
        Ok(((), body)) => body,
        Err(response) => return *response,
    };
    match forward(state, body).await {
        Ok(body) => json(StatusCode::OK, body),
        // An HTTP error, so a client (anvil) treats it as a transport
        // failure and never as chain state.
        Err(error) => {
            Reply::failure(&error, &serde_json::Value::Null, PINNED, "upstream").into_response()
        }
    }
}

/// Send `body` to the endpoint, retrying there on transient failures and
/// rate limits, `-32005` answers included. When the retries run out on a
/// `-32005`, that answer is returned as it was sent.
async fn forward(state: &State, body: Bytes) -> Result<Bytes, RpcError> {
    let limited: Mutex<Option<Bytes>> = Mutex::new(None);
    let (body, limited_ref) = (&body, &limited);
    let result = RetryPolicy::same_endpoint_transient()
        .run(move || async move {
            let answer = state.upstream.send(&state.target, body.clone()).await;
            let mut limited = limited_ref.lock().unwrap_or_else(|e| e.into_inner());
            *limited = None;
            let answer = answer?;
            if rpc_error_code(&answer) == Some(LIMIT_EXCEEDED) {
                *limited = Some(answer);
                return Err(RpcError::RateLimited { retry_after: None });
            }
            Ok(answer)
        })
        .await;
    let limited = limited.into_inner().unwrap_or_else(|e| e.into_inner());
    match (result, limited) {
        (Err(_), Some(answer)) => Ok(answer),
        (result, _) => result,
    }
}
