//! Chain-reference resolution.
//!
//! Which chain a command means, first match wins:
//! 1. the explicit argument;
//! 2. `$CHAINZ_CHAIN` (set by `chainz shell` and `chainz env`);
//! 3. `default` in config (callers print `chainz: <name> (default)`);
//! 4. otherwise the caller opens a picker on a TTY, or fails.
//!
//! Within a reference: an exact chain ID, an exact name or alias
//! (case-insensitive), then a well-known long name as an implicit alias
//! ([`implicit_alias`]: `arbitrum` finds the chain with ID 42161), then a
//! live fork's exact name, then an unambiguous prefix of a name or alias. Configured chains come first: a fork never
//! shadows one, and a dead fork never resolves. Each command matches under one [`Policy`]; destructive commands
//! use [`Policy::Exact`]: name or ID only.

use crate::contract::Env;
use crate::model::{Chain, Config};
use crate::runtime::{ForkRecord, Runtime};
use anyhow::{Context as _, Result, anyhow, bail};

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
    if let Some((name, _)) = exact(config, query)? {
        return Ok(ChainRef::Configured(name.to_string()));
    }
    if let Some(runtime) = runtime
        && crate::names::is_valid_chain_name(query)
        && let Some(fork) = runtime
            .fork(query)
            .with_context(|| format!("unable to read fork '{query}'"))?
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
    /// An ID or the exact name: destructive commands (`rm`).
    Exact,
    /// An ID, the exact name, or an alias (any case, implicit ones too),
    /// never a prefix: `label rm --chain`.
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
    if policy != Policy::Prefix {
        if let Some(found) = query.parse().ok().and_then(|id| config.chain_by_id(id)) {
            return Ok(found);
        }
        let exact = match policy {
            Policy::Exact => config.chains.get_key_value(query).map(|(n, c)| found(n, c)),
            _ => config
                .chains
                .iter()
                .find(|(name, chain)| *name == query || chain.matches_alias(query))
                .map(|(n, c)| found(n, c))
                .or_else(|| implicit_alias(config, query)),
        };
        return exact.ok_or_else(|| {
            anyhow!("chain '{query}' not found; this command needs an exact chain name or ID")
        });
    }
    if let Some(found) = exact(config, query)? {
        return Ok(found);
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

/// [`Policy::Prefix`]'s exact matches only: an ID (an unknown ID is an
/// error), then a name or alias in any case, then an [`implicit_alias`].
/// The gateway resolves its URL paths this way, so they name the chains
/// the CLI's references do.
pub fn exact<'c>(config: &'c Config, query: &str) -> Result<Option<(&'c str, &'c Chain)>> {
    if let Ok(id) = query.parse::<u64>() {
        return match config.chain_by_id(id) {
            Some(found) => Ok(Some(found)),
            None => bail!("no chain with ID {id} is configured{}", hint(config)),
        };
    }
    Ok(config
        .chains
        .iter()
        .find(|(name, chain)| name.eq_ignore_ascii_case(query) || chain.matches_alias(query))
        .map(|(name, chain)| (name.as_str(), chain))
        .or_else(|| implicit_alias(config, query)))
}

/// The configured chain a well-known long name stands for
/// ([`names::LONG_NAMES`](crate::names::LONG_NAMES): `arbitrum`,
/// `ethereum`), as an implicit alias of the chain with that ID. Callers try
/// it only after every configured name and alias, so those always win.
pub fn implicit_alias<'c>(config: &'c Config, query: &str) -> Option<(&'c str, &'c Chain)> {
    crate::names::long_name_id(query).and_then(|id| config.chain_by_id(id))
}

/// What a command that needs a chain says when none is configured.
pub const NO_CHAINS: &str =
    "no chains configured yet; `chainz init` sets one up (or `chainz` shows the quick start)";

fn hint(config: &Config) -> String {
    if config.chains.is_empty() {
        format!(" ({NO_CHAINS})")
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
    fn long_names_are_implicit_aliases_after_configured_ones() {
        let c = crate::model::Config::parse(
            r#"
version = 2
[chains.arb]
id = 42161
rpc = "a"
rpcs.a = "https://a"
[chains.eth]
id = 1
rpc = "a"
rpcs.a = "https://a"
[chains.nova]
id = 42170
aliases = ["optimism"]
rpc = "a"
rpcs.a = "https://a"
"#,
        )
        .unwrap();
        let r = |q| resolve(&c, None, q).map(|r| r.name().to_string());
        assert_eq!(r("arbitrum").unwrap(), "arb");
        assert_eq!(r("Ethereum").unwrap(), "eth");
        assert_eq!(r("mainnet").unwrap(), "eth");
        // A configured alias wins (OP isn't configured, but even if it were).
        assert_eq!(r("optimism").unwrap(), "nova");
        // Only whole long names: no new prefixes.
        assert!(r("arbitr").is_err());
        // Not configured: nothing.
        assert!(r("avalanche").is_err());
        let alias = |q| configured(&c, q, Policy::ExactOrAlias).map(|(n, _)| n);
        assert_eq!(alias("arbitrum").unwrap(), "arb");
        assert!(configured(&c, "arbitrum", Policy::Exact).is_err());
        assert_eq!(exact(&c, "ARBITRUM").unwrap().unwrap().0, "arb");
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

    fn fork(rt: &Runtime, name: &str) -> crate::runtime::ForkClaim {
        let claim = rt.claim_fork(name).unwrap().unwrap();
        claim
            .register(&ForkRecord {
                format: ForkRecord::FORMAT,
                name: name.into(),
                pid: std::process::id(),
                anvil_pid: 0,
                port: 8601,
                parent: "base".into(),
                chain_id: 8453,
                fork_block: None,
                started_at: 0,
                ready: true,
            })
            .unwrap();
        claim
    }

    #[test]
    fn live_forks_resolve_by_name_only() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        let c = config();
        let claim = fork(&rt, "base-fork");
        assert!(matches!(
            resolve(&c, Some(&rt), "base-fork").unwrap(),
            ChainRef::Fork(_)
        ));
        assert_eq!(
            resolve(&c, Some(&rt), "8453").unwrap(),
            ChainRef::Configured("base".into())
        );
        // A fork's name is exact: never a prefix match of a configured chain.
        assert!(resolve(&c, None, "base-fork").is_err());
        // Its supervisor gone, the fork no longer resolves.
        drop(claim);
        assert!(resolve(&c, Some(&rt), "base-fork").is_err());
    }

    #[test]
    fn configured_chains_come_before_forks() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        let c = config();
        // A chain configured after the fork started keeps its name.
        let _base = fork(&rt, "base");
        let _alias = fork(&rt, "mainnet");
        assert_eq!(
            resolve(&c, Some(&rt), "base").unwrap(),
            ChainRef::Configured("base".into())
        );
        assert_eq!(
            resolve(&c, Some(&rt), "mainnet").unwrap(),
            ChainRef::Configured("eth".into())
        );
        // A live fork's exact name beats a prefix of configured names.
        let _ba = fork(&rt, "ba");
        assert!(matches!(
            resolve(&c, Some(&rt), "ba").unwrap(),
            ChainRef::Fork(f) if f.name == "ba"
        ));
    }
}
