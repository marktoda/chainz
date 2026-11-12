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
mod watch;

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chainz_core::names;
use chainz_core::route::{self, Route};
use chainz_core::runtime::Runtime;
use chainz_rpc::{ErrorClass, HealthSample, HealthStore, RpcClient, Target, probe_all};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc, watch as channel};
use tokio::task::{JoinHandle, JoinSet};

use crate::gateway::GatewayLink;
use crate::model::{Fault, GatewayInfo, PROBE_EVERY_MS, Refresh, Update};
use crate::plan::{self, ChainPlan, Plan, Source};
use chainz_os::now_ms;

use watch::GatewayWatch;
pub use watch::{ConfigFiles, ConfigStamp};

/// What the workers tell the event loop.
#[derive(Debug, Clone)]
pub enum Event {
    /// Something to fold into the state.
    Update(Update),
    /// Something the user should know that is not a row's status: an
    /// unreadable gateway record, a health history that cannot be saved.
    Warning(String),
}

/// The engine's intervals and deadlines; tests shrink them.
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
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            min_pace: Duration::from_secs(3),
            max_pace: Duration::from_secs(12),
            error_pace: Duration::from_secs(4),
            request_timeout: Duration::from_secs(5),
            block_span: 200,
            probe_every: Duration::from_millis(PROBE_EVERY_MS),
            balance_every: Duration::from_secs(30),
            traffic_every: Duration::from_secs(1),
            watch_every: Duration::from_secs(2),
            stagger: Duration::from_millis(250),
        }
    }
}

/// The health history the probes feed, shared with the UI (which reads it
/// when it rebuilds its state).
#[derive(Debug)]
struct HealthSink {
    store: Mutex<HealthStore>,
    /// Save errors, so a broken state dir is said once.
    failed: SayOnce,
}

impl HealthSink {
    fn snapshot(&self) -> HealthStore {
        lock(&self.store).clone()
    }

    /// Record a probe round of `chain_id`'s endpoints and save the history
    /// (blocking: the save merges under a file lock). A save error is
    /// returned once, not again until a save succeeds or fails differently.
    fn record(&self, chain_id: u64, samples: Vec<(String, HealthSample)>) -> Option<String> {
        let mut store = lock(&self.store);
        for (endpoint, sample) in samples {
            store.record(chain_id, &endpoint, sample);
        }
        let saved = store.save();
        drop(store);
        let error = saved.err().map(|e| format!("health history not saved: {e}"));
        self.failed.report(error)
    }
}

/// A problem said once, and not again until it changes or clears.
#[derive(Debug, Default)]
struct SayOnce(Mutex<Option<String>>);

impl SayOnce {
    /// `problem` (or `None` when there is none now), unless it is the one
    /// said last.
    fn report(&self, problem: Option<String>) -> Option<String> {
        let mut last = lock(&self.0);
        let new = problem
            .as_ref()
            .filter(|p| last.as_ref() != Some(*p))
            .cloned();
        *last = problem;
        new
    }
}

/// Where Multicall3 is known to have no code, by chain ID and the URL it
/// was asked through (a fork or another network behind the same ID may
/// have it).
#[derive(Debug, Default)]
struct MulticallCache {
    absent: Mutex<HashSet<(u64, String)>>,
}

impl MulticallCache {
    fn absent(&self, chain_id: u64, target: &Target) -> bool {
        lock(&self.absent).contains(&(chain_id, target.url().to_string()))
    }

