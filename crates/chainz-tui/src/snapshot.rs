//! The `--once --json` document: one snapshot of everything the tabs
//! show, in a shape meant for `jq`.
//!
//! Amounts that can exceed 2^53 (wei, raw token units) are decimal strings.
//! A value too large to read exactly (2^128 - 1 or more, which only a
//! broken or hostile endpoint reports) is `null`, and amounts say so with
//! `saturated`.
//! URLs are redacted unless `--reveal` (they come from the plan, which
//! applied that choice already).

use serde::Serialize;

use crate::model::{self, Fault, State, Via};
use crate::view::{self, ChainRow};
use chainz_rpc::abi::units::format_exact;

/// The whole document.
#[derive(Debug, Serialize)]
pub struct Snapshot {
    /// When the snapshot was taken, unix seconds.
    pub at: u64,
    /// The gateway, if one runs.
    pub gateway: Gateway,
    /// Every chain, in config order.
    pub chains: Vec<Chain>,
    /// Every signer with a cached address.
    pub balances: Vec<SignerBalances>,
    /// The gateway's status and metrics; `null` without a gateway.
    pub traffic: Option<Traffic>,
}

/// Whether a gateway runs.
#[derive(Debug, Serialize)]
pub struct Gateway {
    /// `chainz serve` holds its lock.
    pub running: bool,
    /// Its loopback port.
    pub port: Option<u16>,
}

/// An error, as a class and a message.
#[derive(Debug, Serialize)]
pub struct Problem {
    /// Stable class (`timeout`, `connect`, `rate_limited`, …).
    pub class: String,
    /// Human-readable text; never a URL.
    pub message: String,
}

/// One chain.
#[derive(Debug, Serialize)]
pub struct Chain {
    /// Short name.
    pub name: String,
    /// Chain ID.
    pub id: u64,
    /// Aliases.
    pub aliases: Vec<String>,
    /// Active endpoint name.
    pub rpc: String,
    /// Whether the head was read through the gateway.
    pub via_gateway: bool,
    /// `ok`, `lag`, `slow`, `down` or `pending`.
    pub status: &'static str,
    /// Head block number.
    pub head: Option<u64>,
    /// Head block timestamp, unix seconds.
    pub block_timestamp: Option<u64>,
    /// Seconds since the head block.
    pub block_age_secs: Option<u64>,
    /// Average block time, seconds.
    pub block_time_secs: Option<f64>,
    /// Base fee in wei; `null` when unknown or saturated.
    pub base_fee_wei: Option<String>,
    /// Base fee in gwei.
    pub base_fee_gwei: Option<f64>,
    /// Latency of the head request.
    pub latency_ms: Option<u32>,
    /// Median latency over the recent history.
    pub p50_ms: Option<u32>,
    /// Blocks behind the best known head.
    pub lag_blocks: u64,
    /// Why the head could not be read.
    pub error: Option<Problem>,
    /// The chain's endpoints, with their last known health.
    pub endpoints: Vec<Endpoint>,
}

/// One endpoint, with its most recent probe (from the health history;
/// `--once` does not probe).
#[derive(Debug, Serialize)]
pub struct Endpoint {
    /// Endpoint name.
    pub name: String,
    /// URL, redacted unless `--reveal`.
    pub url: String,
    /// Whether it is the active endpoint.
    pub active: bool,
    /// `ws://` or `wss://`.
    pub websocket: bool,
    /// `ok`, `lagging`, `failing`, `websocket`, `invalid` or `unknown`.
    pub status: &'static str,
    /// Capability tags.
    pub tags: Vec<String>,
    /// Latency of the last probe.
    pub latency_ms: Option<u32>,
    /// Median latency over the recent probes.
    pub p50_ms: Option<u32>,
    /// Head at the last probe.
    pub head: Option<u64>,
    /// Blocks behind the best endpoint.
    pub lag_blocks: Option<u64>,
    /// Error class of the last probe.
    pub error: Option<String>,
    /// When it was last probed, unix milliseconds.
    pub probed_at_ms: Option<u64>,
}

/// One signer's balances across chains.
#[derive(Debug, Serialize)]
pub struct SignerBalances {
    /// Signer name.
    pub signer: String,
    /// Its cached address.
    pub address: String,
    /// One entry per chain.
    pub chains: Vec<ChainBalances>,
}

