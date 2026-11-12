//! The dashboard's owned, thread-safe view of the config: what to poll,
//! with every `${VAR}` already expanded into a [`Target`].
//!
//! Built on the UI thread from a [`Chainz`] (which is not `Sync`) and handed
//! to the workers. Display strings are redacted here, once, so no renderer
//! can leak a raw URL.

use chainz_core::Address;
use chainz_core::Chainz;
use chainz_core::labels::{TOKEN_ORDER, builtin_tokens, native_symbol};
use chainz_core::model::Endpoint;
use chainz_core::redact;
use chainz_core::route::Route;
use chainz_rpc::Target;

/// Everything the workers and the renderer need to know about the config.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Chains in config order.
    pub chains: Vec<ChainPlan>,
    /// Signers in config order.
    pub signers: Vec<SignerPlan>,
}

/// One configured chain.
#[derive(Debug, Clone)]
pub struct ChainPlan {
    /// Short name.
    pub name: String,
    /// Chain ID.
    pub id: u64,
    /// Aliases (matched by the filter).
    pub aliases: Vec<String>,
    /// Name of the active endpoint.
    pub active: String,
    /// Endpoints in config order.
    pub endpoints: Vec<EndpointPlan>,
    /// Built-in tokens on this chain, by label name.
    pub tokens: Vec<TokenPlan>,
    /// Symbol of the native currency.
    pub native_symbol: &'static str,
}

impl ChainPlan {
    /// The active endpoint.
    pub fn active_endpoint(&self) -> Option<&EndpointPlan> {
        self.endpoints.iter().find(|e| e.name == self.active)
    }
}

/// One endpoint of a chain.
#[derive(Debug, Clone)]
pub struct EndpointPlan {
    /// Endpoint name.
    pub name: String,
    /// The URL as it may be shown and copied: redacted unless `--reveal`,
    /// in which case it is the raw config value (templates unexpanded).
    pub url: String,
    /// `ws://` or `wss://`: listed, never probed.
    pub websocket: bool,
    /// The resolved target, or why it could not be built (never a URL).
    pub target: Result<Target, String>,
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
            .and_then(|route| {
                Target::new(route.chain_id, &route.endpoint, &route.url, &route.headers)
                    .map_err(|e| e.to_string())
            });
        EndpointPlan {
            name: name.to_string(),
            url: redact::url(&endpoint.url, reveal),
            websocket: endpoint.is_websocket(),
            target,
        }
    }
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
                ChainPlan {
                    name: name.clone(),
                    id: chain.id,
                    aliases: chain.aliases.clone(),
                    active: chain.rpc.clone(),
                    endpoints,
                    native_symbol: native_symbol(chain.id),
                    tokens,
                }
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

    /// The chain named `name`.
    pub fn chain(&self, name: &str) -> Option<&ChainPlan> {
        self.chains.iter().find(|c| c.name == name)
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
        for token in self.chains.iter().flat_map(|c| &c.tokens) {
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
