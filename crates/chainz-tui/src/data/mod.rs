//! The data layer: background workers that poll chains, probe endpoints,
//! read balances and follow the gateway, and report to the UI as
//! [`Update`]s over a channel. Nothing here draws, and the UI thread never
//! waits on anything here.
//!
//! Traffic budget, per the design: one head request per chain per tick
//! (paced by block time), one balance request per chain every 30 s, and
//! probes only for the chain whose endpoints are on screen, and only while
//! someone is pressing keys.

mod balances;
mod heads;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use chainz_core::names;
use chainz_core::route::{self, Route};
use chainz_core::runtime::{GatewayState, Runtime, ServeInfo};
use chainz_rpc::{ErrorClass, HealthSample, HealthStore, RpcClient, Target, probe_all};
use tokio::runtime::Handle;
use tokio::sync::{Notify, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::gateway::GatewayLink;
use crate::model::{Fault, GatewayInfo, Probe, Refresh, Update};
use crate::plan::{ChainPlan, Plan, Source};
use chainz_sys::now_ms;

/// Every interval and deadline the workers use; tests shrink them.
#[derive(Debug, Clone)]
pub struct Tuning {
    /// Fastest head poll, for chains with fast blocks. Each poll is a
    /// billed request on a metered key, hence 3 s and not 1 s.
    pub min_pace: Duration,
    /// Slowest head poll, also used while the block time is unknown.
    pub max_pace: Duration,
    /// Wait after a failed head poll.
    pub error_pace: Duration,
    /// Deadline for one head or balance request.
    pub request_timeout: Duration,
    /// Blocks to look back for the first block-time estimate.
    pub block_span: u64,
    /// Endpoint probe interval for the chain on screen.
    pub probe_every: Duration,
    /// Balance refresh interval.
    pub balance_every: Duration,
    /// Gateway metrics poll interval.
    pub traffic_every: Duration,
    /// How often to look for a gateway and a changed config.
    pub watch_every: Duration,
    /// Gap between the first requests to chains that share a provider, so
    /// start-up is not a burst against one API key.
    pub stagger: Duration,
    /// `--once` only: wait before retrying a rate-limited request.
    pub retry_wait: Duration,
    /// With no key pressed for this long, heads are polled at `max_pace`
    /// until the next key: a dashboard left open should cost little.
    pub idle_after: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            min_pace: Duration::from_secs(3),
            max_pace: Duration::from_secs(12),
            error_pace: Duration::from_secs(4),
            request_timeout: Duration::from_secs(5),
            block_span: 200,
            probe_every: Duration::from_secs(15),
            balance_every: Duration::from_secs(30),
            traffic_every: Duration::from_secs(1),
            watch_every: Duration::from_secs(2),
            stagger: Duration::from_millis(250),
            retry_wait: Duration::from_secs(1),
            idle_after: Duration::from_secs(300),
        }
    }
}

