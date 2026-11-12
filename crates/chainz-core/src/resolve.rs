//! Chain-reference resolution.
//!
//! Which chain a command means, first match wins:
//! 1. the explicit argument;
//! 2. `$CHAINZ_CHAIN` (set by `chainz shell` and `chainz env`);
//! 3. `default` in config (callers print `chainz: <name> (default)`);
//! 4. otherwise the caller opens a picker on a TTY, or fails.
//!
//! Within a reference: a live fork name, an exact chain ID, an exact name
//! or alias (case-insensitive), then an unambiguous prefix of a name or
//! alias. Destructive commands use [`resolve_exact`]: name or ID only.

use crate::contract::Env;
use crate::model::Config;
use crate::runtime::{ForkRecord, Runtime};
use anyhow::{Result, anyhow, bail};

/// What a reference resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainRef {
    /// A configured chain, by its short name.
    Configured(String),
    /// A live fork from the runtime registry.
    Fork(ForkRecord),
}

impl ChainRef {
    /// The name exported as `CHAINZ_CHAIN`.
    pub fn name(&self) -> &str {
        match self {
            ChainRef::Configured(name) => name,
            ChainRef::Fork(fork) => &fork.name,
        }
    }
}

/// Where the chain came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The command-line argument.
    Explicit,
    /// `$CHAINZ_CHAIN`.
    Env,
    /// `default` in config.
    Default,
}

/// Resolve one reference. `runtime` enables fork names.
pub fn resolve(config: &Config, runtime: Option<&Runtime>, query: &str) -> Result<ChainRef> {
    let query = query.trim();
    if query.is_empty() {
        bail!("empty chain reference");
    }
    if let Some(runtime) = runtime
        && crate::names::is_valid_chain_name(query)
        && let Ok(Some(fork)) = runtime.read_fork(query)
    {
        return Ok(ChainRef::Fork(fork));
    }
    resolve_configured(config, query).map(ChainRef::Configured)
}

/// Resolve to a configured chain's short name (no forks).
pub fn resolve_configured(config: &Config, query: &str) -> Result<String> {
    if let Ok(id) = query.parse::<u64>() {
        if let Some((name, _)) = config.chain_by_id(id) {
            return Ok(name.to_string());
        }
        bail!("no chain with ID {id} is configured{}", hint(config));
    }
    for (name, chain) in &config.chains {
        if name.eq_ignore_ascii_case(query) || chain.matches_alias(query) {
            return Ok(name.clone());
        }
    }
    let lower = query.to_ascii_lowercase();
    let matches: Vec<&str> = config
        .chains
        .iter()
        .filter(|(name, chain)| {
            name.starts_with(&lower)
                || chain
                    .aliases
                    .iter()
                    .any(|a| a.to_ascii_lowercase().starts_with(&lower))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.to_string()),
        [] => Err(anyhow!("chain '{query}' not found{}", hint(config))),
        many => Err(anyhow!(
            "chain '{query}' is ambiguous: matches {}",
            many.join(", ")
        )),
    }
}

/// Exact name or ID only, for destructive commands.
pub fn resolve_exact(config: &Config, query: &str) -> Result<String> {
    if let Ok(id) = query.parse::<u64>()
        && let Some((name, _)) = config.chain_by_id(id)
    {
        return Ok(name.to_string());
    }
    config
        .chains
        .keys()
        .find(|name| name.as_str() == query)
        .cloned()
        .ok_or_else(|| {
            anyhow!("chain '{query}' not found; this command needs an exact chain name or ID")
        })
}

fn hint(config: &Config) -> String {
    if config.chains.is_empty() {
        " (no chains configured; add one with `chainz chain add`)".to_string()
    } else {
        String::new()
    }
}

/// Steps 1–3 of the resolution order. `Ok(None)` means nothing chose a
/// chain: the caller opens a picker (TTY) or fails.
pub fn select(
    config: &Config,
    runtime: Option<&Runtime>,
    explicit: Option<&str>,
    env: Env,
) -> Result<Option<(ChainRef, Source)>> {
    if let Some(query) = explicit {
        return resolve(config, runtime, query).map(|c| Some((c, Source::Explicit)));
    }
    if let Some(env) = env.chainz_chain() {
        return resolve(config, runtime, &env)
            .map(|c| Some((c, Source::Env)))
            .map_err(|e| e.context("from $CHAINZ_CHAIN"));
    }
    if let Some(default) = &config.default {
        return Ok(Some((
            ChainRef::Configured(default.clone()),
            Source::Default,
        )));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        crate::model::Config::parse(
            r#"
version = 2
[chains.eth]
id = 1
aliases = ["Ethereum Mainnet", "mainnet"]
rpc = "a"
rpcs.a = "https://a"
[chains.base]
id = 8453
rpc = "a"
rpcs.a = "https://a"
[chains.base-sepolia]
id = 84532
rpc = "a"
rpcs.a = "https://a"
"#,
        )
        .unwrap()
    }

    #[test]
    fn id_name_alias_prefix() {
        let c = config();
        let r = |q| resolve(&c, None, q).map(|r| r.name().to_string());
        assert_eq!(r("1").unwrap(), "eth");
        assert_eq!(r("ETH").unwrap(), "eth");
        assert_eq!(r("ethereum mainnet").unwrap(), "eth");
        assert_eq!(r("main").unwrap(), "eth");
        assert_eq!(r("base").unwrap(), "base", "exact beats prefix");
        assert!(r("ba").unwrap_err().to_string().contains("ambiguous"));
        assert!(r("999").is_err());
        assert!(r("nope").unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn exact_only_for_destructive_commands() {
        let c = config();
        assert_eq!(resolve_exact(&c, "8453").unwrap(), "base");
        assert!(resolve_exact(&c, "mainnet").is_err());
        assert!(resolve_exact(&c, "bas").is_err());
    }

    #[test]
    fn forks_resolve_by_name_only() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        rt.write_fork(&crate::runtime::ForkRecord {
            name: "base-fork".into(),
            pid: 1,
            port: 8601,
            parent: "base".into(),
            chain_id: 8453,
            fork_block: None,
            started_at: 0,
        })
        .unwrap();
        let c = config();
        assert!(matches!(
            resolve(&c, Some(&rt), "base-fork").unwrap(),
            ChainRef::Fork(_)
        ));
        assert_eq!(
            resolve(&c, Some(&rt), "8453").unwrap(),
            ChainRef::Configured("base".into())
        );
    }
}
