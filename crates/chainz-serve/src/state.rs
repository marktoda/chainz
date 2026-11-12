//! The gateway's view of the config: chains, pools and per-endpoint state.
//!
//! A [`Snapshot`] is immutable and replaced wholesale on reload. Live state
//! (limiters, heads, sticky filters) is carried across reloads when the
//! chain (same name and ID) or endpoint (same chain ID, URL and headers) is
//! unchanged.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use chainz_core::labels::LabelIndex;
use chainz_core::model::{Chain, Config, Endpoint};
use chainz_core::{Chainz, LoadMode, StorePaths, resolve};
use chainz_rpc::control::ChainStatus;
use chainz_rpc::health::FAILOVER_LAG;
use chainz_rpc::{Capabilities, HealthStore, Target};
use chainz_sys::FileStamp;

use crate::classify::Needs;
use crate::limiter::AimdLimiter;

/// Probe rounds the high-water mark is taken over. Long enough that one
/// failed probe of the leading endpoint doesn't drop the mark, short enough
/// that one bogus head doesn't hold it up for good.
const HEAD_ROUNDS: usize = 3;
/// Sticky filter entries kept per chain.
const MAX_FILTERS: usize = 4096;

/// One routable (HTTP) endpoint.
pub struct EndpointState {
    /// Endpoint name.
    name: String,
    /// Expanded URL and headers.
    target: Target,
    /// URL and headers, to detect changes on reload. Secret: never printed.
    fingerprint: String,
    /// Adaptive rate limiter.
    limiter: AimdLimiter,
    /// Last probed head; 0 = unknown or the last probe failed.
    head: AtomicU64,
    caps: Mutex<Option<Capabilities>>,
}

impl std::fmt::Debug for EndpointState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointState")
            .field("name", &self.name)
            .field("head", &self.head())
            .finish_non_exhaustive()
    }
}

impl EndpointState {
    /// The endpoint's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where requests go.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Take a slot in the endpoint's rate limiter, waiting up to `budget`:
    /// `Ok(wait)` to sleep first, or `Err(wait)` when the wait would be
    /// longer than that.
    pub fn acquire(&self, now: Instant, budget: Duration) -> Result<Duration, Duration> {
        self.limiter.try_acquire(now, budget)
    }

    /// The endpoint answered: the limiter may speed up.
    pub fn on_success(&self) {
        self.limiter.on_success();
    }

    /// The endpoint answered 429: the limiter slows down.
    pub fn on_rate_limited(&self, now: Instant) {
        self.limiter.on_rate_limited(now);
    }

    /// The last probed head, if the last probe succeeded.
    pub fn head(&self) -> Option<u64> {
        match self.head.load(Ordering::Relaxed) {
            0 => None,
            head => Some(head),
        }
    }

    /// Record a probe result.
    pub fn set_head(&self, head: Option<u64>) {
        self.head.store(head.unwrap_or(0), Ordering::Relaxed);
    }

    /// Whether the endpoint is tagged for everything in `needs`:
    /// `Some(false)` if any needed capability is known missing, `None` if
    /// none is known missing but at least one is unknown.
    pub fn supports(&self, needs: Needs) -> Option<bool> {
        let caps = self.caps.lock().unwrap_or_else(|e| e.into_inner());
        let caps = caps.as_ref();
        let wanted = [
            (needs.archive, caps.and_then(|c| c.archive)),
            (needs.debug, caps.and_then(|c| c.debug_trace)),
            (needs.trace, caps.and_then(|c| c.trace_block)),
        ];
        let mut unknown = false;
        for (needed, tag) in wanted {
            match (needed, tag) {
                (false, _) | (true, Some(true)) => {}
                (true, Some(false)) => return Some(false),
                (true, None) => unknown = true,
            }
        }
        if unknown { None } else { Some(true) }
    }

    fn set_caps(&self, caps: Option<Capabilities>) {
        *self.caps.lock().unwrap_or_else(|e| e.into_inner()) = caps;
    }
}

