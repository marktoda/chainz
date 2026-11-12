//! The running gateway: listeners, background tasks and lifecycle.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chainz_core::StorePaths;
use chainz_core::runtime::{
    DEFAULT_PORT, GatewayAddr, GatewayState, LOOPBACK, Published, Runtime, ServeInfo, ServeLock,
};
use chainz_rpc::control::{ChainStatus, MetricsSnapshot, MetricsTotals, SocketPath, Status};
use chainz_rpc::{HttpUpstream, RpcClient};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::header::{ALLOW, CONTENT_TYPE, HOST, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::Value;
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{broadcast, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::auth;
use crate::control::{self, ControlState, EventRing};
use crate::http::{self, BoxBody, Reply, is_json, json, plain};
use crate::inspector::Inspector;
use crate::router::Router;
use crate::state::Snapshot;
use chainz_sys::{FileStamp, now_secs};

/// Largest request body accepted.
const MAX_REQUEST_BODY: usize = 32 * 1024 * 1024;
/// A chain with no request for this many seconds is idle: its pool is
/// probed [`IDLE_PROBE_FACTOR`] times less often.
const PROBE_IDLE_AFTER: u64 = 300;
/// How much slower idle pools are probed (every minute at the default 5 s).
const IDLE_PROBE_FACTOR: u32 = 12;
/// How long in-flight requests get to finish on shutdown, by default.
pub const DRAIN: Duration = Duration::from_secs(5);
/// What shutdown allows past the drain before aborting the gateway's tasks.
const DRAIN_SLACK: Duration = Duration::from_millis(250);
/// How long shutdown waits for the `--log` writer to flush.
const LOG_WRITER_WAIT: Duration = Duration::from_secs(2);
/// The longest [`Gateway::shutdown`] takes with the default drain.
pub const SHUTDOWN_BUDGET: Duration = DRAIN
    .saturating_add(DRAIN_SLACK)
    .saturating_add(LOG_WRITER_WAIT);

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
        GatewayConfig {
            paths,
            runtime,
            state_dir,
            port: DEFAULT_PORT,
            log: None,
            timeout: Duration::from_secs(30),
            probe_interval: Duration::from_secs(5),
            watch_interval: Duration::from_millis(500),
            drain: DRAIN,
        }
    }
}

/// State shared by every task.
struct Shared {
    router: Router,
    paths: StorePaths,
    state_dir: Option<PathBuf>,
    /// Port and token: the gateway's URLs.
    addr: GatewayAddr,
    /// What `serve.json` says; the control API's status is built from it.
    info: ServeInfo,
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
            version: self.info.version.clone(),
            pid: self.info.pid,
            port: self.info.port,
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

impl ControlState for Shared {
    fn status(&self) -> Status {
        Shared::status(self)
    }

    fn metrics(&self) -> MetricsSnapshot {
        self.router.metrics.snapshot(now_secs())
    }

    fn events(&self) -> &EventRing {
        self.router.inspector.events()
    }

    async fn reload(&self) -> Result<()> {
        Shared::reload(self).await
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
    /// `serve.json`, withdrawn first on shutdown (before the lock goes).
    published: Option<Published>,
    _lock: ServeLock,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("port", &self.shared.addr.port)
            .finish_non_exhaustive()
    }
}

impl Gateway {
    /// Take `serve.lock`, load the config, bind the loopback port and the
    /// control socket, publish `serve.json`, and start serving.
    pub async fn start(config: GatewayConfig) -> Result<Gateway> {
        // Taking the lock also removes what a gateway that died left, so
        // `serve stop` never reads a dead gateway's PID while we run.
        let Some(lock) = config
            .runtime
            .acquire_serve_lock()
            .context("can't take serve.lock")?
        else {
            let running = match config.runtime.gateway() {
                Ok(GatewayState::Up(info)) => Some(info),
                _ => None,
            };
            bail!(crate::AlreadyRunning(running));
        };
        // Stamped before the load, so the watcher sees a change made while
        // it runs.
        let stamps = Stamps::take(&config.paths, config.state_dir.as_deref());
        let snapshot = Snapshot::load(&config.paths, config.state_dir.as_deref(), None)?;
        let warnings = snapshot.warnings.clone();

        let address = SocketAddr::from((LOOPBACK, config.port));
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
        let addr = GatewayAddr {
            port,
            token: auth::new_token(),
        };
        // One pool for forwarding and head probes, under the per-attempt
        // timeout.
        let client = RpcClient::with_timeout(config.timeout);
        let shared = Arc::new(Shared {
            router: Router::new(snapshot, HttpUpstream::new(client.clone()), inspector),
            paths: config.paths,
            state_dir: config.state_dir,
            info: ServeInfo::new(&addr, now_secs()),
            addr,
            started: Instant::now(),
            notices,
            reloading: tokio::sync::Mutex::new(()),
        });

        // Published before anything is spawned. Both listeners are bound,
        // so a client that reads it early waits in the accept backlog; and
        // a failed write leaves nothing running that holds them.
        let published = match lock.publish(&shared.info) {
            Ok(published) => published,
            Err(error) => {
                let _ = std::fs::remove_file(&sock);
                shared.router.inspector.close_log();
                if let Some(writer) = log_writer {
                    let _ = writer.await;
                }
                return Err(error).context("can't write serve.json");
            }
        };

        let (stop, stopped) = watch::channel(false);
        let tasks = vec![
            tokio::spawn(http::serve(
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
            tokio::spawn(http::serve(
                unix,
                {
                    let shared = shared.clone();
                    let stopped = stopped.clone();
                    move |request| {
                        let shared = shared.clone();
                        let stopped = stopped.clone();
                        async move { control::handle(&*shared, request, stopped).await }
                    }
                },
                stopped.clone(),
                config.drain,
            )),
            tokio::spawn(probe_loop(
                shared.clone(),
                client,
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
            published: Some(published),
            _lock: lock,
        };
        for warning in warnings {
            gateway.shared.notice(format!("warning: {warning}"));
        }
        Ok(gateway)
    }

    /// The loopback port.
    pub fn port(&self) -> u16 {
        self.shared.addr.port
    }

    /// Port and token: the gateway's URLs. The token is a secret.
    pub fn addr(&self) -> &GatewayAddr {
        &self.shared.addr
    }

    /// The current status, as the control API reports it.
    pub fn status(&self) -> Status {
        self.shared.status()
    }

    /// Totals over the last minute.
    pub fn summary(&self) -> MetricsTotals {
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
        drop(self.published.take());
        let _ = self.stop.send(true);
        // Each task stops on the signal (the servers after draining);
        // whatever is still running at the deadline is aborted.
        let deadline = tokio::time::Instant::now() + self.drain + DRAIN_SLACK;
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
            && tokio::time::timeout(LOG_WRITER_WAIT, &mut writer)
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
    if !auth::host_allowed(host, shared.addr.port) {
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
    let Some(path) = GatewayAddr::parse_path(&path) else {
        return plain(StatusCode::FORBIDDEN, "forbidden\n");
    };
    let (Some(chain_ref), pin) = (path.chain, path.endpoint) else {
        return plain(StatusCode::FORBIDDEN, "forbidden\n");
    };
    if !auth::token_matches(path.token, &shared.addr.token) {
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
    shared
        .router
        .handle(&snapshot, chain, pin, body)
        .await
        .into_response()
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
