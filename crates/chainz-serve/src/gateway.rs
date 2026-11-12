//! The running gateway: listeners, background tasks and lifecycle.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use chainz_core::StorePaths;
use chainz_core::runtime::{Runtime, ServeInfo, ServeLock};
use chainz_rpc::control::{
    ChainStatus, EVENTS_PATH, EventsOptions, METRICS_PATH, RELOAD_PATH, RequestRecord, STATUS_PATH,
    SocketPath, Status,
};
use chainz_rpc::{HttpUpstream, RpcClient, Upstream};
use http_body_util::{BodyExt, Limited};
use hyper::body::{Body, Frame, Incoming};
use hyper::header::{ALLOW, CONTENT_TYPE, HOST, HeaderValue, RETRY_AFTER};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use serde_json::Value;
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{broadcast, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::auth;
use crate::http::{self, BoxBody, is_json, json, plain, retry_after_secs};
use crate::inspector::Inspector;
use crate::metrics::Summary;
use crate::router::{Reply, Router};
use crate::state::Snapshot;
use chainz_sys::{FileStamp, now_secs};

/// The default gateway port (8545 stays free for anvil).
pub const DEFAULT_PORT: u16 = 18545;
/// Largest request body accepted.
const MAX_REQUEST_BODY: usize = 32 * 1024 * 1024;
/// A chain with no request for this many seconds is idle: its pool is
/// probed [`IDLE_PROBE_FACTOR`] times less often.
const PROBE_IDLE_AFTER: u64 = 300;
/// How much slower idle pools are probed (every minute at the default 5 s).
const IDLE_PROBE_FACTOR: u32 = 12;

/// How to start a [`Gateway`].
pub struct GatewayConfig {
    /// Where the config lives.
    pub paths: StorePaths,
    /// The runtime registry (`serve.lock`, `serve.json`, `serve.sock`).
    pub runtime: Runtime,
    /// The state dir (capability tags); `None` to skip.
    pub state_dir: Option<PathBuf>,
    /// Loopback port; 0 picks a free one.
    pub port: u16,
    /// Append request records to this file as JSONL.
    pub log: Option<PathBuf>,
    /// Where requests are forwarded.
    pub upstream: Arc<dyn Upstream>,
    /// Client for head probes.
    pub client: RpcClient,
    /// Per-attempt upstream timeout.
    pub timeout: Duration,
    /// How often each failover pool's heads are probed.
    pub probe_interval: Duration,
    /// How often the config files are checked for changes.
    pub watch_interval: Duration,
    /// How long in-flight requests may take to finish on shutdown.
    pub drain: Duration,
}

impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl GatewayConfig {
    /// The production defaults: 30 s upstream timeout, probes every 5 s,
    /// config checked every 500 ms, 5 s drain.
    pub fn new(paths: StorePaths, runtime: Runtime, state_dir: Option<PathBuf>) -> Self {
        let timeout = Duration::from_secs(30);
        let client = RpcClient::with_timeout(timeout);
        GatewayConfig {
            paths,
            runtime,
            state_dir,
            port: DEFAULT_PORT,
            log: None,
            upstream: Arc::new(HttpUpstream::new(client.clone())),
            client,
            timeout,
            probe_interval: Duration::from_secs(5),
            watch_interval: Duration::from_millis(500),
            drain: Duration::from_secs(5),
        }
    }
}

/// State shared by every task.
struct Shared {
    router: Router,
    paths: StorePaths,
    state_dir: Option<PathBuf>,
    token: String,
    port: u16,
    started: Instant,
    /// Human-readable notices (reloads, warnings) for the foreground UI.
    notices: broadcast::Sender<String>,
    /// Reloads (SIGHUP, the control API, the file watch) run one at a
    /// time, so an older read of the config can't replace a newer one.
    reloading: tokio::sync::Mutex<()>,
}

impl Shared {
    fn notice(&self, text: String) {
        let _ = self.notices.send(text);
    }

    async fn reload(&self) -> Result<()> {
        let _one_at_a_time = self.reloading.lock().await;
        let previous = self.router.snapshot();
        let (paths, state_dir, base) =
            (self.paths.clone(), self.state_dir.clone(), previous.clone());
        // Reading and parsing the files blocks.
        let loaded = tokio::task::spawn_blocking(move || {
            Snapshot::load(&paths, state_dir.as_deref(), Some(&base))
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("the reload task failed: {e}")));
        match loaded {
            Ok(snapshot) => {
                let chains = snapshot.chains.len();
                // Only what this reload newly broke.
                for warning in &snapshot.warnings {
                    if !previous.warnings.contains(warning) {
                        self.notice(format!("warning: {warning}"));
                    }
                }
                self.router.set_snapshot(snapshot);
                self.notice(format!("config reloaded ({chains} chains)"));
                Ok(())
            }
            Err(error) => {
                self.notice(format!(
                    "config reload failed, keeping the previous config: {error:#}"
                ));
                Err(error)
            }
        }
    }

    fn status(&self) -> Status {
        let snapshot = self.router.snapshot();
        Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            port: self.port,
            uptime_secs: self.started.elapsed().as_secs(),
            chains: snapshot
                .chains
                .iter()
                .map(|chain| ChainStatus {
                    chain: chain.name.clone(),
                    chain_id: chain.id,
                    active: chain.active_name.clone(),
                    failover: chain.failover_names.clone(),
                    head: chain.head(),
                })
                .collect(),
        }
    }
}