/// Per-chain state that survives reloads.
#[derive(Debug, Default)]
struct ChainLive {
    /// The best head of each of the last [`HEAD_ROUNDS`] probe rounds that
    /// had one, oldest first.
    rounds: Mutex<VecDeque<u64>>,
    /// Their maximum: the head high-water mark; 0 = unknown.
    hwm: AtomicU64,
    /// Filter ID → (endpoint that created it, last use).
    filters: Mutex<HashMap<String, (String, Instant)>>,
    /// Unix seconds of the last request; 0 = never.
    last_used: AtomicU64,
    /// When the pool was last probed.
    probed: Mutex<Option<Instant>>,
}

/// One served chain.
#[derive(Debug)]
pub struct ChainState {
    /// Short name.
    name: String,
    /// Chain ID.
    id: u64,
    /// Name of the active endpoint.
    active_name: String,
    /// Names of the failover endpoints, in config order.
    failover_names: Vec<String>,
    /// The active endpoint, or why it can't be routed.
    active: Result<Arc<EndpointState>, String>,
    /// Routable failover endpoints, in config order.
    failover: Vec<Arc<EndpointState>>,
    /// Every configured endpoint by name, or why it can't be routed.
    endpoints: HashMap<String, Result<Arc<EndpointState>, String>>,
    live: Arc<ChainLive>,
}

impl ChainState {
    /// The chain's short name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The chain ID.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The active endpoint's name.
    pub fn active_name(&self) -> &str {
        &self.active_name
    }

    /// What the control API reports for this chain.
    pub fn status(&self) -> ChainStatus {
        ChainStatus {
            chain: self.name.clone(),
            chain_id: self.id,
            active: self.active_name.clone(),
            failover: self.failover_names.clone(),
            head: self.head(),
        }
    }

    /// The active endpoint, or why it can't be routed.
    pub fn active(&self) -> Result<Arc<EndpointState>, String> {
        self.active.clone().map_err(|reason| {
            format!(
                "active endpoint '{}' of chain '{}' can't be routed: {reason}",
                self.active_name, self.name
            )
        })
    }

    /// The endpoint called `name`, or why it can't be routed.
    pub fn endpoint(&self, name: &str) -> Result<Arc<EndpointState>, String> {
        match self.endpoints.get(name) {
            Some(Ok(endpoint)) => Ok(endpoint.clone()),
            Some(Err(reason)) => Err(format!(
                "endpoint '{name}' of chain '{}' can't be routed: {reason}",
                self.name
            )),
            None => Err(format!("chain '{}' has no endpoint '{name}'", self.name)),
        }
    }

    /// The failover endpoints that may take a read now, in config order.
    pub fn current_failovers(&self) -> impl Iterator<Item = &Arc<EndpointState>> {
        self.failover.iter().filter(|e| self.head_is_current(e))
    }

    /// The pool whose heads the gateway probes: only failover pools with a
    /// routable active endpoint need a head high-water mark.
    pub fn probe_pool(&self) -> Option<Vec<Arc<EndpointState>>> {
        (!self.failover.is_empty() && self.active.is_ok()).then(|| self.pool())
    }

    /// The head high-water mark, if any probe has succeeded.
    pub fn head(&self) -> Option<u64> {
        match self.live.hwm.load(Ordering::Relaxed) {
            0 => None,
            head => Some(head),
        }
    }

    /// Record a probe round's best head (`None`: no probe succeeded). The
    /// mark is the best head over the last [`HEAD_ROUNDS`] rounds, so it
    /// never steps back on one failed probe, and a bogus head (a node
    /// briefly reporting a block far ahead) ages out instead of blocking
    /// failover and archive routing for good.
    pub fn record_round(&self, best: Option<u64>) {
        let Some(best) = best else { return };
        let mut rounds = self.live.rounds.lock().unwrap_or_else(|e| e.into_inner());
        if rounds.len() == HEAD_ROUNDS {
            rounds.pop_front();
        }
        rounds.push_back(best);
        let mark = rounds.iter().copied().max().unwrap_or(0);
        self.live.hwm.store(mark, Ordering::Relaxed);
    }

    /// The active endpoint, then the failover list.
    fn pool(&self) -> Vec<Arc<EndpointState>> {
        let mut pool = Vec::with_capacity(1 + self.failover.len());
        pool.extend(self.active.as_ref().ok().cloned());
        pool.extend(self.failover.iter().cloned());
        pool
    }

