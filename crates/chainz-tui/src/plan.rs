//! The dashboard's owned, thread-safe view of the config: what to poll,
//! with every `${VAR}` already expanded into a [`Target`].
//!
//! Built on the UI thread from a [`Chainz`] (which is not `Sync`) and handed
//! to the workers. Display strings are redacted here, once, and the state
//! and the renderers get only the `*Info` parts, so no renderer can leak a
//! raw URL.

use chainz_core::Address;
use chainz_core::Chainz;
use chainz_core::labels::{TOKEN_ORDER, builtin_tokens, native_symbol};
use chainz_core::model::Endpoint;
use chainz_core::redact;
use chainz_core::route::Route;
use chainz_rpc::Target;
use std::sync::atomic::{AtomicU64, Ordering};

/// Everything the workers and the renderer need to know about the config.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Chains in config order.
    pub chains: Vec<ChainPlan>,
    /// Signers in config order.
    pub signers: Vec<SignerPlan>,
}

/// One configured chain: what is shown about it, and how to reach it.
#[derive(Debug, Clone)]
pub struct ChainPlan {
    /// What the dashboard shows about it.
    pub info: ChainInfo,
    /// Endpoints in config order.
    pub endpoints: Vec<EndpointPlan>,
    /// Where it reads from, as an identity: two plans of a chain with the
    /// same source read it the same way (same name, ID and active
    /// endpoint, URL and headers as resolved), so a poller, a balance
    /// read or a head for one is good for the other. Workers stamp their
    /// updates with it; the state drops updates from another source.
    pub source: Source,
}

/// A chain's source identity; see [`ChainPlan::source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Source(u64);

impl Source {
    /// A fixture's source, the same for every chain of this name.
    #[cfg(test)]
    pub(crate) fn named(name: impl AsRef<str>) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.as_ref().hash(&mut hasher);
        Source(hasher.finish() | 1 << 63)
    }

    /// A source no other plan has.
    pub fn fresh() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Source(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// What the dashboard shows about a chain: display data only, never a
/// target or an expanded URL.
#[derive(Debug, Clone)]
pub struct ChainInfo {
    /// Short name.
    pub name: String,
    /// Chain ID.
    pub id: u64,
    /// Aliases (matched by the filter).
    pub aliases: Vec<String>,
    /// Name of the active endpoint.
    pub active: String,
    /// Built-in tokens on this chain, by label name.
    pub tokens: Vec<TokenPlan>,
    /// Symbol of the native currency.
    pub native_symbol: &'static str,
}

impl ChainPlan {
    /// The plan of a chain, with a fresh [`Source`].
    pub fn new(info: ChainInfo, endpoints: Vec<EndpointPlan>) -> Self {
        ChainPlan {
            info,
            endpoints,
            source: Source::fresh(),
        }
    }

    /// The active endpoint.
    pub fn active_endpoint(&self) -> Option<&EndpointPlan> {
        self.endpoints
            .iter()
            .find(|e| e.info.name == self.info.active)
    }

    /// Whether `other` reads this chain the same way (see
    /// [`ChainPlan::source`]).
    fn same_source(&self, other: &ChainPlan) -> bool {
        let same_endpoint = match (self.active_endpoint(), other.active_endpoint()) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.info.websocket == b.info.websocket
                    && a.info.invalid == b.info.invalid
                    && match (&a.target, &b.target) {
                        (Some(a), Some(b)) => a.url() == b.url() && a.headers() == b.headers(),
                        (None, None) => true,
                        _ => false,
                    }
            }
            _ => false,
        };
        self.info.name == other.info.name && self.info.id == other.info.id && same_endpoint
    }
}

/// One endpoint of a chain.
#[derive(Debug, Clone)]
pub struct EndpointPlan {
    /// What the dashboard shows about it.
    pub info: EndpointInfo,
    /// The resolved target; `None` when `info.invalid` says why not.
    pub target: Option<Target>,
}

/// What the dashboard shows about an endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointInfo {
    /// Endpoint name.
    pub name: String,
    /// The URL as it may be shown and copied: redacted unless `--reveal`,
    /// in which case it is the raw config value (templates unexpanded).
    pub url: String,
    /// `ws://` or `wss://`: listed, never probed.
    pub websocket: bool,
    /// Why it cannot be used, when its config does not resolve (never a
    /// URL).
    pub invalid: Option<String>,
}