/// A running gateway. Call [`Gateway::shutdown`] to stop it cleanly.
pub struct Gateway {
    shared: Arc<Shared>,
    runtime: Runtime,
    stop: watch::Sender<bool>,
    /// The servers and the background loops; each stops on `stop`.
    tasks: Vec<JoinHandle<()>>,
    log_writer: Option<JoinHandle<()>>,
    drain: Duration,
    _lock: ServeLock,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("port", &self.shared.port)
            .finish_non_exhaustive()
    }
}

impl Gateway {
    /// Take `serve.lock`, load the config, bind the loopback port and the
    /// control socket, publish `serve.json`, and start serving.
    pub async fn start(config: GatewayConfig) -> Result<Gateway> {
        let Some(lock) = config.runtime.acquire_serve_lock()? else {
            let port = config
                .runtime
                .read_serve_info()
                .ok()
                .flatten()
                .map(|info| format!(" on port {}", info.port))
                .unwrap_or_default();
            bail!("chainz serve is already running{port} (stop it with `chainz serve stop`)");
        };
        // Anything left by a gateway that died is stale now, and `serve stop`
        // must never read a dead gateway's PID while we hold the lock.
        let _ = config.runtime.remove_serve_info();
        // Stamped before the load, so the watcher sees a change made while
        // it runs.
        let stamps = Stamps::take(&config.paths, config.state_dir.as_deref());
        let snapshot = Snapshot::load(&config.paths, config.state_dir.as_deref(), None)?;
        let warnings = snapshot.warnings.clone();

        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
        let tcp = TcpListener::bind(address)
            .await
            .with_context(|| format!("can't listen on {address}"))?;
        let port = tcp.local_addr()?.port();

        // We hold the lock, so any socket file here is stale.
        let sock = config.runtime.serve_sock_path();
        match std::fs::remove_file(&sock) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("can't replace {}", sock.display()));
            }
            _ => {}
        }
        let unix = SocketPath::new(&sock)
            .and_then(|short| UnixListener::bind(short.as_path()))
            .with_context(|| format!("can't bind the control socket {}", sock.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;

        let (notices, _) = broadcast::channel(64);
        let (inspector, log_writer) =
            match Inspector::new(config.log.as_deref(), notices.clone()).await {
                Ok(pair) => pair,
                Err(error) => {
                    let _ = std::fs::remove_file(&sock);
                    return Err(error).context("can't open the --log file");
                }
            };
        let shared = Arc::new(Shared {
            router: Router::new(snapshot, config.upstream, inspector, config.timeout),
            paths: config.paths,
            state_dir: config.state_dir,
            token: auth::new_token(),
            port,
            started: Instant::now(),
            notices,
            reloading: tokio::sync::Mutex::new(()),
        });

        // Published before anything is spawned. Both listeners are bound,
        // so a client that reads it early waits in the accept backlog; and
        // a failed write leaves nothing running that holds them.
        let info = ServeInfo {
            pid: std::process::id(),
            port,
            token: shared.token.clone(),
            started_at: now_secs(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        if let Err(error) = config.runtime.write_serve_info(&info) {
            let _ = std::fs::remove_file(&sock);
            shared.router.inspector.close_log();
            if let Some(writer) = log_writer {
                let _ = writer.await;
            }
            return Err(error).context("can't write serve.json");
        }

        let (stop, stopped) = watch::channel(false);
        let tasks = vec![
            tokio::spawn(http::serve_tcp(
                tcp,
                {
                    let shared = shared.clone();
                    move |request| {
                        let shared = shared.clone();
                        async move { proxy(&shared, request).await }
                    }
                },
                stopped.clone(),
                config.drain,
            )),
            tokio::spawn(serve_control(
                unix,
                shared.clone(),
                stopped.clone(),
                config.drain,
            )),
            tokio::spawn(probe_loop(
                shared.clone(),
                config.client,
                config.probe_interval,
                stopped.clone(),
            )),
            tokio::spawn(watch_loop(
                shared.clone(),
                config.watch_interval,
                stamps,
                stopped,
            )),
        ];
        let gateway = Gateway {
            shared,
            runtime: config.runtime,
            stop,
            tasks,
            log_writer,
            drain: config.drain,
            _lock: lock,
        };
        for warning in warnings {
            gateway.shared.notice(format!("warning: {warning}"));
        }
        Ok(gateway)
    }

    /// The loopback port.
    pub fn port(&self) -> u16 {
        self.shared.port
    }

    /// The URL for a chain, optionally pinned to an endpoint. Contains the
    /// auth token: treat as secret.
    pub fn url(&self, chain: &str, endpoint: Option<&str>) -> String {
        let base = format!(
            "http://127.0.0.1:{}/{}/{chain}",
            self.shared.port, self.shared.token
        );
        match endpoint {
            Some(endpoint) => format!("{base}/{endpoint}"),
            None => base,
        }
    }

    /// The current status, as the control API reports it.
    pub fn status(&self) -> Status {
        self.shared.status()
    }

    /// Totals over the last minute.
    pub fn summary(&self) -> Summary {
        self.shared.router.metrics.summary(now_secs())
    }

    /// Notices (reloads, warnings) as they happen.
    pub fn notices(&self) -> broadcast::Receiver<String> {
        self.shared.notices.subscribe()
    }

    /// Unroutable endpoints found when the config was last loaded.
    pub fn warnings(&self) -> Vec<String> {
        self.shared.router.snapshot().warnings.clone()
    }

    /// Re-read the config now. On error the previous config stays.
    pub async fn reload(&self) -> Result<()> {
        self.shared.reload().await
    }

    /// Stop: withdraw `serve.json`, stop accepting, let in-flight requests
    /// finish (up to the drain time), then remove the socket and release
    /// the lock. Every task the gateway started has ended when this
    /// returns.
    pub async fn shutdown(mut self) {
        let _ = self.runtime.remove_serve_info();
        let _ = self.stop.send(true);
        // Each task stops on the signal (the servers after draining);
        // whatever is still running at the deadline is aborted.
        let deadline = tokio::time::Instant::now() + self.drain + Duration::from_millis(250);
        for mut task in std::mem::take(&mut self.tasks) {
            tokio::select! {
                _ = &mut task => {}
                _ = tokio::time::sleep_until(deadline) => {
                    task.abort();
                    let _ = task.await;
                }
            }
        }
        self.shared.router.inspector.close_log();
        if let Some(mut writer) = self.log_writer.take()
            && tokio::time::timeout(Duration::from_secs(2), &mut writer)
                .await
                .is_err()
        {
            writer.abort();
            let _ = writer.await;
        }
        let _ = std::fs::remove_file(self.runtime.serve_sock_path());
    }
}

/// A gateway dropped without [`Gateway::shutdown`] stops at once: no
/// drain, and its tasks are aborted rather than awaited.
impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        for task in self.tasks.iter().chain(&self.log_writer) {
            task.abort();
        }
    }
}