    /// Whether `endpoint` may take a read that failed over: its head is
    /// known and within [`FAILOVER_LAG`] of the high-water mark.
    fn head_is_current(&self, endpoint: &EndpointState) -> bool {
        match (endpoint.head(), self.head()) {
            (Some(head), Some(hwm)) => head.saturating_add(FAILOVER_LAG) >= hwm,
            _ => false,
        }
    }

    /// Note a request at unix time `now` (seconds).
    pub fn touch(&self, now: u64) {
        self.live.last_used.store(now, Ordering::Relaxed);
    }

    /// Unix seconds of the last request, if any.
    pub fn last_used(&self) -> Option<u64> {
        match self.live.last_used.load(Ordering::Relaxed) {
            0 => None,
            t => Some(t),
        }
    }

    /// Whether the pool was probed less than `age` ago.
    pub fn probed_within(&self, age: std::time::Duration) -> bool {
        self.live
            .probed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|at| at.elapsed() < age)
    }

    /// Note that the pool is being probed now.
    pub fn mark_probed(&self) {
        *self.live.probed.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// Remember which endpoint created a filter.
    pub fn remember_filter(&self, id: &str, endpoint: &str) {
        let mut filters = self.live.filters.lock().unwrap_or_else(|e| e.into_inner());
        if filters.len() >= MAX_FILTERS {
            // Drop the least recently used half.
            let mut ages: Vec<Instant> = filters.values().map(|(_, t)| *t).collect();
            ages.sort_unstable();
            let cutoff = ages[ages.len() / 2];
            filters.retain(|_, (_, used)| *used > cutoff);
        }
        filters.insert(
            id.to_ascii_lowercase(),
            (endpoint.to_string(), Instant::now()),
        );
    }

    /// The endpoint that created filter `id`, if known.
    pub fn filter_endpoint(&self, id: &str) -> Option<String> {
        let mut filters = self.live.filters.lock().unwrap_or_else(|e| e.into_inner());
        let entry = filters.get_mut(id)?;
        entry.1 = Instant::now();
        Some(entry.0.clone())
    }

    /// Forget an uninstalled filter.
    pub fn forget_filter(&self, id: &str) {
        self.live
            .filters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }
}

/// The files a snapshot was built from, as they were just before it was
/// read: the watcher reloads when they change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamps {
    /// `config.toml`.
    pub config: Option<FileStamp>,
    /// `secrets.toml`.
    pub secrets: Option<FileStamp>,
    /// `health.json` (capability tags).
    pub health: Option<FileStamp>,
}

impl Stamps {
    /// The files as they are now.
    pub fn take(paths: &StorePaths, state_dir: Option<&Path>) -> Stamps {
        Stamps {
            config: FileStamp::of(paths.config()),
            secrets: FileStamp::of(paths.secrets()),
            health: state_dir.and_then(|dir| FileStamp::of(&HealthStore::path(dir))),
        }
    }
}

/// Everything the router needs, built from one read of the config.
pub struct Snapshot {
    /// Served chains, in config order.
    pub chains: Vec<Arc<ChainState>>,
    /// The config read, to resolve chain references as the CLI does.
    config: Config,
    /// Chain name → index into `chains`.
    index: HashMap<String, usize>,
    /// Reverse labels for the inspector.
    pub labels: LabelIndex,
    /// Endpoints that can't be routed, as `chain.endpoint: reason`
    /// (websockets excluded: those are expected), and an unreadable health
    /// store.
    pub warnings: Vec<String>,
    /// The files as they were just before they were read.
    pub stamps: Stamps,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("chains", &self.chains)
            .field("labels", &self.labels)
            .field("warnings", &self.warnings)
            .field("stamps", &self.stamps)
            .finish_non_exhaustive()
    }
}