    fn mark_absent(&self, chain_id: u64, target: &Target) {
        lock(&self.absent).insert((chain_id, target.url().to_string()));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// What every worker shares, and what outlives an engine restart: the
/// connection pool, the health history, the gateway as last seen and what
/// was learned about chains.
pub struct Shared {
    client: RpcClient,
    tuning: Tuning,
    events: Sender<Event>,
    health: HealthSink,
    gateway: GatewayWatch,
    multicall: MulticallCache,
    /// Chain ID → what its head poller learned, so a restarted poller
    /// keeps it.
    clocks: Mutex<HashMap<u64, heads::Learned>>,
    /// No key for a while: head pollers slow to the ceiling and probe
    /// rounds are skipped.
    idle: AtomicBool,
    /// `config.toml` or `secrets.toml` changed on disk since the event
    /// loop last looked ([`Engine::watch_config`]).
    config_changed: AtomicBool,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl Shared {
    /// Shared worker state; every [`Event`] goes to `events`. Does no IO:
    /// nothing is known about a gateway until [`Shared::watch_gateway`].
    pub fn new(
        store: HealthStore,
        runtime: Runtime,
        tuning: Tuning,
        events: Sender<Event>,
    ) -> Arc<Self> {
        Arc::new(Shared {
            client: RpcClient::new(),
            tuning,
            events,
            health: HealthSink {
                store: Mutex::new(store),
                failed: SayOnce::default(),
            },
            gateway: GatewayWatch::new(runtime),
            multicall: MulticallCache::default(),
            clocks: Mutex::new(HashMap::new()),
            idle: AtomicBool::new(false),
            config_changed: AtomicBool::new(false),
        })
    }

    /// Whether the config files changed since the last call.
    pub fn take_config_changed(&self) -> bool {
        self.config_changed.swap(false, Ordering::Relaxed)
    }

    /// Tell the workers whether anyone is watching. Takes effect at each
    /// head poller's next tick; going active again causes no burst.
    pub fn set_idle(&self, idle: bool) {
        self.idle.store(idle, Ordering::Relaxed);
    }

    fn idle(&self) -> bool {
        self.idle.load(Ordering::Relaxed)
    }

    /// The health history, as of the latest probe.
    pub fn store(&self) -> HealthStore {
        self.health.snapshot()
    }

    /// The running gateway, as last seen.
    pub fn gateway(&self) -> Option<GatewayInfo> {
        self.gateway.info().as_ref().map(GatewayInfo::from)
    }

    /// Look for a gateway now (one non-blocking `flock` and a small file
    /// read). A change is reported as an [`Update::Gateway`]; a gateway
    /// record that can't be read counts as no gateway and is reported as a
    /// warning, once per distinct error.
    pub fn watch_gateway(&self) {
        let warning = self.gateway.detect(|info| {
            self.send(Update::Gateway(info.map(GatewayInfo::from)));
        });
        if let Some(warning) = warning {
            self.warn(warning);
        }
    }

    /// A link to the gateway's control socket.
    fn link(&self) -> GatewayLink {
        GatewayLink::new(self.gateway.socket())
    }

    /// Ask a running gateway to re-read the config now, rather than at its
    /// own next look. A failure is a warning.
    pub fn reload_gateway(self: &Arc<Self>, handle: &Handle) {
        if self.gateway.info().is_none() {
            return;
        }
        let shared = self.clone();
        handle.spawn(async move {
            let link = shared.link();
            let asked = tokio::time::timeout(shared.tuning.request_timeout, link.reload());
            let error = match asked.await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error.to_string(),
                Err(_) => "timed out".into(),
            };
            shared.warn(format!("gateway not reloaded: {error}"));
        });
    }

    fn send(&self, update: Update) {
        self.emit(Event::Update(update));
    }

    fn warn(&self, text: String) {
        self.emit(Event::Warning(text));
    }

    fn emit(&self, event: Event) {
        // The receiver is gone only while the UI is shutting down.
        let _ = self.events.send(event);
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
            .gateway
            .info()
            .filter(|_| route::uses_gateway(active.info.websocket, false))
        {
            let route = Route::via_gateway(
                &info,
                &chain.info.name,
                chain.info.id,
                &active.info.name,
                false,
            );
            return (plan::target(&route).map_err(invalid), true);
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
    /// The probe, balance, gateway and config workers.
    tasks: Vec<JoinHandle<()>>,
    plan: channel::Sender<Arc<Plan>>,
    focus: channel::Sender<Option<String>>,
    signals: Arc<Signals>,
}

impl Engine {
    /// Spawn the workers for `plan` on `handle`.
    pub fn start(handle: &Handle, shared: &Arc<Shared>, plan: &Arc<Plan>) -> Self {
        let signals = Arc::new(Signals::default());
        let (focus, focus_rx) = channel::channel(None);
        let (plan_tx, plan_rx) = channel::channel(plan.clone());
        // Every first poller holds a sender until its first request: the
        // balance reader waits for all of them to let go.
        let (started, pollers_started) = mpsc::channel(1);
        let tasks = vec![
            handle.spawn(probe_loop(
                shared.clone(),
                plan_rx.clone(),
                focus_rx,
                signals.clone(),
            )),
            handle.spawn(balances::run(
                shared.clone(),
                plan_rx,
                pollers_started,
                signals.clone(),
            )),
            handle.spawn(watch::gateway_loop(shared.clone())),
            handle.spawn(watch::traffic_loop(shared.clone(), signals.clone())),
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
        engine.start_pollers(plan, Some(&started));
        engine
    }

    /// Also tell the event loop when `files` change from `since`.
    pub fn watch_config(&mut self, files: ConfigFiles, since: ConfigStamp) {
        let task = watch::config_loop(self.shared.clone(), files, since);
        self.tasks.push(self.handle.spawn(task));
    }

    /// Move the workers to a new plan (a config change, `s`). Only what
    /// changed restarts: a chain that reads from the same source keeps its
    /// poller and its schedule, and balances are read again only for
    /// chains that are new or read from somewhere else.
    pub fn replan(&mut self, plan: &Arc<Plan>) {
        self.start_pollers(plan, None);
        self.plan.send_replace(plan.clone());
    }

    /// Keep the pollers whose chain reads from the same source, stop the
    /// rest, and start what is missing.
    fn start_pollers(&mut self, plan: &Arc<Plan>, started: Option<&mpsc::Sender<()>>) {
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
            // The live poller keeps its first tick to one request.
            let poller = heads::Poller::new(self.shared.clone(), chain.clone(), slot, 1);
            let task = poller.run(self.signals.clone(), started.cloned());
            let task = self.handle.spawn(task);
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

/// One round of everything `--once` reports, sent as the live workers
/// send theirs: every chain's head and balances, and the gateway's metrics
/// when one runs. No endpoint probes. A rate-limited request is retried
/// after `retry_wait`, then twice that.
pub async fn collect_once(shared: &Arc<Shared>, plan: &Plan, retry_wait: Duration) {
    // Reports a gateway it finds before anything read from it.
    shared.watch_gateway();
    if shared.gateway().is_some() {
        shared.send(Update::Traffic(traffic_snapshot(shared).await));
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
            let mut poller = heads::Poller::new(shared.clone(), chain.clone(), slot, 0);
            tokio::time::sleep(poller.offset()).await;
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
                tokio::time::sleep(retry_wait * attempt).await;
                head = poller.step().await;
            }
            shared.send(head);
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
                    tokio::time::sleep(retry_wait * attempt).await;
                    balances = read().await;
                }
                shared.send(balances);
            }
        });
        names.insert(task.id(), name);
    }
    gather(shared, set, &names).await;
}

/// Wait for every chain's task in `set`. A chain whose task died is
/// reported as a failed head, an internal fault, and the other chains'
/// results are kept.
async fn gather(
    shared: &Shared,
    mut set: JoinSet<()>,
    names: &HashMap<tokio::task::Id, (String, Source)>,
) {
    while let Some(joined) = set.join_next().await {
        let Err(error) = joined else { continue };
        let Some((chain, source)) = names.get(&error.id()) else {
            continue;
        };
        shared.send(Update::Head {
            chain: chain.clone(),
            source: *source,
            at_ms: now_ms(),
            via_gateway: false,
            result: Err(Fault::internal(format!("internal error: {error}"))),
            block_time: None,
        });
    }
}

/// The gateway's status and metrics, within `request_timeout`: a wedged
/// control socket is an error, not a hang.
async fn traffic_snapshot(shared: &Shared) -> Result<crate::model::TrafficSnapshot, String> {
    tokio::time::timeout(shared.tuning.request_timeout, shared.link().snapshot())
        .await
        .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
        .map_err(|e| e.to_string())
}

/// Probe the focused chain's endpoints every `probe_every`, and at once
/// when the focus moves to a chain whose last probe is older than that.
/// While the dashboard is idle, rounds are skipped.
async fn probe_loop(
    shared: Arc<Shared>,
    mut plan: channel::Receiver<Arc<Plan>>,
    mut focus: channel::Receiver<Option<String>>,
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

/// One probe round over a chain's HTTP endpoints: report it, and record it
/// in the health history (as `rpc bench` does), the same samples both.
async fn probe_chain(shared: &Arc<Shared>, chain: &ChainPlan) {
    let (names, targets): (Vec<&str>, Vec<Target>) = chain
        .endpoints
        .iter()
        .filter(|e| !e.info.websocket)
        .filter_map(|e| Some((e.info.name.as_str(), e.target.clone()?)))
        .unzip();
    let mut round = probe_all(&shared.client, targets, chain.info.id);
    let mut results = Vec::with_capacity(names.len());
    while let Some((index, result)) = round.recv().await {
        let message = result.error.as_ref().map(ToString::to_string);
        let sample = HealthSample::from_probe(&result);
        results.push((names[index].to_string(), sample, message));
    }
    let samples = results
        .iter()
        .map(|(name, sample, _)| (name.clone(), sample.clone()))
        .collect();
    shared.send(Update::Probes {
        chain: chain.info.name.clone(),
        source: chain.source,
        at_ms: now_ms(),
        results,
    });
    // Saving merges under a file lock; keep it off the async workers.
    let (sink, chain_id) = (shared.clone(), chain.info.id);
    let saved = tokio::task::spawn_blocking(move || sink.health.record(chain_id, samples));
    if let Ok(Some(warning)) = saved.await {
        shared.warn(warning);
    }
}
