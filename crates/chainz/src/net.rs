//! Bridge from config to `chainz-rpc`'s network side: the async runtime,
//! targets and the chain registry. Never used by `exec`, `env` or `ls`
//! (the recorded endpoint health `ls` shows is [`crate::load`]'s).

use anyhow::{Result, anyhow};
use chainz_core::Chainz;
use chainz_core::model::Endpoint;
use chainz_core::route::Route;
use chainz_rpc::Target;
use chainz_rpc::chainlist::{self, ChainlistError, Registry};
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
