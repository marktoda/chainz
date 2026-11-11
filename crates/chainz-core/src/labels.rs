//! The address book: names for addresses, in both directions.
//!
//! **Frozen interface** (Phase 0): [`LabelIndex`] resolves a name on a
//! chain (`@usdc`, `@myrouter`, `@funder`) and reverse-looks-up a
//! `(chain_id, address)`. It covers three sources:
//!
//! 1. user labels, `[addresses.NAME]` in config (per chain, or `"*"`);
//! 2. signers (every signer with a cached address is a label on every chain);
//! 3. the compiled-in token registry ([`builtin_tokens`]).
//!
//! Lookup of `name` on chain C: user entry for C, then the user `"*"`
//! entry, then a signer of that name, then the built-in token for C. User
//! labels may shadow built-in tokens; context tokens and signer names are
//! reserved (enforced by validation).

use crate::address::Address;
use crate::model::Config;
use crate::names;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

/// The token registry source, compiled into the binary.
pub const TOKENS_TOML: &str = include_str!("tokens.toml");

/// One built-in token entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinToken {
    /// Chain ID.
    pub chain_id: u64,
    /// Label name (`weth`, `usdc`).
    pub name: String,
    /// Contract address.
    pub address: Address,
    /// `symbol()` as the contract returns it.
    pub symbol: String,
    /// `decimals()`.
    pub decimals: u8,
    /// Issuer or chain documentation listing the address.
    pub source: String,
}

#[derive(Deserialize)]
struct TokenWire {
    address: Address,
    symbol: String,
    decimals: u8,
    source: String,
}

/// Every built-in token, parsed once on first use.
pub fn builtin_tokens() -> &'static [BuiltinToken] {
    static TOKENS: OnceLock<Vec<BuiltinToken>> = OnceLock::new();
    TOKENS.get_or_init(|| {
        let parsed: HashMap<String, HashMap<String, TokenWire>> =
            toml_edit::de::from_str(TOKENS_TOML).expect("tokens.toml is valid");
        let mut out: Vec<BuiltinToken> = parsed
            .into_iter()
            .flat_map(|(chain, tokens)| {
                let chain_id: u64 = chain.parse().expect("tokens.toml keys are chain IDs");
                tokens.into_iter().map(move |(name, t)| BuiltinToken {
                    chain_id,
                    name,
                    address: t.address,
                    symbol: t.symbol,
                    decimals: t.decimals,
                    source: t.source,
                })
            })
            .collect();
        out.sort_by(|a, b| (a.chain_id, &a.name).cmp(&(b.chain_id, &b.name)));
        out
    })
}

/// A built-in token by chain and name.
pub fn builtin_token(chain_id: u64, name: &str) -> Option<&'static BuiltinToken> {
    builtin_tokens()
        .iter()
        .find(|t| t.chain_id == chain_id && t.name.eq_ignore_ascii_case(name))
}

/// Where a label came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LabelSource {
    /// `[addresses.NAME]` in config.
    User,
    /// A signer's cached address.
    Signer,
    /// The compiled-in token registry.
    Token,
}

/// Which chains a label applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LabelScope {
    /// One chain ID.
    Chain(u64),
    /// Every chain (user `"*"` entries and signers).
    AllChains,
}

/// A resolved label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    /// Label name, lowercase.
    pub name: String,
    /// The address.
    pub address: Address,
    /// Where it came from.
    pub source: LabelSource,
    /// Which chains it covers.
    pub scope: LabelScope,
}

/// Why a name did not resolve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    /// No label has this name anywhere.
    #[error("unknown label @{name}{}", suggestion.as_ref().map(|s| format!(" (did you mean @{s}?)")).unwrap_or_default())]
    Unknown {
        /// The name looked up.
        name: String,
        /// The closest known name.
        suggestion: Option<String>,
    },
    /// The label exists but has no address on this chain.
    #[error("@{name} has no address on chain {chain_id}")]
    NotOnChain {
        /// The name looked up.
        name: String,
        /// The chain asked about.
        chain_id: u64,
    },
}

#[derive(Debug, Default)]
struct Entry {
    per_chain: HashMap<u64, Address>,
    all_chains: Option<Address>,
}