impl Snapshot {
    /// Load the config at `paths` and build a snapshot, reusing live state
    /// from `previous` where the chain or endpoint is unchanged.
    pub fn load(
        paths: &StorePaths,
        state_dir: Option<&Path>,
        previous: Option<&Snapshot>,
    ) -> Result<Snapshot> {
        // Stamped before the read, so the watcher sees a change made
        // while it runs.
        let stamps = Stamps::take(paths, state_dir);
        let chainz = Chainz::open_at(paths.clone(), LoadMode::Strict)?;
        let config = chainz.config();
        // Loading may tighten a file's permissions: say so with the rest.
        let mut warnings: Vec<String> = chainz
            .take_notices()
            .iter()
            .map(ToString::to_string)
            .collect();
        let health = state_dir.and_then(|dir| {
            HealthStore::load(dir)
                .map_err(|e| warnings.push(format!("capability tags unavailable: {e}")))
                .ok()
        });
        let mut chains = Vec::with_capacity(config.chains.len());
        let mut index = HashMap::with_capacity(config.chains.len());
        for (name, chain) in &config.chains {
            // The same chain: same name and same ID. A chain whose ID
            // changed starts afresh (its heads and filters are another
            // chain's).
            let old = previous.and_then(|p| {
                p.chains
                    .iter()
                    .find(|o| o.name == *name && o.id == chain.id)
            });
            let mut endpoints = HashMap::with_capacity(chain.rpcs.len());
            for (endpoint_name, endpoint) in &chain.rpcs {
                let built = if endpoint.is_websocket() {
                    Err("it is a websocket endpoint; the gateway routes HTTP only".to_string())
                } else {
                    build_target(&chainz, name, chain, endpoint_name, endpoint)
                        .map(|(target, fingerprint)| {
                            let reused = old
                                .and_then(|o| o.endpoints.get(endpoint_name))
                                .and_then(|e| e.as_ref().ok())
                                .filter(|e| e.fingerprint == fingerprint)
                                .cloned();
                            reused.unwrap_or_else(|| {
                                Arc::new(EndpointState {
                                    name: endpoint_name.clone(),
                                    target,
                                    fingerprint,
                                    limiter: AimdLimiter::new(Instant::now()),
                                    head: AtomicU64::new(0),
                                    caps: Mutex::new(None),
                                })
                            })
                        })
                        .map_err(|e| {
                            let reason = format!("{e:#}");
                            warnings.push(format!("{name}.{endpoint_name}: {reason}"));
                            reason
                        })
                };
                if let Ok(state) = &built {
                    state.set_caps(
                        health
                            .as_ref()
                            .and_then(|h| h.capabilities(chain.id, endpoint_name))
                            .cloned(),
                    );
                }
                endpoints.insert(endpoint_name.clone(), built);
            }
            let active = endpoints
                .get(&chain.rpc)
                .cloned()
                .unwrap_or_else(|| Err("it is not defined".to_string()));
            let failover = chain
                .failover
                .iter()
                .filter_map(|f| endpoints.get(f)?.as_ref().ok().cloned())
                .collect();
            index.insert(name.clone(), chains.len());
            chains.push(Arc::new(ChainState {
                name: name.clone(),
                id: chain.id,
                active_name: chain.rpc.clone(),
                failover_names: chain.failover.clone(),
                active,
                failover,
                endpoints,
                live: old.map(|o| o.live.clone()).unwrap_or_default(),
            }));
        }
        Ok(Snapshot {
            chains,
            index,
            labels: LabelIndex::new(config),
            config: config.clone(),
            warnings,
            stamps,
        })
    }

    /// A chain by ID, name or alias (any case), never a prefix: core's
    /// [`resolve::exact`], so a gateway path names what the CLI would.
    pub fn chain(&self, reference: &str) -> Option<&Arc<ChainState>> {
        let (name, _) = resolve::exact(&self.config, reference).ok()??;
        self.chains.get(*self.index.get(name)?)
    }

    /// Re-read capability tags from the health store.
    pub fn refresh_capabilities(&self, state_dir: &Path) -> std::io::Result<()> {
        let health = HealthStore::load(state_dir)?;
        for chain in &self.chains {
            for (name, endpoint) in &chain.endpoints {
                if let Ok(endpoint) = endpoint {
                    endpoint.set_caps(health.capabilities(chain.id, name).cloned());
                }
            }
        }
        Ok(())
    }
}

