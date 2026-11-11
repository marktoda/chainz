//! Bridge from config to `chainz-rpc`: async runtime, targets, health store.
//! Never used by `exec`, `env`, `ls` or `shell`.

use anyhow::{Context, Result};
use chainz_core::model::{Chain, Endpoint};
use chainz_core::secrets::{Builtins, VarResolver};
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

/// A probe target for a configured endpoint, `${VAR}`s expanded.
pub fn target(
    chainz: &Chainz,
    chain_name: &str,
    chain: &Chain,
    endpoint_name: &str,
    endpoint: &Endpoint,
) -> Result<Target> {
    let loader = || chainz.secrets();
    let vars = VarResolver::new(
        Builtins {
            chain_id: Some(chain.id),
            chain: Some(chain_name),
        },
        &loader,
    );
    let url = vars
        .expand(&endpoint.url)
        .with_context(|| format!("chain '{chain_name}' endpoint '{endpoint_name}'"))?;
    let mut headers = Vec::new();
    for (name, value) in &endpoint.headers {
        let value = vars.expand(value).with_context(|| {
            format!("chain '{chain_name}' endpoint '{endpoint_name}' header '{name}'")
        })?;
        headers.push((name.clone(), value.into_owned()));
    }
    Target::new(chain.id, endpoint_name, &url, &headers)
        .map_err(|e| anyhow::anyhow!("chain '{chain_name}' endpoint '{endpoint_name}': {e}"))
}

/// The persisted endpoint health store (empty when unreadable).
pub fn health_store() -> HealthStore {
    paths::state_dir()
        .ok()
        .and_then(|dir| HealthStore::load(&dir).ok())
        .unwrap_or_else(|| {
            HealthStore::load(std::path::Path::new("/nonexistent/chainz")).expect("empty store")
        })
}

/// The chainlist cache directory.
pub fn cache_dir() -> Result<std::path::PathBuf> {
    paths::cache_dir().context("unable to find the cache directory")
}
