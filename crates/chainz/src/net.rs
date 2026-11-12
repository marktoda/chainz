//! Bridge from config to `chainz-rpc`: async runtime, targets, health store.
//! Never used by `exec`, `env`, `ls` or `shell`.

use anyhow::{Context, Result, anyhow};
use chainz_core::model::Endpoint;
use chainz_core::route::Route;
use chainz_core::{Chainz, paths};
use chainz_rpc::{HealthStore, Target};

/// Run a future on a fresh single-threaded runtime.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
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

/// The persisted endpoint health store (empty when unreadable).
pub fn health_store() -> HealthStore {
    HealthStore::load_or_empty(paths::state_dir().ok().as_deref())
}

/// The chainlist cache directory.
pub fn cache_dir() -> Result<std::path::PathBuf> {
    paths::cache_dir().context("unable to find the cache directory")
}