/// What every worker shares, and what outlives an engine restart: the
/// connection pool, the health history and what was learned about chains.
pub struct Shared {
    client: RpcClient,
    store: Mutex<HealthStore>,
    runtime: Runtime,
    config_path: PathBuf,
    tuning: Tuning,
    updates: Sender<Update>,
    gateway: RwLock<Option<ServeInfo>>,
    /// Chain ID → whether Multicall3 has code there (once known).
    multicall: Mutex<HashMap<u64, bool>>,
    /// Chain ID → its block clock and look-back tries left, so a restarted
    /// poller keeps what an earlier one measured.
    learned: Mutex<HashMap<u64, (heads::BlockClock, u8)>>,
    /// No key for `idle_after`: head pollers slow to the ceiling.
    idle: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl Shared {
    /// Shared worker state. `updates` is where every [`Update`] goes.
    pub fn new(
        store: HealthStore,
        runtime: Runtime,
        config_path: PathBuf,
        tuning: Tuning,
        updates: Sender<Update>,
    ) -> Arc<Self> {
        let shared = Shared {
            client: RpcClient::new(),
            store: Mutex::new(store),
            gateway: RwLock::new(None),
            runtime,
            config_path,
            tuning,
            updates,
            multicall: Mutex::new(HashMap::new()),
            learned: Mutex::new(HashMap::new()),
            idle: std::sync::atomic::AtomicBool::new(false),
        };
        shared.detect_gateway();
        Arc::new(shared)
    }

    /// The worker intervals.
    pub fn tuning(&self) -> &Tuning {
        &self.tuning
    }

    /// Tell the head pollers whether anyone is watching. Takes effect at
    /// each poller's next tick; going active again causes no burst.
    pub fn set_idle(&self, idle: bool) {
        self.idle.store(idle, std::sync::atomic::Ordering::Relaxed);
    }

    fn idle(&self) -> bool {
        self.idle.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The health history, as of the latest probe.
    pub fn store(&self) -> HealthStore {
        self.store.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The running gateway, as last detected.
    pub fn gateway(&self) -> Option<GatewayInfo> {
        self.serve_info()
            .map(|info| GatewayInfo { port: info.port })
    }

    /// A link to the gateway's control socket.
    pub fn link(&self) -> GatewayLink {
        GatewayLink::new(self.runtime.serve_sock_path())
    }

    fn serve_info(&self) -> Option<ServeInfo> {
        self.gateway
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Look for a gateway (one non-blocking `flock` and a small file read)
    /// and report whether anything changed.
    fn detect_gateway(&self) -> bool {
        // Unreadable counts as no gateway here; the dashboard adopts the
        // warning with the rest of B2 (stream S4).
        let found = match self.runtime.gateway() {
            Ok(GatewayState::Up(info)) => Some(info),
            _ => None,
        };
        let mut current = self.gateway.write().unwrap_or_else(|e| e.into_inner());
        let changed = *current != found;
        *current = found;
        changed
    }

    fn lock_learned(&self) -> std::sync::MutexGuard<'_, HashMap<u64, (heads::BlockClock, u8)>> {
        self.learned.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, update: Update) {
        // The receiver is gone only while the UI is shutting down.
        let _ = self.updates.send(update);
    }

    /// Where to read `chain` from: the gateway when one runs (so the
    /// dashboard's polling shows in its inspector), else the chain's active
    /// endpoint. The flag says which.
    fn route(&self, chain: &ChainPlan) -> (Result<Target, Fault>, bool) {
        let invalid = |message: String| Fault::new(ErrorClass::Invalid, message);
        let Some(active) = chain.active_endpoint() else {
            return (Err(invalid("no active endpoint".into())), false);
        };
        if let Some(info) = self
            .serve_info()
            .filter(|_| route::uses_gateway(active.info.websocket, false))
        {
            let route =
                Route::via_gateway(&info, &chain.info.name, chain.info.id, "gateway", false);
            let target = Target::new(route.chain_id, &route.endpoint, &route.url, &route.headers)
                .map_err(|e| invalid(e.to_string()));
            return (target, true);
        }
        let target = active
            .target
            .clone()
            .ok_or_else(|| invalid(active.info.invalid.clone().unwrap_or_default()));
        (target, false)
    }
}

/// Wake-ups for `r`: each worker waits on its own.
#[derive(Debug, Default)]
struct Signals {
    chains: Notify,
    endpoints: Notify,
    balances: Notify,
    traffic: Notify,
}

/// The running workers. Dropping it stops them.
#[derive(Debug)]
pub struct Engine {
    handle: Handle,
    shared: Arc<Shared>,
    /// Each chain's head poller, by name, with the source it polls.
    pollers: HashMap<String, (Source, JoinHandle<()>)>,
    /// The probe, balance and watch workers.
    tasks: Vec<JoinHandle<()>>,
    plan: watch::Sender<Arc<Plan>>,
    focus: watch::Sender<Option<String>>,
    signals: Arc<Signals>,
}

impl Engine {
    /// Spawn the workers for `plan` on `handle`.
    pub fn start(handle: &Handle, shared: &Arc<Shared>, plan: &Arc<Plan>) -> Self {
        let signals = Arc::new(Signals::default());
        let (focus, focus_rx) = watch::channel(None);
        let (plan_tx, plan_rx) = watch::channel(plan.clone());
        let tasks = vec![
            handle.spawn(probe_loop(
                shared.clone(),
                plan_rx.clone(),
                focus_rx,
                signals.clone(),
            )),
            handle.spawn(balances::run(shared.clone(), plan_rx, signals.clone())),
            handle.spawn(watch_loop(shared.clone(), signals.clone())),
        ];
        let mut engine = Engine {
            handle: handle.clone(),
            shared: shared.clone(),
            pollers: HashMap::new(),
            tasks,
            plan: plan_tx,
            focus,
            signals,
        };
        engine.start_pollers(plan);
        engine
    }

    /// Move the workers to a new plan (a config change, `s`). Only what
    /// changed restarts: a chain whose active endpoint is the same keeps
    /// its poller and its schedule, and balances are read again only for
    /// chains that are new or read from somewhere else.
    pub fn replan(&mut self, plan: &Arc<Plan>) {
        self.start_pollers(plan);
        self.plan.send_replace(plan.clone());
    }

    /// Keep the pollers whose chain reads from the same place, stop the
    /// rest, and start what is missing (staggered per provider).
    fn start_pollers(&mut self, plan: &Arc<Plan>) {
        let mut running = std::mem::take(&mut self.pollers);
        for (chain, slot) in plan.chains.iter().zip(provider_slots(plan)) {
            let name = &chain.info.name;
            if let Some((source, task)) = running.remove(name) {
                if source == chain.source {
                    self.pollers.insert(name.clone(), (source, task));
                    continue;
                }
                task.abort();
            }
            let poller = heads::Poller::patient(self.shared.clone(), chain.clone());
            let offset = self.shared.tuning.stagger * slot;
            let task = self.handle.spawn(poller.run(offset, self.signals.clone()));
            self.pollers.insert(name.clone(), (chain.source, task));
        }
        for (_, task) in running.into_values() {
            task.abort();
        }
    }

    /// Wake one worker early.
    pub fn refresh(&self, what: Refresh) {
        match what {
            Refresh::Chains => self.signals.chains.notify_waiters(),
            Refresh::Endpoints => self.signals.endpoints.notify_waiters(),
            Refresh::Balances => self.signals.balances.notify_waiters(),
            Refresh::Traffic => self.signals.traffic.notify_waiters(),
        }
    }

    /// Probe this chain's endpoints while it is on screen; `None` stops.
    pub fn focus(&self, chain: Option<String>) {
        self.focus.send_if_modified(|current| {
            let changed = *current != chain;
            *current = chain;
            changed
        });
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let pollers = self.pollers.values().map(|(_, task)| task);
        for task in self.tasks.iter().chain(pollers) {
            task.abort();
        }
    }
}

/// For each chain, how many earlier chains use the same provider
/// ([`names::provider_of`] the active endpoint). Chains on one provider
/// usually share one API key and one rate limit.
fn provider_slots(plan: &Plan) -> Vec<u32> {
    let provider = |chain: &ChainPlan| -> Option<String> {
        let target = chain.active_endpoint()?.target.as_ref()?;
        names::provider_of(target.url().as_str())
    };
    let mut seen: HashMap<String, u32> = HashMap::new();
    plan.chains
        .iter()
        .map(|chain| match provider(chain) {
            Some(key) => {
                let slot = seen.entry(key).or_insert(0);
                *slot += 1;
                *slot - 1
            }
            None => 0,
        })
        .collect()
}

/// How often `--once` retries a rate-limited request.
const ONCE_RETRIES: u32 = 2;

/// One round of everything `--once` reports: every chain's head and
/// balances, and the gateway's metrics when one runs. No endpoint probes.
pub async fn collect_once(shared: &Arc<Shared>, plan: &Plan) -> Vec<Update> {
    let mut updates = Vec::new();
    if let Some(info) = shared.gateway() {
        updates.push(Update::Gateway(Some(info)));
        updates.push(Update::Traffic(traffic_snapshot(shared).await));
    }
    let holders = Arc::new(plan.holders());
    let mut set = JoinSet::new();
    let mut names = HashMap::new();
    for (chain, slot) in plan.chains.iter().zip(provider_slots(plan)) {
        let (shared, chain, holders) = (shared.clone(), chain.clone(), holders.clone());
        let name = (chain.info.name.clone(), chain.source);
        // Per chain the requests go one after another, and chains on one
        // provider start a beat apart.
        let task = set.spawn(async move {
            tokio::time::sleep(shared.tuning.stagger * slot).await;
            let mut poller = heads::Poller::new(shared.clone(), chain.clone());
            let mut head = poller.step().await;
            for attempt in 1..=ONCE_RETRIES {
                // A snapshot has one chance: give a rate limit (or a block
                // time that needed a second look) a moment and try again.
                let again = match &head {
                    Update::Head {
                        result: Err(fault), ..
                    } => fault.rate_limited,
                    Update::Head { block_time, .. } => block_time.is_none(),
                    _ => false,
                };
                if !again {
                    break;
                }
                tokio::time::sleep(shared.tuning.retry_wait * attempt).await;
                head = poller.step().await;
            }
            let mut updates = vec![head];
            if !holders.is_empty() {
                let read = || balances::read_update(shared.clone(), chain.clone(), holders.clone());
                let mut balances = read().await;
                for attempt in 1..=ONCE_RETRIES {
                    let again = matches!(
                        &balances,
                        Update::Balances { result: Err(fault), .. } if fault.rate_limited
                    );
                    if !again {
                        break;
                    }
                    tokio::time::sleep(shared.tuning.retry_wait * attempt).await;
                    balances = read().await;
                }
                updates.push(balances);
            }
            updates
        });
        names.insert(task.id(), name);
    }
    updates.extend(gather(set, &names).await);
    updates
}

/// Every chain's updates from `set`. A chain whose task died is reported
/// as a failed head, and the other chains' results are kept.
async fn gather(
    mut set: JoinSet<Vec<Update>>,
    names: &HashMap<tokio::task::Id, (String, Source)>,
) -> Vec<Update> {
    let mut updates = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(chain_updates) => updates.extend(chain_updates),
            Err(error) => {
                let Some((chain, source)) = names.get(&error.id()) else {
                    continue;
                };
                let fault = Fault::new(ErrorClass::Invalid, format!("internal error: {error}"));
                updates.push(Update::Head {
                    chain: chain.clone(),
                    source: *source,
                    at_ms: now_ms(),
                    via_gateway: false,
                    result: Err(fault),
                    block_time: None,
                });
            }
        }
    }
    updates
}

/// The gateway's status and metrics, within `request_timeout`: a wedged
/// control socket is an error, not a hang.
async fn traffic_snapshot(shared: &Shared) -> Result<crate::model::TrafficSnapshot, String> {
    tokio::time::timeout(shared.tuning.request_timeout, shared.link().snapshot())
        .await
        .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))
        .map_err(|e| e.to_string())
}

