//! The running gateway: listeners, background tasks and lifecycle.

use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use chainz_core::StorePaths;
use chainz_core::runtime::{Runtime, ServeInfo, ServeLock};
use chainz_rpc::control::{
    ChainStatus, EVENTS_PATH, EventsOptions, METRICS_PATH, RELOAD_PATH, STATUS_PATH, SocketPath,
    Status,
};
use chainz_rpc::{HttpUpstream, RpcClient, Upstream};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Frame, Incoming};
use hyper::header::{ALLOW, CONTENT_TYPE, HOST, HeaderValue, RETRY_AFTER};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use serde_json::Value;
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

use crate::inspector::Inspector;
use crate::metrics::Summary;
use crate::router::{Reply, Router, now_ms};
use crate::state::Snapshot;

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
    reloads: AtomicU64,
    /// Human-readable notices (reloads, warnings) for the foreground UI.
    notices: broadcast::Sender<String>,
}

impl Shared {
    fn notice(&self, text: String) {
        let _ = self.notices.send(text);
    }

    fn reload(&self) -> Result<()> {
        let previous = self.router.snapshot();
        match Snapshot::load(&self.paths, self.state_dir.as_deref(), Some(&previous)) {
            Ok(snapshot) => {
                let chains = snapshot.chains.len();
                // Only what this reload newly broke.
                for warning in &snapshot.warnings {
                    if !previous.warnings.contains(warning) {
                        self.notice(format!("warning: {warning}"));
                    }
                }
                self.router.set_snapshot(snapshot);
                self.reloads.fetch_add(1, Ordering::Relaxed);
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
    servers: Vec<JoinHandle<()>>,
    background: Vec<JoinHandle<()>>,
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
    /// control socket, start the background tasks and publish `serve.json`.
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
        }

        let (inspector, log_writer) = match Inspector::new(config.log.as_deref()).await {
            Ok(pair) => pair,
            Err(error) => {
                let _ = std::fs::remove_file(&sock);
                return Err(error).context("can't open the --log file");
            }
        };
        let (notices, _) = broadcast::channel(64);
        let shared = Arc::new(Shared {
            router: Router::new(snapshot, config.upstream, inspector, config.timeout),
            paths: config.paths,
            state_dir: config.state_dir,
            token: new_token(),
            port,
            started: Instant::now(),
            reloads: AtomicU64::new(0),
            notices,
        });
        let (stop, stopped) = watch::channel(false);

        let servers = vec![
            tokio::spawn(serve_tcp(
                tcp,
                shared.clone(),
                stopped.clone(),
                config.drain,
            )),
            tokio::spawn(serve_control(
                unix,
                shared.clone(),
                stopped.clone(),
                config.drain,
            )),
        ];
        let background = vec![
            tokio::spawn(probe_loop(
                shared.clone(),
                config.client,
                config.probe_interval,
            )),
            tokio::spawn(watch_loop(shared.clone(), config.watch_interval)),
        ];

        // Published last: `exec` never sees a URL that isn't accepting yet.
        let info = ServeInfo {
            pid: std::process::id(),
            port,
            token: shared.token.clone(),
            started_at: now_ms() / 1000,
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        if let Err(error) = config.runtime.write_serve_info(&info) {
            let _ = std::fs::remove_file(&sock);
            return Err(error).context("can't write serve.json");
        }
        let gateway = Gateway {
            shared,
            runtime: config.runtime,
            stop,
            servers,
            background,
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
        self.shared.router.metrics.summary(now_ms() / 1000)
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
    pub fn reload(&self) -> Result<()> {
        self.shared.reload()
    }

    /// Stop: withdraw `serve.json`, stop accepting, let in-flight requests
    /// finish (up to the drain time), then remove the socket and release
    /// the lock.
    pub async fn shutdown(self) {
        let _ = self.runtime.remove_serve_info();
        let _ = self.stop.send(true);
        for task in &self.background {
            task.abort();
        }
        let deadline = tokio::time::sleep(self.drain + Duration::from_millis(250));
        tokio::pin!(deadline);
        for mut server in self.servers {
            tokio::select! {
                _ = &mut server => {}
                _ = &mut deadline => server.abort(),
            }
        }
        self.shared.router.inspector.close_log();
        if let Some(writer) = self.log_writer {
            let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
        }
        let _ = std::fs::remove_file(self.runtime.serve_sock_path());
    }
}

/// 32 random bytes, hex.
fn new_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

fn full(body: impl Into<Bytes>) -> BoxBody {
    Full::new(body.into()).boxed()
}

fn plain(status: StatusCode, text: &'static str) -> Response<BoxBody> {
    let mut response = Response::new(full(text));
    *response.status_mut() = status;
    response
}

fn json(status: StatusCode, body: impl Into<Bytes>) -> Response<BoxBody> {
    let mut response = Response::new(full(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

/// Accept connections until stopped, then drain.
async fn serve_tcp(
    listener: TcpListener,
    shared: Arc<Shared>,
    mut stopped: watch::Receiver<bool>,
    drain: Duration,
) {
    let graceful = GracefulShutdown::new();
    let mut builder = http1::Builder::new();
    builder.timer(TokioTimer::new());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    // Out of descriptors, most likely: don't spin.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let _ = stream.set_nodelay(true);
                let shared = shared.clone();
                let service = service_fn(move |request| {
                    let shared = shared.clone();
                    async move { Ok::<_, Infallible>(proxy(&shared, request).await) }
                });
                let connection = builder.serve_connection(TokioIo::new(stream), service);
                let connection = graceful.watch(connection);
                tokio::spawn(async move {
                    let _ = connection.await;
                });
            }
            _ = stopped.changed() => break,
        }
    }
    drop(listener);
    let _ = tokio::time::timeout(drain, graceful.shutdown()).await;
}

/// Whether `Host` names this gateway: `127.0.0.1` or `localhost`, with our
/// port if any. Anything else may be a DNS-rebinding page.
fn host_allowed(host: Option<&HeaderValue>, port: u16) -> bool {
    let Some(host) = host.and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let (name, host_port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    (name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost"))
        && host_port.is_none_or(|p| p.parse() == Ok(port))
}

fn is_json(content_type: Option<&HeaderValue>) -> bool {
    content_type
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// Compare without an early exit on the first differing byte.
fn token_matches(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// One proxied request: auth, then routing. No CORS headers are ever sent.
async fn proxy(shared: &Shared, request: Request<Incoming>) -> Response<BoxBody> {
    if !host_allowed(request.headers().get(HOST), shared.port) {
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
    if !token_matches(token, &shared.token) {
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
            .insert(RETRY_AFTER, HeaderValue::from(wait.as_secs().max(1)));
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
                    async move { Ok::<_, Infallible>(control(&shared, request, stopped)) }
                });
                let connection = builder.serve_connection(TokioIo::new(stream), service);
                let connection = graceful.watch(connection);
                tokio::spawn(async move {
                    let _ = connection.await;
                });
            }
            _ = stopped.changed() => break,
        }
    }
    drop(listener);
    let _ = tokio::time::timeout(drain, graceful.shutdown()).await;
}

fn control(
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
            let snapshot = shared.router.metrics.snapshot(now_ms() / 1000);
            json(
                StatusCode::OK,
                serde_json::to_vec(&snapshot).unwrap_or_default(),
            )
        }
        (&Method::POST, RELOAD_PATH) => match shared.reload() {
            Ok(()) => plain(StatusCode::NO_CONTENT, ""),
            Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "reload failed\n"),
        },
        (&Method::GET, EVENTS_PATH) => {
            let options = EventsOptions::from_query(request.uri().query().unwrap_or(""));
            let mut response = Response::new(events_body(shared, options, stopped));
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

/// A response body fed from a channel; ends when the sender is dropped.
struct ChannelBody(mpsc::Receiver<Bytes>);

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|bytes| Ok(Frame::data(bytes))))
    }
}

/// The NDJSON events stream: the backlog, then live records until the
/// client goes away or the gateway stops.
fn events_body(
    shared: &Arc<Shared>,
    options: EventsOptions,
    mut stopped: watch::Receiver<bool>,
) -> BoxBody {
    let (backlog, live) = shared.router.inspector.subscribe(options);
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    tokio::spawn(async move {
        let line = |record| {
            let mut line = serde_json::to_vec(&record).unwrap_or_default();
            line.push(b'\n');
            Bytes::from(line)
        };
        // One chunk for the whole backlog.
        if !backlog.is_empty() {
            let chunk: Vec<u8> = backlog.into_iter().flat_map(|r| line(r).to_vec()).collect();
            if tx.send(Bytes::from(chunk)).await.is_err() {
                return;
            }
        }
        let Some(mut live) = live else { return };
        loop {
            tokio::select! {
                record = live.recv() => match record {
                    Ok(record) => {
                        if tx.send(line(record)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = tx.closed() => return,
                _ = stopped.changed() => return,
            }
        }
    });
    ChannelBody(rx).boxed()
}

/// Probe every failover pool's heads and keep the per-chain high-water
/// mark. Single-endpoint pools need no mark and are never probed. Pools
/// without recent traffic are probed less often, so an idle gateway spends
/// little against provider quotas.
async fn probe_loop(shared: Arc<Shared>, client: RpcClient, interval: Duration) {
    let idle_interval = interval * IDLE_PROBE_FACTOR;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let now = now_ms() / 1000;
        let snapshot = shared.router.snapshot();
        let mut probes = Vec::new();
        for chain in &snapshot.chains {
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
            for endpoint in chain.pool() {
                let chain = chain.clone();
                let client = client.clone();
                probes.push(tokio::spawn(async move {
                    let result = chainz_rpc::probe(&client, &endpoint.target, chain.id).await;
                    endpoint.set_head(result.head);
                    if let Some(head) = result.head {
                        chain.observe_head(head);
                    }
                }));
            }
        }
        for probe in probes {
            let _ = probe.await;
        }
    }
}

/// Reload when `config.toml` or `secrets.toml` changes, and refresh
/// capability tags when the health store changes.
async fn watch_loop(shared: Arc<Shared>, interval: Duration) {
    type Stamp = Option<(SystemTime, u64)>;
    fn stamp(path: &std::path::Path) -> Stamp {
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }
    let health = shared.state_dir.as_ref().map(|dir| dir.join("health.json"));
    let stamps = |shared: &Shared| -> (Stamp, Stamp, Stamp) {
        (
            stamp(&shared.paths.config),
            stamp(&shared.paths.secrets),
            health.as_deref().and_then(stamp),
        )
    };
    let mut seen = stamps(&shared);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let now = stamps(&shared);
        if (now.0, now.1) != (seen.0, seen.1) {
            let _ = shared.reload();
        } else if now.2 != seen.2
            && let Some(dir) = &shared.state_dir
        {
            shared.router.snapshot().refresh_capabilities(dir);
        }
        seen = now;
    }
}