/// One signer's balances on one chain.
#[derive(Debug, Serialize)]
pub struct ChainBalances {
    /// Chain name.
    pub chain: String,
    /// Chain ID.
    pub id: u64,
    /// `multicall3` or `batch`.
    pub via: Option<&'static str>,
    /// Native balance.
    pub native: Option<Amount>,
    /// Built-in token balances.
    pub tokens: Vec<TokenBalance>,
    /// Why the balances could not be read.
    pub error: Option<Problem>,
}

/// An amount in raw units and formatted with its decimals.
#[derive(Debug, Serialize)]
pub struct Amount {
    /// Currency symbol, when known.
    pub symbol: Option<String>,
    /// Decimals.
    pub decimals: u8,
    /// Raw units, as a decimal string; `null` when saturated.
    pub raw: Option<String>,
    /// `raw / 10^decimals`, exact; `null` when saturated.
    pub formatted: Option<String>,
    /// The endpoint reported 2^128 - 1 or more: not an amount to trust.
    pub saturated: bool,
}

/// `value`, unless it is the `u128::MAX` a saturating decode stands for.
fn exact(value: u128) -> Option<u128> {
    (value != u128::MAX).then_some(value)
}

/// One token balance.
#[derive(Debug, Serialize)]
pub struct TokenBalance {
    /// Label name (`usdc`).
    pub name: String,
    /// Contract address.
    pub address: String,
    /// The balance; `null` when the call failed.
    pub balance: Option<Amount>,
}

fn amount(a: view::Amount) -> Amount {
    let raw = a.exact();
    Amount {
        symbol: Some(a.symbol.to_string()),
        decimals: a.decimals,
        raw: raw.map(|r| r.to_string()),
        formatted: raw.map(|r| format_exact(r, a.decimals)),
        saturated: raw.is_none(),
    }
}

impl From<&Fault> for Problem {
    fn from(fault: &Fault) -> Self {
        Problem {
            class: fault.class.to_string(),
            message: fault.message.clone(),
        }
    }
}

fn chain(row: &ChainRow) -> Chain {
    let c = row.chain;
    let head = c.head.as_ref();
    let base_fee = head.and_then(|h| h.base_fee).and_then(exact);
    Chain {
        name: c.info.name.clone(),
        id: c.info.id,
        aliases: c.info.aliases.clone(),
        rpc: row.rpc.to_string(),
        via_gateway: c.via_gateway,
        status: row.status.word(),
        head: head.map(|h| h.number),
        block_timestamp: head.and_then(|h| h.timestamp),
        block_age_secs: row.block_age_secs,
        block_time_secs: c.block_time,
        base_fee_wei: base_fee.map(|f| f.to_string()),
        base_fee_gwei: base_fee.map(|f| f as f64 / 1e9),
        latency_ms: head.map(|h| h.latency_ms),
        p50_ms: row.p50_ms,
        lag_blocks: c.lag,
        error: c.fault.as_ref().map(Problem::from),
        endpoints: view::endpoint_rows(c)
            .into_iter()
            .map(|r| Endpoint {
                name: r.endpoint.info.name.clone(),
                url: r.endpoint.info.url.clone(),
                active: r.endpoint.active,
                websocket: r.endpoint.info.websocket,
                status: r.status.word(),
                tags: r.endpoint.tags.clone(),
                latency_ms: r.latency_ms,
                p50_ms: r.p50_ms,
                head: r.latest.and_then(|s| s.head),
                lag_blocks: r.lag,
                error: r
                    .latest
                    .and_then(|s| s.error)
                    .map(|class| class.to_string()),
                probed_at_ms: r.latest.map(|s| s.at_ms),
            })
            .collect(),
    }
}

fn balances(state: &State) -> Vec<SignerBalances> {
    state
        .signers
        .iter()
        .filter_map(|signer| Some((signer, signer.address?)))
        .map(|(signer, address)| SignerBalances {
            signer: signer.name.clone(),
            address: address.to_checksum(),
            chains: view::balance_rows(signer, &state.chains)
                .into_iter()
                .map(|row| ChainBalances {
                    chain: row.chain.info.name.clone(),
                    id: row.chain.info.id,
                    via: row.via.map(|via| match via {
                        Via::Multicall3 => "multicall3",
                        Via::Batch => "batch",
                    }),
                    native: row.native.amount().map(amount),
                    tokens: row
                        .tokens
                        .iter()
                        .map(|(token, balance)| TokenBalance {
                            name: token.name.clone(),
                            address: token.address.to_checksum(),
                            balance: balance.amount().map(amount),
                        })
                        .collect(),
                    error: row.chain.balance_fault.as_ref().map(Problem::from),
                })
                .collect(),
        })
        .collect()
}