/// Probe the focused chain's endpoints every `probe_every`, and at once
/// when the focus moves to a chain whose last probe is older than that.
/// While the dashboard is idle, rounds are skipped.
async fn probe_loop(
    shared: Arc<Shared>,
    mut plan: watch::Receiver<Arc<Plan>>,
    mut focus: watch::Receiver<Option<String>>,
    signals: Arc<Signals>,
) {
    let mut last: HashMap<String, Instant> = HashMap::new();
    loop {
        let current = plan.borrow_and_update().clone();
        let focused = focus.borrow_and_update().clone();
        let Some(chain) = focused.as_deref().and_then(|name| current.chain(name)) else {
            tokio::select! {
                changed = focus.changed() => if changed.is_err() { return },
                changed = plan.changed() => if changed.is_err() { return },
            }
            continue;
        };
        let wait = last.get(&chain.info.name).map_or(Duration::ZERO, |at| {
            (*at + shared.tuning.probe_every).saturating_duration_since(Instant::now())
        });
        tokio::select! {
            () = tokio::time::sleep(wait) => {
                // Nobody is watching: skip the round, and look again an
                // interval later. A key press makes the next one count.
                if !shared.idle() {
                    probe_chain(&shared, chain).await;
                }
                last.insert(chain.info.name.clone(), Instant::now());
            }
            changed = focus.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            changed = plan.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = signals.endpoints.notified() => {
                last.remove(&chain.info.name);
            }
        }
    }
}

