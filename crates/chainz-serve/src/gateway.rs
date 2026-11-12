//! The running gateway: listeners, background tasks and lifecycle.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chainz_core::StorePaths;
use chainz_core::runtime::{
    DEFAULT_PORT, GatewayAddr, GatewayState, LOOPBACK, Published, Runtime, ServeInfo, ServeLock,
};
use chainz_rpc::control::{MetricsSnapshot, MetricsTotals, Status, Window};
use chainz_rpc::error::SERVER_ERROR;
use chainz_rpc::{HttpUpstream, RpcClient};
use hyper::body::Incoming;
use hyper::{Request, Response};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::auth;
use crate::control::{self, ControlState};
use crate::events::EventRing;
use crate::http::{self, BoxBody, DRAIN, Reply};
use crate::inspector::Inspector;
use crate::router::Router;
use crate::state::{Snapshot, Stamps};
use chainz_os::now_secs;

/// A chain with no request for this many seconds is idle: its pool is
/// probed [`IDLE_PROBE_FACTOR`] times less often.
const PROBE_IDLE_AFTER: u64 = 300;
/// How much slower idle pools are probed (every minute at the default 5 s).
const IDLE_PROBE_FACTOR: u32 = 12;
/// What shutdown allows past the drain before aborting the gateway's tasks.
const DRAIN_SLACK: Duration = Duration::from_millis(250);
/// How long shutdown waits for the `--log` writer to flush.
const LOG_WRITER_WAIT: Duration = Duration::from_secs(2);
/// The longest [`Gateway::shutdown`] takes with the default drain.
pub const SHUTDOWN_BUDGET: Duration = DRAIN
    .saturating_add(DRAIN_SLACK)
    .saturating_add(LOG_WRITER_WAIT);

/// The one wording of "a gateway already runs": the bin says it before
/// detaching, and [`Gateway::start`] says it when it loses the lock race.
#[derive(Debug)]
pub struct AlreadyRunning(pub Option<ServeInfo>);

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "chainz serve is already running")?;
        if let Some(info) = &self.0 {
            write!(f, " on port {} (pid {})", info.port, info.pid)?;
        }
        write!(f, "; stop it with `chainz serve stop`")
    }
}

impl std::error::Error for AlreadyRunning {}

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
    /// What `serve.json` says: port and token (the gateway's URLs) and the
    /// start; the control API's status is built from it.
    info: ServeInfo,
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

    /// The config files and the health store as they are now.
    fn stamps(&self) -> Stamps {
        Stamps::take(&self.paths, self.state_dir.as_deref())
    }

    /// Re-read the config now.
    async fn reload(&self) -> Result<()> {
        let one_at_a_time = self.reloading.lock().await;
        self.reload_locked(one_at_a_time).await
    }

    /// Re-read the config unless the running snapshot already reflects the
    /// files: a reload that ran while this one waited may have read them.
    async fn reload_if_changed(&self) -> Result<()> {
        let one_at_a_time = self.reloading.lock().await;
        if self.stamps().store == self.router.snapshot().stamps().store {
            return Ok(());
        }
        self.reload_locked(one_at_a_time).await
    }

    async fn reload_locked(&self, _one_at_a_time: tokio::sync::MutexGuard<'_, ()>) -> Result<()> {
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
            uptime_secs: now_secs().saturating_sub(self.info.started_at),
            chains: snapshot.chains.iter().map(|chain| chain.status()).collect(),
        }
    }
}

impl ControlState for Shared {
    fn status(&self) -> Status {
        Shared::status(self)
    }

    fn metrics(&self) -> MetricsSnapshot {
        self.router.metrics(now_secs())
    }

    fn events(&self) -> &EventRing {
        self.router.events()
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
            .field("port", &self.shared.info.port)
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
            bail!(AlreadyRunning(running));
        };
        let snapshot = Snapshot::load(&config.paths, config.state_dir.as_deref(), None)?;

        let address = SocketAddr::from((LOOPBACK, config.port));
        let tcp = TcpListener::bind(address)
            .await
            .with_context(|| format!("can't listen on {address}"))?;
        let port = tcp.local_addr()?.port();

