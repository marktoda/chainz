//! Fork policy: which names a fork may take, and which chains anvil can
//! fork. The registry of live forks is [`crate::runtime`]; claiming a name
//! there is what rules out two live forks sharing it.

use crate::model::Config;
use crate::names;
use anyhow::{Result, bail};

/// Names a fork may not take: the `chainz fork` subcommands and their
/// aliases (a bin test keeps the two lists equal).
pub const RESERVED_FORK_NAMES: [&str; 3] = ["ls", "list", "stop"];

/// Chains anvil can't fork: `(chain ID, display name)`.
pub const UNFORKABLE: &[(u64, &str)] = &[(324, "zkSync Era"), (300, "zkSync Sepolia")];

/// The name a fork of `parent` gets without `--name`.
pub fn default_fork_name(parent: &str) -> String {
    format!("{parent}-fork")
}

/// Reject a chain anvil can't fork, before anything starts.
pub fn check_forkable(chain_name: &str, chain_id: u64) -> Result<()> {
    match UNFORKABLE.iter().find(|(id, _)| *id == chain_id) {
        Some((_, label)) => bail!(
            "'{chain_name}' can't be forked: anvil does not support {label} (chain {chain_id}), which is not EVM-equivalent"
        ),
        None => Ok(()),
    }
}

/// A fork name must be a valid chain name (so never a chain ID, and safe
/// as a file name in the registry), not a `fork` subcommand, and must not
/// shadow a configured chain's name or alias. Whether a live fork already
/// has it is the registry's answer ([`crate::runtime::Runtime::claim_fork`]).
pub fn check_fork_name(config: &Config, name: &str) -> Result<()> {
    if RESERVED_FORK_NAMES.contains(&name) {
        bail!("a fork can't be named '{name}': it is a `chainz fork` subcommand");
    }
    if !names::is_valid_chain_name(name) {
        bail!(
            "invalid fork name '{name}': use lowercase letters, digits, '-' and '_', not digits only"
        );
    }
    for (chain_name, chain) in &config.chains {
        if chain_name.eq_ignore_ascii_case(name) {
            bail!("fork name '{name}' is taken by a configured chain; pick another with --name");
        }
        if chain.matches_alias(name) {
            bail!(
                "fork name '{name}' is an alias of configured chain '{chain_name}'; pick another with --name"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::parse(
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
"#,
        )
        .unwrap()
    }

    #[test]
    fn name_rules() {
        let c = config();
        let check = |name: &str| check_fork_name(&c, name).map_err(|e| e.to_string());
        assert_eq!(check("eth-fork"), Ok(()));
        assert_eq!(check("scratch_2"), Ok(()));
        assert_eq!(check(&default_fork_name("base")), Ok(()));
        // Reserved subcommand names.
        for reserved in RESERVED_FORK_NAMES {
            assert!(check(reserved).unwrap_err().contains("subcommand"));
        }
        // Configured chain names and aliases, case-insensitively.
        assert!(check("base").unwrap_err().contains("configured chain"));
        assert!(check("mainnet").unwrap_err().contains("alias of"));
        // Shape: a chain ID must never resolve to a fork, and the name is
        // a file name in the registry.
        for bad in ["8453", "", "Base", "a b", "a/b", "../x", "a.b", "-x", "@x"] {
            assert!(check(bad).unwrap_err().contains("invalid"), "{bad:?}");
        }
    }

    #[test]
    fn unforkable_chains_are_rejected_up_front() {
        let err = check_forkable("zksync", 324).unwrap_err().to_string();
        assert!(err.contains("zkSync Era") && err.contains("324"), "{err}");
        assert!(check_forkable("base", 8453).is_ok());
    }
}