/// One probe round over a chain's HTTP endpoints: record it in the health
/// history (as `rpc bench` does) and report it.
async fn probe_chain(shared: &Arc<Shared>, chain: &ChainPlan) {
    let (names, targets): (Vec<&str>, Vec<Target>) = chain
        .endpoints
        .iter()
        .filter(|e| !e.info.websocket)
        .filter_map(|e| Some((e.info.name.as_str(), e.target.clone()?)))
        .unzip();
    let mut probes = probe_all(&shared.client, targets, chain.info.id);
    let mut results = Vec::with_capacity(names.len());
    let snapshot = {
        let mut pending = Vec::with_capacity(names.len());
        while let Some((index, result)) = probes.recv().await {
            pending.push((index, result));
        }
        let mut store = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        for (index, result) in pending {
            store.record(
                chain.info.id,
                names[index],
                HealthSample::from_probe(&result),
            );
            results.push((
                names[index].to_string(),
                Probe {
                    latency_ms: u32::try_from(result.latency.as_millis()).unwrap_or(u32::MAX),
                    head: result.head,
                    error: result.error.as_ref().map(Fault::from),
                },
            ));
        }
        store.clone()
    };
    // Saving merges under a file lock; keep it off the async workers.
    tokio::task::spawn_blocking(move || snapshot.save());
    shared.send(Update::Probes {
        chain: chain.info.name.clone(),
        source: chain.source,
        at_ms: now_ms(),
        results,
    });
}