/// The gateway's status and metrics.
#[derive(Debug, Serialize)]
pub struct Traffic {
    /// Gateway version.
    pub version: String,
    /// Gateway port.
    pub port: u16,
    /// Seconds since the gateway started.
    pub uptime_secs: u64,
    /// Served chains.
    pub chains: Vec<GatewayChain>,
    /// Metrics windows, shortest first.
    pub windows: Vec<TrafficWindow>,
}

/// The gateway's view of one chain.
#[derive(Debug, Serialize)]
pub struct GatewayChain {
    /// Chain name.
    pub chain: String,
    /// The endpoint it serves from.
    pub active: String,
    /// Head high-water mark.
    pub head: Option<u64>,
}

/// One metrics window.
#[derive(Debug, Serialize)]
pub struct TrafficWindow {
    /// Window length.
    pub window_secs: u64,
    /// Requests in the window.
    pub requests: u64,
    /// Errors in the window.
    pub errors: u64,
    /// 429s in the window.
    pub rate_limited: u64,
    /// Failovers in the window.
    pub failovers: u64,
    /// Response bytes in the window.
    pub bytes: u64,
    /// Per method, busiest first.
    pub methods: Vec<TrafficRow>,
    /// Per `chain/endpoint`, busiest first.
    pub endpoints: Vec<TrafficRow>,
}

/// One method or endpoint within a window.
#[derive(Debug, Serialize)]
pub struct TrafficRow {
    /// Method name, or `chain/endpoint`.
    pub name: String,
    /// Requests.
    pub requests: u64,
    /// Errors of every class.
    pub errors: u64,
    /// HTTP 429s.
    pub rate_limited: u64,
    /// Failovers.
    pub failovers: u64,
    /// Median latency (request-weighted across merged rows).
    pub p50_ms: u32,
    /// Worst 95th percentile among merged rows.
    pub p95_ms: u32,
}

fn traffic(t: &model::TrafficSnapshot) -> Traffic {
    let rows = |rows: &[model::TrafficRow]| -> Vec<TrafficRow> {
        rows.iter()
            .map(|r| TrafficRow {
                name: r.name.clone(),
                requests: r.requests,
                errors: r.errors,
                rate_limited: r.rate_limited,
                failovers: r.failovers,
                p50_ms: r.p50_ms,
                p95_ms: r.p95_ms,
            })
            .collect()
    };
    Traffic {
        version: t.version.clone(),
        port: t.port,
        uptime_secs: t.uptime_secs,
        chains: t
            .chains
            .iter()
            .map(|c| GatewayChain {
                chain: c.chain.clone(),
                active: c.active.clone(),
                head: c.head,
            })
            .collect(),
        windows: t
            .windows
            .iter()
            .map(|w| TrafficWindow {
                window_secs: w.window_secs,
                requests: w.requests,
                errors: w.errors,
                rate_limited: w.rate_limited,
                failovers: w.failovers,
                bytes: w.bytes,
                methods: rows(&w.methods),
                endpoints: rows(&w.endpoints),
            })
            .collect(),
    }
}

impl Snapshot {
    /// The snapshot of `state` at `now_ms`.
    pub fn new(state: &State, now_ms: u64) -> Self {
        Snapshot {
            at: now_ms / 1_000,
            gateway: Gateway {
                running: state.gateway.is_some(),
                port: state.gateway.as_ref().map(|g| g.port),
            },
            chains: view::chain_rows(state, &state.chains, now_ms)
                .iter()
                .map(chain)
                .collect(),
            balances: balances(state),
            traffic: state.traffic.as_ref().map(traffic),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saturated_amount_is_not_printed_as_a_number() {
        let usdc = |raw| view::Amount {
            raw,
            decimals: 6,
            symbol: "USDC",
        };
        let fine = serde_json::to_value(amount(usdc(1_500_000))).unwrap();
        assert_eq!(
            fine,
            serde_json::json!({
                "symbol": "USDC", "decimals": 6, "raw": "1500000",
                "formatted": "1.5", "saturated": false,
            })
        );
        let huge = serde_json::to_value(amount(usdc(u128::MAX))).unwrap();
        assert_eq!(huge["raw"], serde_json::Value::Null);
        assert_eq!(huge["formatted"], serde_json::Value::Null);
        assert_eq!(huge["saturated"], true);
        assert_eq!(exact(u128::MAX - 1), Some(u128::MAX - 1));
    }
}
