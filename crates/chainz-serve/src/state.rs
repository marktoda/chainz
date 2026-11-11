//! The gateway's view of the config: chains, pools and per-endpoint state.
//!
//! A [`Snapshot`] is immutable and replaced wholesale on reload. Live state
//! (limiters, heads, sticky filters) is carried across reloads when the
//! chain or endpoint is unchanged.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use chainz_core::labels::LabelIndex;
use chainz_core::model::{Chain, Endpoint};
use chainz_core::secrets::{Builtins, VarResolver};
use chainz_core::{Chainz, LoadMode, StorePaths};
use chainz_rpc::{AimdLimiter, Capabilities, HealthStore, Target};

use crate::classify::Needs;

/// A failover target must be within this many blocks of the chain's head
/// high-water mark.
pub const MAX_HEAD_LAG: u64 = 2;
/// Sticky filter entries kept per chain.
const MAX_FILTERS: usize = 4096;

/// One routable (HTTP) endpoint.
pub struct EndpointState {
    /// Endpoint name.
    pub name: String,
    /// Expanded URL and headers.
    pub target: Target,
    /// URL and headers, to detect changes on reload. Secret: never printed.
    fingerprint: String,
    /// Adaptive rate limiter.
    pub limiter: AimdLimiter,
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
pub struct ChainLive {
    /// Head high-water mark; 0 = unknown.
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
    pub name: String,
    /// Chain ID.
    pub id: u64,
    /// Name of the active endpoint.
    pub active_name: String,
    /// Names of the failover endpoints, in config order.
    pub failover_names: Vec<String>,
    /// The active endpoint, or why it can't be routed.
    pub active: Result<Arc<EndpointState>, String>,
    /// Routable failover endpoints, in config order.
    pub failover: Vec<Arc<EndpointState>>,
    /// Every configured endpoint by name, or why it can't be routed.
    pub endpoints: HashMap<String, Result<Arc<EndpointState>, String>>,
    live: Arc<ChainLive>,
}

impl ChainState {
    /// The head high-water mark, if any probe has succeeded.
    pub fn head(&self) -> Option<u64> {
        match self.live.hwm.load(Ordering::Relaxed) {
            0 => None,
            head => Some(head),
        }
    }

    /// Raise the high-water mark.
    pub fn observe_head(&self, head: u64) {
        self.live.hwm.fetch_max(head, Ordering::Relaxed);
    }

    /// The active endpoint, then the failover list.
    pub fn pool(&self) -> Vec<Arc<EndpointState>> {
        let mut pool = Vec::with_capacity(1 + self.failover.len());
        pool.extend(self.active.as_ref().ok().cloned());
        pool.extend(self.failover.iter().cloned());
        pool
    }

    /// Whether `endpoint` may take a read that failed over: its head is
    /// known and within [`MAX_HEAD_LAG`] of the high-water mark.
    pub fn head_is_current(&self, endpoint: &EndpointState) -> bool {
        match (endpoint.head(), self.head()) {
            (Some(head), Some(hwm)) => head.saturating_add(MAX_HEAD_LAG) >= hwm,
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

/// Everything the router needs, built from one read of the config.
#[derive(Debug)]
pub struct Snapshot {
    /// Served chains, in config order.
    pub chains: Vec<Arc<ChainState>>,
    lookup: HashMap<String, usize>,
    /// Reverse labels for the inspector.
    pub labels: LabelIndex,
    /// Endpoints that can't be routed, as `chain.endpoint: reason`
    /// (websockets excluded: those are expected).
    pub warnings: Vec<String>,
}

impl Snapshot {
    /// Load the config at `paths` and build a snapshot, reusing live state
    /// from `previous` where the chain or endpoint is unchanged.
    pub fn load(
        paths: &StorePaths,
        state_dir: Option<&Path>,
        previous: Option<&Snapshot>,
    ) -> Result<Snapshot> {
        let chainz = Chainz::open_at(paths.clone(), LoadMode::Strict)?;
        let config = chainz.config();
        let health = state_dir.and_then(|dir| HealthStore::load(dir).ok());
        let mut chains = Vec::with_capacity(config.chains.len());
        let mut lookup = HashMap::new();
        let mut warnings = Vec::new();
        for (name, chain) in &config.chains {
            let old = previous.and_then(|p| p.chain(name));
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
            let index = chains.len();
            lookup.insert(name.to_ascii_lowercase(), index);
            for alias in &chain.aliases {
                lookup.entry(alias.to_ascii_lowercase()).or_insert(index);
            }
            lookup.entry(chain.id.to_string()).or_insert(index);
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
            lookup,
            labels: LabelIndex::new(config),
            warnings,
        })
    }

    /// A chain by name, alias or ID (case-insensitive, exact).
    pub fn chain(&self, reference: &str) -> Option<&Arc<ChainState>> {
        let index = match self.lookup.get(reference) {
            Some(index) => *index,
            None => *self.lookup.get(&reference.to_ascii_lowercase())?,
        };
        self.chains.get(index)
    }

    /// Re-read capability tags from the health store.
    pub fn refresh_capabilities(&self, state_dir: &Path) {
        let Ok(health) = HealthStore::load(state_dir) else {
            return;
        };
        for chain in &self.chains {
            for (name, endpoint) in &chain.endpoints {
                if let Ok(endpoint) = endpoint {
                    endpoint.set_caps(health.capabilities(chain.id, name).cloned());
                }
            }
        }
    }
}

/// Expand an endpoint's `${VAR}`s into a target plus its fingerprint.
fn build_target(
    chainz: &Chainz,
    chain_name: &str,
    chain: &Chain,
    endpoint_name: &str,
    endpoint: &Endpoint,
) -> Result<(Target, String)> {
    let loader = || chainz.secrets();
    let vars = VarResolver::new(
        Builtins {
            chain_id: Some(chain.id),
            chain: Some(chain_name),
        },
        &loader,
    );
    let url = vars.expand(&endpoint.url)?;
    let mut fingerprint = url.to_string();
    let mut headers = Vec::with_capacity(endpoint.headers.len());
    for (name, value) in &endpoint.headers {
        let value = vars
            .expand(value)
            .with_context(|| format!("header '{name}'"))?;
        fingerprint.push_str(&format!("\n{name}: {value}"));
        headers.push((name.clone(), value.into_owned()));
    }
    let target = Target::new(chain.id, endpoint_name, &url, &headers)?;
    Ok((target, fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, config: &str, secrets: &str) -> StorePaths {
        fs::write(dir.join("config.toml"), config).unwrap();
        fs::write(dir.join("secrets.toml"), secrets).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in ["config.toml", "secrets.toml"] {
                fs::set_permissions(dir.join(f), fs::Permissions::from_mode(0o600)).unwrap();
            }
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
        eth.observe_head(100);
        eth.remember_filter("0xAB", "main");
        let main = eth.active.clone().unwrap();
        main.set_head(Some(100));
        main.limiter.on_rate_limited(Instant::now());
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
