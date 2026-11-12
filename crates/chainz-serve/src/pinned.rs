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
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use chainz_core::runtime::{GatewayAddr, LOOPBACK};
use chainz_rpc::{HttpUpstream, RetryPolicy, Target};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::auth;
use crate::http::{self, BoxBody, Reply, json};

/// How long in-flight requests may take to finish on shutdown.
const DRAIN: Duration = Duration::from_secs(5);

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
    policy: RetryPolicy,
    addr: GatewayAddr,
}

impl PinnedProxy {
    /// Bind a free loopback port and serve: each request body goes to
    /// `target` through `upstream`, retried there per `policy`. The
    /// proxy's tasks live on the runtime that calls this.
    pub async fn start(
        upstream: HttpUpstream,
        target: Target,
        policy: RetryPolicy,
    ) -> io::Result<PinnedProxy> {
        let listener = TcpListener::bind(SocketAddr::from((LOOPBACK, 0))).await?;
        let addr = GatewayAddr {
            port: listener.local_addr()?.port(),
            token: auth::new_token(),
        };
        let state = Arc::new(State {
            upstream,
            target,
            policy,
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

    /// The loopback port.
    pub fn port(&self) -> u16 {
        self.addr.port
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
    match state
        .upstream
        .send(&state.target, body, &state.policy)
        .await
    {
        Ok(body) => json(StatusCode::OK, body),
        // An HTTP error, so a client (anvil) treats it as a transport
        // failure and never as chain state.
        Err(error) => {
            let message = format!("chainz proxy: upstream {error}");
            Reply::failure(&error, &serde_json::Value::Null, message).into_response()
        }
    }
}
