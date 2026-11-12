//! The config→RPC bridge: which URL and headers reach a chain, and through
//! what. `exec`'s context, `fork` and the probing commands all route here,
//! so they agree on when the gateway is used.
//!
//! The rule: a chain's requests go through a running gateway (pinned to the
//! endpoint when one is named) unless the caller asks for a direct
//! connection or the endpoint is a websocket, which the gateway does not
//! serve. Otherwise they go to the endpoint itself, `${VAR}`s expanded. A
//! one-off URL (`--rpc URL` the chain has no endpoint for) always goes
//! straight to it.
//!
//! `chainz-rpc` does not depend on this crate: a consumer turns a [`Route`]
//! into its `Target` with one line,
//! `Target::new(route.chain_id, &route.endpoint, &route.url, &route.headers)`.

use crate::model::{Chain, Endpoint, RpcChoice};
use crate::redact;
use crate::runtime::{GatewayState, Runtime, ServeInfo};
use crate::store::Chainz;
use anyhow::{Context as _, Result, anyhow};
use std::fmt;

/// How to route.
#[derive(Debug, Clone, Default)]
pub struct RouteOpts {
    /// `--rpc NAME|URL` ([`Chain::rpc_choice`]): use this endpoint instead
    /// of the active one (and pin the gateway to it), or a URL the chain
    /// does not have, straight, for this route only (nothing is written).
    pub endpoint: Option<String>,
    /// Bypass a running gateway.
    pub direct: bool,
}

/// Where a chain's requests go.
#[derive(Clone)]
pub struct Route {
    /// The chain's ID.
    pub chain_id: u64,
    /// The chain's short name.
    pub chain: String,
    /// The endpoint the requests are for.
    pub endpoint: String,
    /// The URL to send to (secrets expanded, or the gateway's token).
    pub url: String,
    /// Headers to send, expanded (none through the gateway: it adds them).
    pub headers: Vec<(String, String)>,
    /// A safe-to-print form of the URL.
    pub display: String,
    /// Whether the URL is the local gateway.
    pub via_gateway: bool,
    /// Whether the endpoint is a websocket.
    pub websocket: bool,
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route")
            .field("chain_id", &self.chain_id)
            .field("chain", &self.chain)
            .field("endpoint", &self.endpoint)
            .field("url", &self.display)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("via_gateway", &self.via_gateway)
            .field("websocket", &self.websocket)
            .finish()
    }
}

/// The rule: whether requests to an endpoint go through a running gateway.
/// Not when the caller asks for a direct connection, nor for a websocket
/// endpoint, which the gateway does not serve.
pub fn uses_gateway(websocket: bool, direct: bool) -> bool {
    !direct && !websocket
}

impl Route {
    /// Through the gateway `info` describes, to chain `chain` (`chain_id`),
    /// for its endpoint `endpoint`; the URL pins the gateway to it when
    /// `pin` is set.
    pub fn via_gateway(
        info: &ServeInfo,
        chain: &str,
        chain_id: u64,
        endpoint: &str,
        pin: bool,
    ) -> Route {
        let url = info.addr().url(chain, pin.then_some(endpoint));
        Route {
            chain_id,
            chain: chain.to_string(),
            endpoint: endpoint.to_string(),
            display: redact::url(&url, false),
            url,
            headers: Vec::new(),
            via_gateway: true,
            websocket: false,
        }
    }
}

impl Chain {
    /// The chain's endpoints in preference order: the active one, then the
    /// failover list, then the rest in config order, each once (names that
    /// are not defined are skipped). `http_only` keeps only HTTP endpoints.
    pub fn endpoints_by_preference(&self, http_only: bool) -> Vec<(&str, &Endpoint)> {
        let preferred = std::iter::once(&self.rpc).chain(&self.failover);
        let mut order: Vec<(&str, &Endpoint)> = Vec::with_capacity(self.rpcs.len());
        for name in preferred.chain(self.rpcs.keys()) {
            if let Some(endpoint) = self.rpcs.get(name)
                && !order.iter().any(|(seen, _)| *seen == name.as_str())
                && (!http_only || endpoint.is_http())
            {
                order.push((name.as_str(), endpoint));
            }
        }
        order
    }
}

impl Chainz {
    /// Route requests for the configured chain `chain_name` (see the
    /// module docs). Reads the runtime registry once; opens no sockets.
    pub fn route(&self, chain_name: &str, opts: &RouteOpts, runtime: &Runtime) -> Result<Route> {
        let chain = self
            .config()
            .chains
            .get(chain_name)
            .ok_or_else(|| anyhow!("chain '{chain_name}' is not configured"))?;
        let chosen;
        let endpoint_name = match opts.endpoint.as_deref() {
            None => chain.rpc.as_str(),
            Some(rpc) => match chain.rpc_choice(chain_name, rpc, &self.secrets()?.vars)? {
                // A one-off URL: the gateway doesn't know it.
                RpcChoice::New(name, endpoint) => {
                    return self.direct_route(chain_name, chain.id, &name, &endpoint);
                }
                RpcChoice::Existing(name) => {
                    chosen = name;
                    chosen.as_str()
                }
            },
        };
        let endpoint = chain.rpcs.get(endpoint_name).ok_or_else(|| {
            anyhow!(
                "chain '{chain_name}' has no endpoint '{endpoint_name}' (endpoints: {})",
                chain
                    .rpcs
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        if uses_gateway(endpoint.is_websocket(), opts.direct) {
            let state = runtime.gateway().context(
                "a gateway is running, but its record can't be read (--direct bypasses it)",
            )?;
            if let GatewayState::Up(info) = state {
                let pin = opts.endpoint.is_some();
                return Ok(Route::via_gateway(
                    &info,
                    chain_name,
                    chain.id,
                    endpoint_name,
                    pin,
                ));
            }
        }
        self.direct_route(chain_name, chain.id, endpoint_name, endpoint)
    }

    /// Straight to `endpoint` of chain `chain_name` (ID `chain_id`), its
    /// `${VAR}`s expanded; never the gateway. The chain need not be
    /// configured yet (`add` probes candidates this way).
    pub fn direct_route(
        &self,
        chain_name: &str,
        chain_id: u64,
        endpoint_name: &str,
        endpoint: &Endpoint,
    ) -> Result<Route> {
        let expanded = self.expand_endpoint(chain_name, chain_id, endpoint_name, endpoint)?;
        Ok(Route {
            chain_id,
            chain: chain_name.to_string(),
            endpoint: endpoint_name.to_string(),
            url: expanded.url,
            headers: expanded.headers,
            display: redact::url(&endpoint.url, false),
            via_gateway: false,
            websocket: endpoint.is_websocket(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_by_preference_active_then_failover_then_the_rest() {
        let chain: Chain = toml_edit::de::from_str(
            r#"
id = 8453
rpc = "b"
failover = ["c", "missing", "b"]
rpcs.a = "https://a"
rpcs.b = "https://b"
rpcs.c = "https://c"
rpcs.w = "wss://w"
"#,
        )
        .unwrap();
        let names = |http_only| -> Vec<&str> {
            chain
                .endpoints_by_preference(http_only)
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        };
        assert_eq!(names(false), ["b", "c", "a", "w"]);
        assert_eq!(names(true), ["b", "c", "a"]);
    }
}
