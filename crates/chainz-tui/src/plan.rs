//! The dashboard's owned, thread-safe view of the config: what to poll,
//! with every `${VAR}` already expanded into a [`Target`].
//!
//! Built on the UI thread from a [`Chainz`] (which is not `Sync`) and handed
//! to the workers. Display strings are redacted here, once, so no renderer
//! can leak a raw URL.

use chainz_core::Address;
use chainz_core::Chainz;
use chainz_core::labels::builtin_tokens;
use chainz_core::redact::redact;
use chainz_core::secrets::{Builtins, VarResolver};
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
    /// Symbol of the native currency, when the registry implies one.
    pub native_symbol: Option<String>,
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
        let loader = || chainz.secrets();
        let chains = config
            .chains
            .iter()
            .map(|(name, chain)| {
                let vars = VarResolver::new(
                    Builtins {
                        chain_id: Some(chain.id),
                        chain: Some(name),
                    },
                    &loader,
                );
                let endpoints = chain
                    .rpcs
                    .iter()
                    .map(|(ep_name, endpoint)| {
                        let target = (|| {
                            let url = vars.expand(&endpoint.url).map_err(|e| e.to_string())?;
                            let mut headers = Vec::with_capacity(endpoint.headers.len());
                            for (header, value) in &endpoint.headers {
                                let value = vars.expand(value).map_err(|e| e.to_string())?;
                                headers.push((header.clone(), value.into_owned()));
                            }
                            Target::new(chain.id, ep_name.as_str(), &url, &headers)
                                .map_err(|e| e.to_string())
                        })();
                        EndpointPlan {
                            name: ep_name.clone(),
                            url: if reveal {
                                endpoint.url.clone()
                            } else {
                                redact(&endpoint.url)
                            },
                            websocket: endpoint.is_websocket(),
                            target,
                        }
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
                    native_symbol: native_symbol(&tokens),
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

    /// Token label names across every chain: the Balances tab's columns.
    /// The wrapped native token sits next to the native balance, then the
    /// majors in a fixed order, then anything else by name.
    pub fn token_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for token in self.chains.iter().flat_map(|c| &c.tokens) {
            if !names.contains(&token.name) {
                names.push(token.name.clone());
            }
        }
        const ORDER: [&str; 5] = ["weth", "usdc", "usdt", "dai", "wbtc"];
        let rank = |name: &str| ORDER.iter().position(|n| *n == name).unwrap_or(ORDER.len());
        names.sort_by_key(|name| (rank(name), name.clone()));
        names
    }
}

/// The registry's `weth` entry is each chain's wrapped native token (WETH,
/// WBNB, WPOL), so the native symbol is its symbol without the `W`. Where
/// the native token is itself the ERC-20 (CELO), the symbol has no `W`.
fn native_symbol(tokens: &[TokenPlan]) -> Option<String> {
    let wrapped = tokens.iter().find(|t| t.name == "weth")?;
    let native = wrapped.symbol.strip_prefix('W').unwrap_or(&wrapped.symbol);
    (!native.is_empty()).then(|| native.to_string())
}