impl EndpointPlan {
    /// The plan for endpoint `name`, configured as `endpoint`, read through
    /// `route` (or why it cannot be).
    pub fn new(
        name: &str,
        endpoint: &Endpoint,
        route: anyhow::Result<Route>,
        reveal: bool,
    ) -> Self {
        // The bare cause ("variable ${X} is not set"): the row already
        // names the chain and the endpoint.
        let target = route
            .map_err(|e| e.root_cause().to_string())
            .and_then(|route| target(&route));
        EndpointPlan {
            info: EndpointInfo {
                name: name.to_string(),
                url: if reveal {
                    endpoint.url.clone()
                } else {
                    redact::url(&endpoint.url)
                },
                websocket: endpoint.is_websocket(),
                invalid: target.as_ref().err().cloned(),
            },
            target: target.ok(),
        }
    }
}

/// The `chainz-rpc` target for a core route, or why there is none.
pub fn target(route: &Route) -> Result<Target, String> {
    Target::new(route.chain_id, &route.endpoint, &route.url, &route.headers)
        .map_err(|e| e.to_string())
}

/// One signer; only its cached address is ever used.
#[derive(Debug, Clone)]
pub struct SignerPlan {
    /// Signer name.
    pub name: String,
    /// Cached address, when the config has one.
    pub address: Option<Address>,
}

/// One built-in token on a chain.
#[derive(Debug, Clone)]
pub struct TokenPlan {
    /// Label name (`weth`, `usdc`).
    pub name: String,
    /// On-chain symbol.
    pub symbol: String,
    /// Decimals.
    pub decimals: u8,
    /// Contract address.
    pub address: Address,
}

impl Plan {
    /// Resolve the whole config. Never fails: an endpoint whose variables
    /// do not resolve carries the error and shows as invalid.
    pub fn build(chainz: &Chainz, reveal: bool) -> Self {
        let config = chainz.config();
        let chains = config
            .chains
            .iter()
            .map(|(name, chain)| {
                let endpoints = chain
                    .rpcs
                    .iter()
                    .map(|(ep_name, endpoint)| {
                        let route = chainz.direct_route(name, chain.id, ep_name, endpoint);
                        EndpointPlan::new(ep_name, endpoint, route, reveal)
                    })
                    .collect();
                let tokens: Vec<TokenPlan> = builtin_tokens()
                    .iter()
                    .filter(|t| t.chain_id == chain.id)
                    .map(|t| TokenPlan {
                        name: t.name.clone(),
                        symbol: t.symbol.clone(),
                        decimals: t.decimals,
                        address: t.address,
                    })
                    .collect();
                let info = ChainInfo {
                    name: name.clone(),
                    id: chain.id,
                    aliases: chain.aliases.clone(),
                    active: chain.rpc.clone(),
                    native_symbol: native_symbol(chain.id),
                    tokens,
                };
                ChainPlan::new(info, endpoints)
            })
            .collect();
        let signers = config
            .signers
            .iter()
            .map(|(name, signer)| SignerPlan {
                name: name.clone(),
                address: signer.address,
            })
            .collect();
        Plan { chains, signers }
    }

    /// Take over `previous`'s source for every chain that reads the same
    /// way in both, so its pollers, balances and state carry over.
    pub fn inherit(&mut self, previous: &Plan) {
        for chain in &mut self.chains {
            if let Some(old) = previous.chain(&chain.info.name)
                && old.same_source(chain)
            {
                chain.source = old.source;
            }
        }
    }

    /// The chain named `name`.
    pub fn chain(&self, name: &str) -> Option<&ChainPlan> {
        self.chains.iter().find(|c| c.info.name == name)
    }

    /// Signers that have a cached address.
    pub fn holders(&self) -> Vec<(String, Address)> {
        self.signers
            .iter()
            .filter_map(|s| Some((s.name.clone(), s.address?)))
            .collect()
    }

    /// Token label names across every chain: the Balances tab's columns,
    /// in the registry's display order ([`TOKEN_ORDER`]), then anything
    /// else by name.
    pub fn token_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for token in self.chains.iter().flat_map(|c| &c.info.tokens) {
            if !names.contains(&token.name) {
                names.push(token.name.clone());
            }
        }
        let rank = |name: &str| {
            TOKEN_ORDER
                .iter()
                .position(|n| *n == name)
                .unwrap_or(TOKEN_ORDER.len())
        };
        names.sort_by_key(|name| (rank(name), name.clone()));
        names
    }
}