/// Notice a gateway starting or stopping and `config.toml` changing, and
/// follow the gateway's metrics and request stream while it runs.
async fn watch_loop(shared: Arc<Shared>, signals: Arc<Signals>) {
    let modified = |shared: &Shared| -> Option<SystemTime> {
        std::fs::metadata(&shared.config_path).ok()?.modified().ok()
    };
    let mut config_seen = modified(&shared);
    let mut feed: JoinSet<()> = JoinSet::new();
    let mut last_watch: Option<Instant> = None;
    shared.send(Update::Gateway(shared.gateway()));
    loop {
        if last_watch.is_none_or(|at| at.elapsed() >= shared.tuning.watch_every) {
            last_watch = Some(Instant::now());
            if shared.detect_gateway() {
                feed.abort_all();
                shared.send(Update::Gateway(shared.gateway()));
            }
            let now = modified(&shared);
            if now != config_seen {
                config_seen = now;
                shared.send(Update::ConfigChanged);
            }
        }
        let running = shared.gateway().is_some();
        if running {
            let link = shared.link();
            shared.send(Update::Traffic(traffic_snapshot(&shared).await));
            // Reap a closed stream, then (re)subscribe.
            while feed.try_join_next().is_some() {}
            if feed.is_empty() {
                let shared = shared.clone();
                feed.spawn(async move {
                    let Ok(mut events) = link.events(crate::model::FEED_CAP).await else {
                        return;
                    };
                    while let Some(item) = events.next().await {
                        shared.send(Update::Feed(item));
                    }
                });
            }
        }
        let nap = if running {
            shared.tuning.traffic_every.min(shared.tuning.watch_every)
        } else {
            shared.tuning.watch_every
        };
        tokio::select! {
            () = tokio::time::sleep(nap) => {}
            () = signals.traffic.notified() => {}
        }
    }
}