/// Forward and reverse index over user labels, signers and built-in tokens.
#[derive(Debug, Default)]
pub struct LabelIndex {
    user: HashMap<String, Entry>,
    signers: HashMap<String, Address>,
    reverse: HashMap<(u64, Address), Vec<Label>>,
    reverse_any: HashMap<Address, Vec<Label>>,
}

impl LabelIndex {
    /// Build the index from config (user labels, signers) plus the built-in
    /// registry. Invalid user entries are skipped (validation reports them).
    pub fn new(config: &Config) -> Self {
        let mut index = LabelIndex::default();
        for (name, entries) in &config.addresses {
            let name = name.to_ascii_lowercase();
            let mut entry = Entry::default();
            for (scope, address) in entries {
                let Ok(address) = address.parse::<Address>() else {
                    continue;
                };
                if scope == "*" {
                    entry.all_chains = Some(address);
                    index.add_reverse_any(Label {
                        name: name.clone(),
                        address,
                        source: LabelSource::User,
                        scope: LabelScope::AllChains,
                    });
                } else if let Some(chain) = config.chains.get(scope) {
                    entry.per_chain.insert(chain.id, address);
                    index.add_reverse(
                        chain.id,
                        Label {
                            name: name.clone(),
                            address,
                            source: LabelSource::User,
                            scope: LabelScope::Chain(chain.id),
                        },
                    );
                }
            }
            index.user.insert(name, entry);
        }
        for (name, signer) in &config.signers {
            if let Some(address) = signer.address {
                index.signers.insert(name.to_ascii_lowercase(), address);
                index.add_reverse_any(Label {
                    name: name.to_ascii_lowercase(),
                    address,
                    source: LabelSource::Signer,
                    scope: LabelScope::AllChains,
                });
            }
        }
        for token in builtin_tokens() {
            index.add_reverse(
                token.chain_id,
                Label {
                    name: token.name.clone(),
                    address: token.address,
                    source: LabelSource::Token,
                    scope: LabelScope::Chain(token.chain_id),
                },
            );
        }
        for labels in index
            .reverse
            .values_mut()
            .chain(index.reverse_any.values_mut())
        {
            labels.sort_by(|a, b| (a.source, &a.name).cmp(&(b.source, &b.name)));
        }
        index
    }

    fn add_reverse(&mut self, chain_id: u64, label: Label) {
        self.reverse
            .entry((chain_id, label.address))
            .or_default()
            .push(label);
    }

    fn add_reverse_any(&mut self, label: Label) {
        self.reverse_any
            .entry(label.address)
            .or_default()
            .push(label);
    }

    /// Resolve `name` (case-insensitive) on `chain_id`.
    pub fn resolve(&self, name: &str, chain_id: u64) -> Result<Label, LabelError> {
        let name = name.to_ascii_lowercase();
        if let Some(entry) = self.user.get(&name) {
            if let Some(address) = entry.per_chain.get(&chain_id) {
                return Ok(Label {
                    name,
                    address: *address,
                    source: LabelSource::User,
                    scope: LabelScope::Chain(chain_id),
                });
            }
            if let Some(address) = entry.all_chains {
                return Ok(Label {
                    name,
                    address,
                    source: LabelSource::User,
                    scope: LabelScope::AllChains,
                });
            }
        }
        if let Some(address) = self.signers.get(&name) {
            return Ok(Label {
                name,
                address: *address,
                source: LabelSource::Signer,
                scope: LabelScope::AllChains,
            });
        }
        if let Some(token) = builtin_token(chain_id, &name) {
            return Ok(Label {
                name,
                address: token.address,
                source: LabelSource::Token,
                scope: LabelScope::Chain(chain_id),
            });
        }
        if self.contains(&name) {
            return Err(LabelError::NotOnChain { name, chain_id });
        }
        let suggestion = names::suggest(&name, self.names()).map(str::to_string);
        Err(LabelError::Unknown { name, suggestion })
    }

    /// Whether any source defines `name` on any chain.
    pub fn contains(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.user.contains_key(&name)
            || self.signers.contains_key(&name)
            || builtin_tokens().iter().any(|t| t.name == name)
    }

