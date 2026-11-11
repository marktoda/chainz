//! The `--once --json` document: one snapshot of everything the tabs
//! show, in a shape meant for `jq`.
//!
//! Amounts that can exceed 2^53 (wei, raw token units) are decimal strings.
//! URLs are redacted unless `--reveal` (they come from the plan, which
//! applied that choice already).

use serde::Serialize;

use crate::fmt;
use crate::model::{ChainState, State, TrafficSnapshot, Via};

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
    pub traffic: Option<TrafficSnapshot>,
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
    /// Base fee in wei.
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
    pub via: Option<Via>,
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
    /// Raw units, as a decimal string.
    pub raw: String,
    /// `raw / 10^decimals`, exact.
    pub formatted: String,
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

fn amount(raw: u128, decimals: u8, symbol: Option<&str>) -> Amount {
    Amount {
        symbol: symbol.map(str::to_string),
        decimals,
        raw: raw.to_string(),
        formatted: fmt::units_exact(raw, decimals),
    }
}

fn chain(c: &ChainState, slow_ms: u32, now_ms: u64) -> Chain {
    let head = c.head.as_ref();
    let statuses = c.endpoint_statuses();
    Chain {
        name: c.plan.name.clone(),
        id: c.plan.id,
        aliases: c.plan.aliases.clone(),
        rpc: c.plan.active.clone(),
        via_gateway: c.via_gateway,
        status: c.status(slow_ms).word(),
        head: head.map(|h| h.number),
        block_timestamp: head.and_then(|h| h.timestamp),
        block_age_secs: c.block_age(now_ms),
        block_time_secs: c.block_time,
        base_fee_wei: head.and_then(|h| h.base_fee).map(|f| f.to_string()),
        base_fee_gwei: head.and_then(|h| h.base_fee).map(|f| f as f64 / 1e9),
        latency_ms: head.map(|h| h.latency_ms),
        p50_ms: c.p50(),
        lag_blocks: c.lag,
        error: c.fault.as_ref().map(|f| Problem {
            class: f.class.to_string(),
            message: f.message.clone(),
        }),
        endpoints: c
            .endpoints
            .iter()
            .zip(statuses)
            .map(|(e, (status, lag))| {
                let sample = e.latest();
                Endpoint {
                    name: e.name.clone(),
                    url: e.url.clone(),
                    active: e.active,
                    websocket: e.websocket,
                    status: status.word(),
                    tags: e.tags.clone(),
                    latency_ms: sample.filter(|s| s.error.is_none()).map(|s| s.latency_ms),
                    p50_ms: e.p50(),
                    head: sample.and_then(|s| s.head),
                    lag_blocks: lag,
                    error: sample.and_then(|s| s.error).map(|class| class.to_string()),
                    probed_at_ms: sample.map(|s| s.at_ms),
                }
            })
            .collect(),
    }
}

impl Snapshot {
    /// The snapshot of `state` at `now_ms`.
    pub fn new(state: &State, now_ms: u64) -> Self {
        let slow_ms = state.slow_threshold();
        let balances = state
            .signers
            .iter()
            .filter_map(|signer| Some((signer, signer.address?)))
            .map(|(signer, address)| SignerBalances {
                signer: signer.name.clone(),
                address: address.to_checksum(),
                chains: state
                    .chains
                    .iter()
                    .map(|c| {
                        let holding = c
                            .balances
                            .as_ref()
                            .and_then(|s| s.holders.iter().find(|h| h.signer == signer.name));
                        ChainBalances {
                            chain: c.plan.name.clone(),
                            id: c.plan.id,
                            via: c.balances.as_ref().map(|s| s.via),
                            native: holding
                                .and_then(|h| h.native)
                                .map(|raw| amount(raw, 18, c.plan.native_symbol.as_deref())),
                            tokens: c
                                .plan
                                .tokens
                                .iter()
                                .enumerate()
                                .map(|(i, token)| TokenBalance {
                                    name: token.name.clone(),
                                    address: token.address.to_checksum(),
                                    balance: holding
                                        .and_then(|h| h.tokens.get(i).copied().flatten())
                                        .map(|raw| {
                                            amount(raw, token.decimals, Some(&token.symbol))
                                        }),
                                })
                                .collect(),
                            error: c.balance_fault.as_ref().map(|f| Problem {
                                class: f.class.to_string(),
                                message: f.message.clone(),
                            }),
                        }
                    })
                    .collect(),
            })
            .collect();
        Snapshot {
            at: now_ms / 1_000,
            gateway: Gateway {
                running: state.gateway.is_some(),
                port: state.gateway.as_ref().map(|g| g.port),
            },
            chains: state
                .chains
                .iter()
                .map(|c| chain(c, slow_ms, now_ms))
                .collect(),
            balances,
            traffic: state.traffic.clone(),
        }
    }
}