/// One proxied request: auth, then routing. No CORS headers are ever sent.
async fn proxy(shared: &Shared, request: Request<Incoming>) -> Response<BoxBody> {
    let host = request.headers().get(HOST).and_then(|h| h.to_str().ok());
    if !auth::host_allowed(host, shared.port) {
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
    let path = request.uri().path().to_string();
    let mut segments = path.strip_prefix('/').unwrap_or(&path).split('/');
    let (Some(token), Some(chain_ref), pin, None) = (
        segments.next(),
        segments.next(),
        segments.next().filter(|pin| !pin.is_empty()),
        segments.next(),
    ) else {
        return plain(StatusCode::FORBIDDEN, "forbidden\n");
    };
    if !auth::token_matches(token, &shared.token) {
        return plain(StatusCode::FORBIDDEN, "forbidden\n");
    }
    let snapshot = shared.router.snapshot();
    let Some(chain) = snapshot.chain(chain_ref) else {
        let reply = Reply::error(404, &Value::Null, -32000, "unknown chain");
        return json(StatusCode::NOT_FOUND, reply.body);
    };
    let body = match Limited::new(request.into_body(), MAX_REQUEST_BODY)
        .collect()
        .await
    {
        Ok(body) => body.to_bytes(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large\n"),
    };
    let reply = shared.router.handle(&snapshot, chain, pin, body).await;
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = json(status, reply.body);
    if let Some(wait) = reply.retry_after {
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(retry_after_secs(wait)));
    }
    response
}

/// Serve the control API on the unix socket until stopped.
async fn serve_control(
    listener: UnixListener,
    shared: Arc<Shared>,
    mut stopped: watch::Receiver<bool>,
    drain: Duration,
) {
    let graceful = GracefulShutdown::new();
    let mut builder = http1::Builder::new();
    builder.timer(TokioTimer::new());
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let shared = shared.clone();
                let stopped = stopped.clone();
                let service = service_fn(move |request| {
                    let shared = shared.clone();
                    let stopped = stopped.clone();
                    async move { Ok::<_, Infallible>(control(&shared, request, stopped).await) }
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
    http::drain_connections(graceful, connections, drain).await;
}

async fn control(
    shared: &Arc<Shared>,
    request: Request<Incoming>,
    stopped: watch::Receiver<bool>,
) -> Response<BoxBody> {
    let path = request.uri().path();
    let known = [STATUS_PATH, METRICS_PATH, EVENTS_PATH, RELOAD_PATH].contains(&path);
    match (request.method(), path) {
        (&Method::GET, STATUS_PATH) => json(
            StatusCode::OK,
            serde_json::to_vec(&shared.status()).unwrap_or_default(),
        ),
        (&Method::GET, METRICS_PATH) => {
            let snapshot = shared.router.metrics.snapshot(now_secs());
            json(
                StatusCode::OK,
                serde_json::to_vec(&snapshot).unwrap_or_default(),
            )
        }
        (&Method::POST, RELOAD_PATH) => match shared.reload().await {
            Ok(()) => plain(StatusCode::NO_CONTENT, ""),
            Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "reload failed\n"),
        },
        (&Method::GET, EVENTS_PATH) => {
            let options = EventsOptions::from_query(request.uri().query().unwrap_or(""));
            let (backlog, live) = shared.router.inspector.subscribe(options);
            let mut response =
                Response::new(EventsBody::new(backlog, live, stopped).boxed_unsync());
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson"),
            );
            response
        }
        _ if known => plain(StatusCode::METHOD_NOT_ALLOWED, ""),
        _ => plain(StatusCode::NOT_FOUND, ""),
    }
}

/// The NDJSON events stream: the backlog in one chunk, then live records
/// until the client goes away or the gateway stops. The body drives
/// itself, so it ends with its connection and leaves no task behind.
struct EventsBody {
    backlog: Option<Bytes>,
    next: Option<NextLine>,
}

type NextLine = Pin<Box<dyn Future<Output = Option<(Bytes, Live)>> + Send>>;

/// What a following events stream waits on.
struct Live {
    records: broadcast::Receiver<RequestRecord>,
    stopped: watch::Receiver<bool>,
}

impl EventsBody {
    fn new(
        backlog: Vec<RequestRecord>,
        live: Option<broadcast::Receiver<RequestRecord>>,
        stopped: watch::Receiver<bool>,
    ) -> Self {
        let chunk: Vec<u8> = backlog.iter().flat_map(ndjson).collect();
        EventsBody {
            backlog: (!chunk.is_empty()).then(|| Bytes::from(chunk)),
            next: live.map(|records| Box::pin(next_line(Live { records, stopped })) as NextLine),
        }
    }
}

fn ndjson(record: &RequestRecord) -> Vec<u8> {
    let mut line = serde_json::to_vec(record).unwrap_or_default();
    line.push(b'\n');
    line
}

/// The next live record as a line, or `None` when the stream is over.
async fn next_line(mut live: Live) -> Option<(Bytes, Live)> {
    loop {
        let record = tokio::select! {
            record = live.records.recv() => record,
            _ = live.stopped.changed() => return None,
        };
        match record {
            Ok(record) => return Some((Bytes::from(ndjson(&record)), live)),
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

impl Body for EventsBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if let Some(chunk) = self.backlog.take() {
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        let Some(next) = self.next.as_mut() else {
            return Poll::Ready(None);
        };
        match next.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some((line, live))) => {
                self.next = Some(Box::pin(next_line(live)));
                Poll::Ready(Some(Ok(Frame::data(line))))
            }
            Poll::Ready(None) => {
                self.next = None;
                Poll::Ready(None)
            }
        }
    }
}

/// Probe every failover pool's heads and keep the per-chain high-water
/// mark. Single-endpoint pools need no mark and are never probed. Pools
/// without recent traffic are probed less often, so an idle gateway spends
/// little against provider quotas.
async fn probe_loop(
    shared: Arc<Shared>,
    client: RpcClient,
    interval: Duration,
    mut stopped: watch::Receiver<bool>,
) {
    let idle_interval = interval * IDLE_PROBE_FACTOR;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stopped.changed() => return,
        }
        let now = now_secs();
        let snapshot = shared.router.snapshot();
        let mut round = JoinSet::new();
        // Each probed chain's best head this round.
        let mut best: HashMap<usize, Option<u64>> = HashMap::new();
        for (index, chain) in snapshot.chains.iter().enumerate() {
            if chain.failover.is_empty() || chain.active.is_err() {
                continue;
            }
            let busy = chain
                .last_used()
                .is_some_and(|t| now.saturating_sub(t) < PROBE_IDLE_AFTER);
            if !busy && chain.probed_within(idle_interval) {
                continue;
            }
            chain.mark_probed();
            best.insert(index, None);
            for endpoint in chain.pool() {
                let client = client.clone();
                let chain_id = chain.id;
                round.spawn(async move {
                    let result = chainz_rpc::probe(&client, &endpoint.target, chain_id).await;
                    endpoint.set_head(result.head);
                    (index, result.head)
                });
            }
        }
        // Every probe has its own deadline. A stop cuts the round short
        // and waits for the aborted probes.
        loop {
            let done = tokio::select! {
                done = round.join_next() => done,
                _ = stopped.changed() => {
                    round.shutdown().await;
                    return;
                }
            };
            match done {
                Some(Ok((index, Some(head)))) => {
                    let slot = best.entry(index).or_default();
                    *slot = (*slot).max(Some(head));
                }
                Some(_) => {}
                None => break,
            }
        }
        for (index, head) in best {
            snapshot.chains[index].record_round(head);
        }
    }
}