    /// The preferred label for `(chain_id, address)`: user, then signer,
    /// then token.
    pub fn reverse(&self, chain_id: u64, address: &Address) -> Option<&Label> {
        self.reverse_all(chain_id, address).into_iter().next()
    }

    /// Every label for `(chain_id, address)`, preferred first.
    pub fn reverse_all(&self, chain_id: u64, address: &Address) -> Vec<&Label> {
        let mut out: Vec<&Label> = self
            .reverse
            .get(&(chain_id, *address))
            .into_iter()
            .flatten()
            .chain(self.reverse_any.get(address).into_iter().flatten())
            .collect();
        out.sort_by(|a, b| (a.source, &a.name).cmp(&(b.source, &b.name)));
        out
    }

    /// Every known label name (deduplicated, sorted).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        let mut all: Vec<&str> = self
            .user
            .keys()
            .chain(self.signers.keys())
            .map(String::as_str)
            .chain(builtin_tokens().iter().map(|t| t.name.as_str()))
            .collect();
        all.sort_unstable();
        all.dedup();
        all.into_iter()
    }

    /// Every label that resolves on `chain_id` (one per name, after
    /// shadowing).
    pub fn labels_on(&self, chain_id: u64) -> Vec<Label> {
        self.names()
            .filter_map(|n| self.resolve(n, chain_id).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml_edit::de::from_str(
            r#"
version = 2
[chains.base]
id = 8453
rpc = "a"
rpcs.a = "https://a"
[chains.eth]
id = 1
rpc = "a"
rpcs.a = "https://a"
[signers.funder]
backend = "keyring"
address = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF"
[addresses.myrouter]
base = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
[addresses.my-safe]
"*" = "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69"
[addresses.usdc]
eth = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
"#,
        )
        .unwrap()
    }

    #[test]
    fn registry_parses_and_has_the_minimum() {
        let tokens = builtin_tokens();
        assert!(tokens.len() >= 28);
        let usdc = builtin_token(8453, "USDC").unwrap();
        assert_eq!(
            usdc.address.to_string(),
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
        );
        assert_eq!(usdc.decimals, 6);
        assert!(tokens.iter().all(|t| t.source.starts_with("https://")));
        for id in [
            1, 8453, 130, 56, 137, 42220, 10, 196, 480, 42161, 7777777, 1868, 4326, 43114, 4663,
            324,
        ] {
            assert!(builtin_token(id, "weth").is_some(), "weth on {id}");
        }
    }

    #[test]
    fn resolution_order_and_shadowing() {
        let index = LabelIndex::new(&config());
        assert_eq!(
            index.resolve("MyRouter", 8453).unwrap().source,
            LabelSource::User
        );
        assert_eq!(
            index.resolve("myrouter", 1),
            Err(LabelError::NotOnChain {
                name: "myrouter".into(),
                chain_id: 1
            })
        );
        assert_eq!(
            index.resolve("my-safe", 1).unwrap().scope,
            LabelScope::AllChains
        );
        assert_eq!(
            index.resolve("funder", 10).unwrap().source,
            LabelSource::Signer
        );
        // User label shadows the built-in on eth only.
        assert_eq!(index.resolve("usdc", 1).unwrap().source, LabelSource::User);
        assert_eq!(
            index.resolve("usdc", 8453).unwrap().source,
            LabelSource::Token
        );
        match index.resolve("udsc", 8453) {
            Err(LabelError::Unknown { suggestion, .. }) => {
                assert_eq!(suggestion.as_deref(), Some("usdc"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reverse_lookup_prefers_user_labels() {
        let index = LabelIndex::new(&config());
        let usdc: Address = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
            .parse()
            .unwrap();
        assert_eq!(index.reverse(8453, &usdc).unwrap().name, "usdc");
        assert!(index.reverse(1, &usdc).is_none());
        let funder: Address = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF"
            .parse()
            .unwrap();
        assert_eq!(index.reverse(999, &funder).unwrap().name, "funder");
        let one: Address = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
            .parse()
            .unwrap();
        assert_eq!(index.reverse(8453, &one).unwrap().name, "myrouter");
        assert_eq!(index.reverse(1, &one).unwrap().name, "usdc");
    }
}