        // We hold the lock, so any socket file here is stale.
        let sock = config.runtime.serve_sock_path();
        let unix = control::bind(&sock)
            .with_context(|| format!("can't bind the control socket {}", sock.display()))?;

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
                shared.router.close_log();
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
            tokio::spawn(watch_loop(shared.clone(), config.watch_interval, stopped)),
        ];
        Ok(Gateway {
            shared,
            runtime: config.runtime,
            stop,
            tasks,
            log_writer,
            drain: config.drain,
            published: Some(published),
            _lock: lock,
        })
    }

    /// Port and token: the gateway's URLs. The token is a secret.
    pub fn addr(&self) -> GatewayAddr {
        self.shared.info.addr()
    }

    /// The current status, as the control API reports it.
    pub fn status(&self) -> Status {
        self.shared.status()
    }

    /// Totals over the last minute: the short window's, as the control
    /// API reports them.
    pub fn summary(&self) -> MetricsTotals {
        self.shared
            .router
            .metrics(now_secs())
            .window(Window::Short)
            .map(|window| window.totals)
            .unwrap_or_default()
    }

    /// Notices as they happen: reloads, and warnings that come up while
    /// running. The warnings of the config the gateway started with are
    /// [`Gateway::warnings`].
    pub fn notices(&self) -> broadcast::Receiver<String> {
        self.shared.notices.subscribe()
    }

    /// Unroutable endpoints and other problems found when the config was
    /// last loaded.
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
        self.shared.router.close_log();
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

/// One proxied request: auth, then routing.
async fn proxy(shared: &Shared, request: Request<Incoming>) -> Response<BoxBody> {
    // `/<token>/<chain>[/<endpoint>]`, and the chain must exist.
    let authorize = |path: &str| {
        let Some(path) = GatewayAddr::parse_path(path) else {
            return Err(http::forbidden());
        };
        let (Some(chain_ref), pin) = (path.chain, path.endpoint) else {
            return Err(http::forbidden());
        };
        if !auth::token_matches(path.token, &shared.info.token) {
            return Err(http::forbidden());
        }
        let Some(chain) = shared.router.chain(chain_ref) else {
            let reply = Reply::error(404, &Value::Null, SERVER_ERROR, "unknown chain");
            return Err(Box::new(reply.into_response()));
        };
        Ok((chain, pin.map(str::to_string)))
    };
    match http::guard(request, shared.info.port, authorize).await {
        Ok(((chain, pin), body)) => shared
            .router
            .handle(chain, pin.as_deref(), body)
            .await
            .into_response(),
        Err(response) => *response,
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
            let Some(pool) = chain.probe_pool() else {
                continue;
            };
            let busy = chain
                .last_used()
                .is_some_and(|t| now.saturating_sub(t) < PROBE_IDLE_AFTER);
            if !busy && chain.probed_within(idle_interval) {
                continue;
            }
            chain.mark_probed();
            best.insert(index, None);
            for endpoint in pool {
                let client = client.clone();
                let chain_id = chain.id();
                round.spawn(async move {
                    let result = chainz_rpc::probe(&client, endpoint.target(), chain_id).await;
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

/// Reload when `config.toml` or `secrets.toml` changes, and refresh
/// capability tags when the health store changes. "Changes" is against the
/// stamps the running snapshot holds, so a reload that already happened
/// (SIGHUP, the control API) is not repeated here.
async fn watch_loop(shared: Arc<Shared>, interval: Duration, mut stopped: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stopped.changed() => return,
        }
        let now = shared.stamps();
        let snapshot = shared.router.snapshot();
        let loaded = snapshot.stamps();
        if now.store != loaded.store {
            let _ = shared.reload_if_changed().await;
        } else if now.health != loaded.health
            && let Some(dir) = shared.state_dir.clone()
        {
            let refreshed =
                tokio::task::spawn_blocking(move || snapshot.refresh_capabilities(&dir))
                    .await
                    .unwrap_or_else(|e| Err(std::io::Error::other(e)));
            if let Err(error) = refreshed {
                shared.notice(format!(
                    "warning: can't refresh capability tags from health.json: {error}"
                ));
            }
        }
    }
}