/// The files the watcher follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamps {
    config: Option<FileStamp>,
    secrets: Option<FileStamp>,
    health: Option<FileStamp>,
}

impl Stamps {
    fn take(paths: &StorePaths, state_dir: Option<&Path>) -> Stamps {
        Stamps {
            config: FileStamp::of(paths.config()),
            secrets: FileStamp::of(paths.secrets()),
            health: state_dir.and_then(|dir| FileStamp::of(&dir.join("health.json"))),
        }
    }
}

/// Reload when `config.toml` or `secrets.toml` changes, and refresh
/// capability tags when the health store changes. `seen` is what the files
/// looked like when the running config was loaded.
async fn watch_loop(
    shared: Arc<Shared>,
    interval: Duration,
    mut seen: Stamps,
    mut stopped: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stopped.changed() => return,
        }
        let now = Stamps::take(&shared.paths, shared.state_dir.as_deref());
        if (now.config, now.secrets) != (seen.config, seen.secrets) {
            let _ = shared.reload().await;
        } else if now.health != seen.health
            && let Some(dir) = shared.state_dir.clone()
        {
            let snapshot = shared.router.snapshot();
            let _ = tokio::task::spawn_blocking(move || snapshot.refresh_capabilities(&dir)).await;
        }
        seen = now;
    }
}
