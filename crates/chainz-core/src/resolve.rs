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
//! alias. Each command matches under one [`Policy`]; destructive commands
//! use [`Policy::Exact`]: name or ID only.

use crate::contract::Env;
use crate::model::{Chain, Config};
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
    /// The chain ID labels and tools use: the fork's, or the configured
    /// chain's (`None` if `config` no longer has it).
    pub fn chain_id(&self, config: &Config) -> Option<u64> {
        match self {
            ChainRef::Configured(name) => config.chains.get(name).map(|c| c.id),
            ChainRef::Fork(fork) => Some(fork.chain_id),
        }
    }

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
    configured(config, query, Policy::Prefix)
        .map(|(name, _)| ChainRef::Configured(name.to_string()))
}

/// How strictly a command matches a configured chain. Each command keeps
/// one policy; the matching rules live here only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// An ID, an exact name or alias (any case), then an unambiguous prefix
    /// of a name or alias: management commands.
    Prefix,
    /// An ID or the exact name: destructive commands (`chain rm`).
    Exact,
    /// An ID, the exact name, or an alias (any case), never a prefix:
    /// `addr rm --chain`.
    ExactOrAlias,
}

/// The configured chain (no forks) `query` names under `policy`: its short
/// name and the chain.
pub fn configured<'c>(
    config: &'c Config,
    query: &str,
    policy: Policy,
) -> Result<(&'c str, &'c Chain)> {
    let found = |name: &'c String, chain: &'c Chain| (name.as_str(), chain);
    if let Ok(id) = query.parse::<u64>() {
        if let Some(found) = config.chain_by_id(id) {
            return Ok(found);
        }
        if policy == Policy::Prefix {
            bail!("no chain with ID {id} is configured{}", hint(config));
        }
    }
    if policy == Policy::Exact {
        return config
            .chains
            .get_key_value(query)
            .map(|(n, c)| found(n, c))
            .ok_or_else(|| {
                anyhow!("chain '{query}' not found; this command needs an exact chain name or ID")
            });
    }
    for (name, chain) in &config.chains {
        if name.eq_ignore_ascii_case(query) || chain.matches_alias(query) {
            if policy == Policy::ExactOrAlias && !name.eq(query) && !chain.matches_alias(query) {
                continue;
            }
            return Ok(found(name, chain));
        }
    }
    if policy == Policy::ExactOrAlias {
        bail!("chain '{query}' not found; this command needs an exact chain name or ID");
    }
    let lower = query.to_ascii_lowercase();
    let matches: Vec<(&'c String, &'c Chain)> = config
        .chains
        .iter()
        .filter(|(name, chain)| {
            name.starts_with(&lower)
                || chain
                    .aliases
                    .iter()
                    .any(|a| a.to_ascii_lowercase().starts_with(&lower))
        })
        .collect();
    match matches.as_slice() {
        [(name, chain)] => Ok(found(name, chain)),
        [] => Err(anyhow!("chain '{query}' not found{}", hint(config))),
        many => Err(anyhow!(
            "chain '{query}' is ambiguous: matches {}",
            many.iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
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
        let exact = |q| configured(&c, q, Policy::Exact).map(|(n, _)| n);
        assert_eq!(exact("8453").unwrap(), "base");
        assert!(exact("mainnet").is_err());
        assert!(exact("bas").is_err());
        assert!(exact("BASE").is_err());
        let alias = |q| configured(&c, q, Policy::ExactOrAlias).map(|(n, _)| n);
        assert_eq!(alias("1").unwrap(), "eth");
        assert_eq!(alias("MAINNET").unwrap(), "eth");
        assert_eq!(alias("base").unwrap(), "base");
        assert!(alias("BASE").is_err());
        assert!(alias("main").is_err());
        let (name, chain) = configured(&c, "base-s", Policy::Prefix).unwrap();
        assert_eq!((name, chain.id), ("base-sepolia", 84532));
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
