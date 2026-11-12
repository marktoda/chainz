//! The address book: names for addresses, in both directions.
//!
//! [`LabelIndex`] resolves a name on a chain (`@usdc`, `@myrouter`,
//! `@funder`) and reverse-looks-up a `(chain_id, address)`. It covers
//! three sources:
//!
//! 1. user labels, `[addresses.NAME]` in config (per chain, or `"*"`);
//! 2. signers (every signer with a cached address is a label on every chain;
//!    one without fails to resolve, saying so);
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

/// The `[addresses.NAME]` scope key meaning "every chain".
pub const ALL_CHAINS: &str = "*";

/// How a label scope reads in a sentence: "on every chain" or "on eth".
pub fn scope_phrase(scope: &str) -> String {
    if scope == ALL_CHAINS {
        "on every chain".to_string()
    } else {
        format!("on {scope}")
    }
}

/// The token registry source, compiled into the binary.
pub(crate) const TOKENS_TOML: &str = include_str!("tokens.toml");

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
            toml::from_str(TOKENS_TOML).expect("tokens.toml is valid");
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

/// The built-in token at `address` on `chain_id`, if the registry has it.
pub fn builtin_token_at(chain_id: u64, address: &Address) -> Option<&'static BuiltinToken> {
    builtin_tokens()
        .iter()
        .find(|t| t.chain_id == chain_id && t.address == *address)
}

/// The registry's token names in display order: the wrapped native token
/// first (it sits next to the native balance), then the majors.
pub const TOKEN_ORDER: [&str; 6] = ["weth", "usdc", "usdce", "usdt", "dai", "wbtc"];

/// Decimals of every EVM chain's native token.
pub const NATIVE_DECIMALS: u8 = 18;

/// The native token's symbol on `chain_id`, for display.
pub fn native_symbol(chain_id: u64) -> &'static str {
    match chain_id {
        56 | 97 => "BNB",
        100 => "xDAI",
        137 | 80002 => "POL",
        196 => "OKB",
        250 => "FTM",
        5000 => "MNT",
        5042 => "USDC",
        42220 => "CELO",
        43114 | 43113 => "AVAX",
        _ => "ETH",
    }
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

impl std::fmt::Display for LabelSource {
    /// `user`, `signer` or `built-in`: the one vocabulary for listings,
    /// JSON and completion.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LabelSource::User => "user",
            LabelSource::Signer => "signer",
            LabelSource::Token => "built-in",
        })
    }
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
    /// The name is a signer without a cached address.
    #[error("{}", crate::signer::no_cached_address(name))]
    NoCachedAddress {
        /// The signer's name.
        name: String,
    },
}

#[derive(Debug, Default)]
struct Entry {
    per_chain: HashMap<u64, Address>,
    all_chains: Option<Address>,
}

/// Reverse maps: address to labels, per chain and for every chain.
#[derive(Debug, Default)]
struct Reverse {
    by_chain: HashMap<(u64, Address), Vec<Label>>,
    any_chain: HashMap<Address, Vec<Label>>,
}

/// Forward and reverse index over user labels, signers and built-in tokens.
/// The reverse maps are built on first use, so `exec`, which only resolves
/// names, never pays for them.
#[derive(Debug, Default)]
pub struct LabelIndex {
    user: HashMap<String, Entry>,
    /// Every signer, with its cached address if it has one.
    signers: HashMap<String, Option<Address>>,
    /// User labels, then signers, in config order.
    configured: Vec<Label>,
    reverse: OnceLock<Reverse>,
}

fn token_label(token: &BuiltinToken) -> Label {
    Label {
        name: token.name.clone(),
        address: token.address,
        source: LabelSource::Token,
        scope: LabelScope::Chain(token.chain_id),
    }
}

impl LabelIndex {
    /// Build the index from config (user labels, signers) plus the built-in
    /// registry. Invalid user entries, and entries naming an unknown chain,
    /// are skipped (validation reports them).
    pub fn new(config: &Config) -> Self {
        let mut index = LabelIndex::default();
        for (name, entries) in &config.addresses {
            let name = name.to_ascii_lowercase();
            let mut entry = Entry::default();
            for (scope, address) in entries {
                let Ok(address) = address.parse::<Address>() else {
                    continue;
                };
                let scope = if scope == ALL_CHAINS {
                    entry.all_chains = Some(address);
                    LabelScope::AllChains
                } else if let Some(chain) = config.chains.get(scope) {
                    entry.per_chain.insert(chain.id, address);
                    LabelScope::Chain(chain.id)
                } else {
                    continue;
                };
                index.configured.push(Label {
                    name: name.clone(),
                    address,
                    source: LabelSource::User,
                    scope,
                });
            }
            index.user.insert(name, entry);
        }
        for (name, signer) in &config.signers {
            let name = name.to_ascii_lowercase();
            index.signers.insert(name.clone(), signer.address);
            if let Some(address) = signer.address {
                index.configured.push(Label {
                    name,
                    address,
                    source: LabelSource::Signer,
                    scope: LabelScope::AllChains,
                });
            }
        }
        index
    }