/// An endpoint as a target plus its fingerprint, through core's config→RPC
/// bridge (`${VAR}`s expanded, never the gateway itself).
fn build_target(
    chainz: &Chainz,
    chain_name: &str,
    chain: &Chain,
    endpoint_name: &str,
    endpoint: &Endpoint,
) -> Result<(Target, String)> {
    let route = chainz.direct_route(chain_name, chain.id, endpoint_name, endpoint)?;
    let mut fingerprint = format!("{}\n{}", route.chain_id, route.url);
    for (name, value) in &route.headers {
        fingerprint.push_str(&format!("\n{name}: {value}"));
    }
    let target = Target::new(route.chain_id, &route.endpoint, &route.url, &route.headers)?;
    Ok((target, fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, config: &str, secrets: &str) -> StorePaths {
        fs::write(dir.join("config.toml"), config).unwrap();
        fs::write(dir.join("secrets.toml"), secrets).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for f in ["config.toml", "secrets.toml"] {
            fs::set_permissions(dir.join(f), fs::Permissions::from_mode(0o600)).unwrap();
        }
        StorePaths::in_dir(dir.to_path_buf())
    }

    const CONFIG: &str = r#"
version = 2
[chains.eth]
id = 1
aliases = ["Mainnet"]
rpc = "main"
failover = ["backup"]
[chains.eth.rpcs]
main = { url = "http://127.0.0.1:1/${GATEWAY_TEST_KEY}", headers = { x-secret = "${GATEWAY_TEST_KEY}" } }
backup = "http://127.0.0.1:2"
sock = "wss://127.0.0.1:3"
broken = "http://127.0.0.1:4/${GATEWAY_TEST_MISSING}"
[chains.ws-only]
id = 5
rpc = "sock"
rpcs.sock = "ws://127.0.0.1:3"
"#;

    #[test]
    fn builds_pools_and_carries_live_state_across_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write(
            dir.path(),
            CONFIG,
            "[vars]\nGATEWAY_TEST_KEY = \"sekrit-key\"\n",
        );
        let snap = Snapshot::load(&paths, None, None).unwrap();
        let eth = snap.chain("eth").unwrap();
        assert!(Arc::ptr_eq(eth, snap.chain("MAINNET").unwrap()));
        assert!(Arc::ptr_eq(eth, snap.chain("1").unwrap()));
        assert!(snap.chain("et").is_none(), "no prefix matching");
        assert_eq!(eth.active_name, "main");
        let pool: Vec<_> = eth.pool().iter().map(|e| e.name.clone()).collect();
        assert_eq!(pool, ["main", "backup"]);
        assert!(
            eth.endpoints["sock"]
                .as_ref()
                .unwrap_err()
                .contains("websocket")
        );
        let broken = eth.endpoints["broken"].as_ref().unwrap_err();
        assert!(broken.contains("GATEWAY_TEST_MISSING"), "{broken}");
        assert_eq!(snap.warnings.len(), 1, "{:?}", snap.warnings);
        let ws = snap.chain("ws-only").unwrap();
        assert!(ws.active.is_err() && ws.pool().is_empty());
        assert!(!format!("{snap:?}").contains("sekrit-key"));

        // Live state.
        eth.record_round(Some(100));
        eth.remember_filter("0xAB", "main");
        let main = eth.active.clone().unwrap();
        main.set_head(Some(100));
        main.on_rate_limited(Instant::now());
        let backup = eth.failover[0].clone();
        assert!(!eth.head_is_current(&backup), "unknown head");
        backup.set_head(Some(97));
        assert!(!eth.head_is_current(&backup), "3 behind");
        backup.set_head(Some(98));
        assert!(eth.head_is_current(&backup));

        // Reload with `backup` changed and `main` untouched.
        let paths = write(
            dir.path(),
            &CONFIG.replace("http://127.0.0.1:2", "http://127.0.0.1:22"),
            "[vars]\nGATEWAY_TEST_KEY = \"sekrit-key\"\n",
        );
        let next = Snapshot::load(&paths, None, Some(&snap)).unwrap();
        let eth2 = next.chain("eth").unwrap();
        assert_eq!(eth2.head(), Some(100));
        assert_eq!(eth2.filter_endpoint("0xab").as_deref(), Some("main"));
        assert!(Arc::ptr_eq(&main, eth2.active.as_ref().unwrap()));
        assert!(eth2.active.as_ref().unwrap().limiter.rate().is_some());
        assert!(!Arc::ptr_eq(&backup, &eth2.failover[0]));
        assert_eq!(eth2.failover[0].head(), None);

        // A changed secret replaces the endpoint.
        let paths = write(
            dir.path(),
            CONFIG,
            "[vars]\nGATEWAY_TEST_KEY = \"rotated\"\n",
        );
        let rotated = Snapshot::load(&paths, None, Some(&next)).unwrap();
        assert!(!Arc::ptr_eq(
            &main,
            rotated.chain("eth").unwrap().active.as_ref().unwrap()
        ));

        // Same name, new chain ID: nothing carries over.
        let eth3 = rotated.chain("eth").unwrap();
        eth3.remember_filter("0xcd", "main");
        let paths = write(
            dir.path(),
            &CONFIG.replace("id = 1\n", "id = 10\n"),
            "[vars]\nGATEWAY_TEST_KEY = \"rotated\"\n",
        );
        let moved = Snapshot::load(&paths, None, Some(&rotated)).unwrap();
        let eth4 = moved.chain("eth").unwrap();
        assert_eq!(eth4.id, 10);
        assert_eq!(eth4.head(), None);
        assert_eq!(eth4.filter_endpoint("0xcd"), None);
        assert!(!Arc::ptr_eq(
            eth3.active.as_ref().unwrap(),
            eth4.active.as_ref().unwrap()
        ));
        assert_eq!(eth4.active.as_ref().unwrap().target.chain_id(), 10);
    }

    #[test]
    fn a_bogus_head_ages_out_of_the_mark() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write(dir.path(), CONFIG, "[vars]\nGATEWAY_TEST_KEY = \"k\"\n");
        let snap = Snapshot::load(&paths, None, None).unwrap();
        let eth = snap.chain("eth").unwrap();
        let backup = eth.failover[0].clone();
        backup.set_head(Some(100));
        eth.record_round(Some(100));
        assert!(eth.head_is_current(&backup));

        // One round with a wild head: failover stops...
        eth.record_round(Some(1_000_000_000));
        assert_eq!(eth.head(), Some(1_000_000_000));
        assert!(!eth.head_is_current(&backup));
        // ...a round with no head at all changes nothing...
        eth.record_round(None);
        assert_eq!(eth.head(), Some(1_000_000_000));
        // ...and sane rounds push it out.
        for head in [101, 101, 102] {
            eth.record_round(Some(head));
        }
        assert_eq!(eth.head(), Some(102));
        backup.set_head(Some(101));
        assert!(eth.head_is_current(&backup));
        // A lower best head than earlier rounds doesn't drop the mark.
        eth.record_round(Some(90));
        assert_eq!(eth.head(), Some(102));
    }

    #[test]
    fn capability_tags() {
        let endpoint = EndpointState {
            name: "a".into(),
            target: Target::new(1, "a", "http://127.0.0.1:1", &[]).unwrap(),
            fingerprint: String::new(),
            limiter: AimdLimiter::new(Instant::now()),
            head: AtomicU64::new(0),
            caps: Mutex::new(None),
        };
        let archive = Needs {
            archive: true,
            ..Needs::default()
        };
        let both = Needs {
            archive: true,
            debug: true,
            trace: false,
        };
        assert_eq!(endpoint.supports(Needs::default()), Some(true));
        assert_eq!(endpoint.supports(archive), None);
        endpoint.set_caps(Some(Capabilities {
            archive: Some(true),
            ..Capabilities::default()
        }));
        assert_eq!(endpoint.supports(archive), Some(true));
        assert_eq!(endpoint.supports(both), None);
        endpoint.set_caps(Some(Capabilities {
            archive: Some(true),
            debug_trace: Some(false),
            ..Capabilities::default()
        }));
        assert_eq!(endpoint.supports(both), Some(false));
    }
}
