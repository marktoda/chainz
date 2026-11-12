//! Bridge from config to `chainz-rpc`: async runtime, targets, health store.
//! Never used by `exec`, `env`, `ls` or `shell`.

use anyhow::{Result, anyhow};
use chainz_core::model::Endpoint;
use chainz_core::route::Route;
use chainz_core::{Chainz, paths};
use chainz_rpc::chainlist::{self, ChainlistError, Registry};
use chainz_rpc::{HealthStore, Target};
use std::future::Future;
use std::path::Path;

use crate::ui;

/// Run a future on a fresh single-threaded runtime.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

/// Run `futures` concurrently; their outputs in input order. A task that
/// panicked is an error.
pub async fn join_indexed<F>(futures: Vec<F>) -> Result<Vec<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for (i, f) in futures.into_iter().enumerate() {
        set.spawn(async move { (i, f.await) });
    }
    let mut out: Vec<Option<F::Output>> = (0..set.len()).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        let (i, v) = joined.map_err(|e| anyhow!("a concurrent task failed: {e}"))?;
        out[i] = Some(v);
    }
    Ok(out.into_iter().flatten().collect())
}

/// A probe target for an endpoint of chain `chain_name` (ID `chain_id`),
/// `${VAR}`s expanded. The chain need not be configured yet.
pub fn target(
    chainz: &Chainz,
    chain_name: &str,
    chain_id: u64,
    endpoint_name: &str,
    endpoint: &Endpoint,
) -> Result<Target> {
    to_target(&chainz.direct_route(chain_name, chain_id, endpoint_name, endpoint)?)
}

/// The `chainz-rpc` target for a route: the one adapter.
pub fn to_target(route: &Route) -> Result<Target> {
    Target::new(route.chain_id, &route.endpoint, &route.url, &route.headers)
        .map_err(|e| anyhow!("chain '{}' endpoint '{}': {e}", route.chain, route.endpoint))
}

/// The state directory (endpoint health history, capability tags), or
/// `None` with a warning on stderr when there is none.
pub fn state_dir() -> Option<std::path::PathBuf> {
    paths::state_dir()
        .map_err(|e| {
            eprintln!(
                "{}",
                ui::warn(&format!("no endpoint health history: {e:#}"))
            );
        })
        .ok()
}

/// The persisted endpoint health store. When it can't be read, that is
/// said on stderr and an empty store is used (one that still saves, when
/// there is a state directory).
pub fn health_store() -> HealthStore {
    let Some(dir) = state_dir() else {
        return HealthStore::empty();
    };
    HealthStore::load(&dir).unwrap_or_else(|e| {
        eprintln!(
            "{}",
            ui::warn(&format!("could not read the endpoint health history: {e}"))
        );
        HealthStore::new(&dir)
    })
}

/// The chain registry (chainid.network), from the cache under `cache` when
/// it is fresh and `refresh` is off; a download that could not be cached
/// is said on stderr.
pub fn chainlist(cache: &Path, refresh: bool) -> Result<Registry, ChainlistError> {
    let registry = block_on(chainlist::fetch_all(cache, refresh))?;
    if let Some(warning) = &registry.cache_warning {
        eprintln!("{}", ui::warn(warning));
    }
    Ok(registry)
}