    /// Every label from every source: user labels and signers in config
    /// order, then the built-in tokens. Shadowed built-in tokens are
    /// included; compare against [`Self::resolve`] to tell.
    pub fn labels(&self) -> impl Iterator<Item = Label> + '_ {
        self.configured
            .iter()
            .cloned()
            .chain(builtin_tokens().iter().map(token_label))
    }

    fn reverse_maps(&self) -> &Reverse {
        self.reverse.get_or_init(|| {
            let mut reverse = Reverse::default();
            for label in self.labels() {
                match label.scope {
                    LabelScope::Chain(id) => reverse
                        .by_chain
                        .entry((id, label.address))
                        .or_default()
                        .push(label),
                    LabelScope::AllChains => reverse
                        .any_chain
                        .entry(label.address)
                        .or_default()
                        .push(label),
                }
            }
            reverse
        })
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
        match self.signers.get(&name) {
            Some(Some(address)) => {
                return Ok(Label {
                    name,
                    address: *address,
                    source: LabelSource::Signer,
                    scope: LabelScope::AllChains,
                });
            }
            Some(None) => return Err(LabelError::NoCachedAddress { name }),
            None => {}
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

    /// The preferred label for `(chain_id, address)` (user, then signer,
    /// then token) among those that resolve back to `address` on
    /// `chain_id`: the name can be pasted back as `@name`, so a shadowed
    /// built-in token never labels.
    pub fn reverse(&self, chain_id: u64, address: &Address) -> Option<&Label> {
        self.reverse_all(chain_id, address).into_iter().find(|l| {
            self.resolve(&l.name, chain_id)
                .is_ok_and(|r| r.address == *address)
        })
    }

    /// Every label for `(chain_id, address)`, preferred first.
    fn reverse_all(&self, chain_id: u64, address: &Address) -> Vec<&Label> {
        let reverse = self.reverse_maps();
        let mut out: Vec<&Label> = reverse
            .by_chain
            .get(&(chain_id, *address))
            .into_iter()
            .flatten()
            .chain(reverse.any_chain.get(address).into_iter().flatten())
            .collect();
        out.sort_by(|a, b| (a.source, &a.name).cmp(&(b.source, &b.name)));
        out
    }

    /// The label that wins over `label` on its chain, if another one does:
    /// a user label or signer hiding a built-in token, or a per-chain
    /// entry hiding an every-chain one. `None` for an every-chain label
    /// (which chain would it be shadowed on?) and for the winner itself.
    pub fn shadowed(&self, label: &Label) -> Option<Label> {
        let LabelScope::Chain(chain_id) = label.scope else {
            return None;
        };
        self.resolve(&label.name, chain_id)
            .ok()
            .filter(|winner| (winner.source, winner.address) != (label.source, label.address))
    }

    /// The built-in tokens that label `name` (lowercase), holding
    /// `address` on `scope`, hides: on its chain the token of that name at
    /// another address (a per-chain label always wins there), and for an
    /// every-chain label each such token on a chain where the label wins.
    /// Works for a label not yet written when `scope` is one chain (what
    /// an import previews).
    pub fn shadows(
        &self,
        name: &str,
        scope: LabelScope,
        address: &Address,
    ) -> Vec<&'static BuiltinToken> {
        builtin_tokens()
            .iter()
            .filter(|t| t.name == name && t.address != *address)
            .filter(|t| match scope {
                LabelScope::Chain(chain_id) => t.chain_id == chain_id,
                LabelScope::AllChains => self
                    .resolve(name, t.chain_id)
                    .is_ok_and(|w| w.source != LabelSource::Token && w.address == *address),
            })
            .collect()
    }

    /// Every built-in token another label hides at a different address on
    /// the token's chain, with the label that wins.
    pub fn shadowed_tokens(&self) -> impl Iterator<Item = (&'static BuiltinToken, Label)> + '_ {
        builtin_tokens().iter().filter_map(|token| {
            self.shadowed(&token_label(token))
                .filter(|winner| winner.address != token.address)
                .map(|winner| (token, winner))
        })
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
        crate::model::Config::parse(
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
    fn a_signer_without_a_cached_address_says_so() {
        let mut config = config();
        config.signers.insert(
            "cold".into(),
            crate::model::Signer::new(crate::model::Backend::Keyring, None),
        );
        let index = LabelIndex::new(&config);
        let err = index.resolve("cold", 1).unwrap_err();
        assert_eq!(
            err,
            LabelError::NoCachedAddress {
                name: "cold".into()
            }
        );
        assert!(
            err.to_string().contains("`chainz signer add cold`"),
            "{err}"
        );
        assert!(index.labels().all(|l| l.name != "cold"));
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
    fn labels_lists_every_source_and_reverse_maps_are_lazy() {
        let index = LabelIndex::new(&config());
        assert!(index.reverse.get().is_none(), "built only on first use");
        let labels: Vec<Label> = index.labels().collect();
        let configured: Vec<(&str, LabelSource)> = labels
            .iter()
            .filter(|l| l.source != LabelSource::Token)
            .map(|l| (l.name.as_str(), l.source))
            .collect();
        assert_eq!(
            configured,
            [
                ("myrouter", LabelSource::User),
                ("my-safe", LabelSource::User),
                ("usdc", LabelSource::User),
                ("funder", LabelSource::Signer)
            ]
        );
        assert_eq!(labels.len() - configured.len(), builtin_tokens().len());
        index.resolve("usdc", 1).unwrap();
        assert!(index.reverse.get().is_none());
        let funder: Address = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF"
            .parse()
            .unwrap();
        assert!(index.reverse(1, &funder).is_some());
        assert!(index.reverse.get().is_some());
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
        // The built-in USDC that the user's `usdc` hides on chain 1 is not
        // labelled `usdc` there: `@usdc` names another address.
        let hidden = builtin_token(1, "usdc").unwrap().address;
        assert!(index.reverse(1, &hidden).is_none_or(|l| l.name != "usdc"));
    }

    #[test]
    fn a_label_shadows_the_token_of_its_name_where_it_wins() {
        let index = LabelIndex::new(&config());
        let eth_usdc = builtin_token(1, "usdc").unwrap();
        let mine = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
            .parse()
            .unwrap();
        // Per chain: the token of that name on that chain, whatever the
        // config holds (an import previews labels not yet written).
        assert_eq!(
            index.shadows("usdc", LabelScope::Chain(1), &mine),
            [eth_usdc]
        );
        assert!(
            index
                .shadows("usdc", LabelScope::Chain(1), &eth_usdc.address)
                .is_empty()
        );
        assert!(
            index
                .shadows("myrouter", LabelScope::Chain(1), &mine)
                .is_empty()
        );
        // Every chain: only where the label is the winner (`usdc` on eth
        // is the user's per-chain entry, at `mine`).
        let base_usdc = builtin_token(8453, "usdc").unwrap();
        let mut all = config();
        all.addresses
            .get_mut("usdc")
            .unwrap()
            .insert("*".into(), mine.to_string());
        let index = LabelIndex::new(&all);
        let shadowed = index.shadows("usdc", LabelScope::AllChains, &mine);
        assert!(
            shadowed.contains(&eth_usdc) && shadowed.contains(&base_usdc),
            "{shadowed:?}"
        );
    }

    #[test]
    fn shadowing_sources_and_registry_lookups() {
        let config = config();
        let index = LabelIndex::new(&config);
        let eth_usdc = builtin_token(1, "usdc").unwrap();
        let token = token_label(eth_usdc);
        let winner = index.shadowed(&token).unwrap();
        assert_eq!(winner.source, LabelSource::User);
        let base_usdc = token_label(builtin_token(8453, "usdc").unwrap());
        assert_eq!(index.shadowed(&base_usdc), None, "nothing hides base usdc");
        let user = index.resolve("usdc", 1).unwrap();
        assert_eq!(index.shadowed(&user), None, "the winner is not shadowed");
        let hidden: Vec<_> = index
            .shadowed_tokens()
            .map(|(t, winner)| (t.chain_id, t.name.as_str(), winner.source))
            .collect();
        assert_eq!(hidden, [(1, "usdc", LabelSource::User)]);
        assert_eq!(LabelSource::Token.to_string(), "built-in");
        assert_eq!(builtin_token_at(1, &eth_usdc.address), Some(eth_usdc));
        assert_eq!(builtin_token_at(8453, &eth_usdc.address), None);
        assert_eq!(native_symbol(56), "BNB");
        assert_eq!(native_symbol(8453), "ETH");
        assert!(
            builtin_tokens()
                .iter()
                .all(|t| TOKEN_ORDER.contains(&t.name.as_str()))
        );
        assert_eq!(config.chain_display(8453), "base");
        assert_eq!(config.chain_display(10), "op");
        assert_eq!(config.chain_display(424242), "chain 424242");
    }
}
