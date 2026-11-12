//! The view model: [`State`] turned into the rows each tab shows, with
//! every figure derived once. The ratatui renderers (`ui`), the `--once`
//! text tables and the `--once --json` document all read these, so they
//! cannot disagree on a status, a median or which endpoint serves.

use chainz_core::labels::NATIVE_DECIMALS;
use chainz_rpc::HealthSample;
use chainz_rpc::control::Window;

use crate::model::{
    ChainState, EndpointState, EndpointStatus, Holding, State, Status, TrafficSnapshot,
    TrafficWindow, Via,
};
use crate::plan::{SignerPlan, TokenPlan};

/// One chain in the Chains tab.
#[derive(Debug, Clone, Copy)]
pub struct ChainRow<'a> {
    /// The chain.
    pub chain: &'a ChainState,
    /// Its status.
    pub status: Status,
    /// The endpoint serving it ([`State::serving`]).
    pub rpc: &'a str,
    /// Median latency of the recent head polls.
    pub p50_ms: Option<u32>,
    /// Seconds since the head block.
    pub block_age_secs: Option<u64>,
    /// Average block time, seconds.
    pub block_time_secs: Option<f64>,
    /// The head's base fee in wei; `None` when unknown or [`SATURATED`].
    pub base_fee_wei: Option<u128>,
}

impl ChainRow<'_> {
    /// The base fee in gwei.
    pub fn base_fee_gwei(&self) -> Option<f64> {
        self.base_fee_wei.map(|wei| wei as f64 / 1e9)
    }
}

/// The rows for `chains` (a subset of `state`'s, in order) at `now_ms`.
pub fn chain_rows<'a>(
    state: &'a State,
    chains: impl IntoIterator<Item = &'a ChainState>,
    now_ms: u64,
) -> Vec<ChainRow<'a>> {
    let slow_ms = state.slow_threshold();
    chains
        .into_iter()
        .map(|chain| ChainRow {
            chain,
            status: chain.status(slow_ms),
            rpc: state.serving(chain),
            p50_ms: chain.p50(),
            block_age_secs: chain.block_age(now_ms),
            block_time_secs: chain.block_time,
            base_fee_wei: chain.head.as_ref().and_then(|h| h.base_fee).and_then(exact),
        })
        .collect()
}

/// One endpoint in the Endpoints tab.
#[derive(Debug, Clone, Copy)]
pub struct EndpointRow<'a> {
    /// The endpoint.
    pub endpoint: &'a EndpointState,
    /// Whether it serves the chain ([`State::serving`]).
    pub serving: bool,
    /// Its status.
    pub status: EndpointStatus,
    /// Blocks behind the best endpoint, when both reported a head.
    pub lag: Option<u64>,
    /// The latest probe.
    pub latest: Option<&'a HealthSample>,
    /// The latest probe's latency, when it succeeded.
    pub latency_ms: Option<u32>,
    /// Median latency over the recent probes.
    pub p50_ms: Option<u32>,
}

/// The rows of `chain`'s endpoints, in config order.
pub fn endpoint_rows<'a>(state: &'a State, chain: &'a ChainState) -> Vec<EndpointRow<'a>> {
    let serving = state.serving(chain);
    chain
        .endpoints
        .iter()
        .zip(chain.endpoint_statuses())
        .map(|(endpoint, (status, lag))| {
            let latest = endpoint.latest();
            EndpointRow {
                endpoint,
                serving: endpoint.info.name == serving,
                status,
                lag,
                latest,
                latency_ms: latest.filter(|s| s.error.is_none()).map(|s| s.latency_ms),
                p50_ms: endpoint.p50(),
            }
        })
        .collect()
}

/// What a saturating decode reads for 2^128 - 1 or more, which only a
/// broken or hostile endpoint reports: not a figure to show or trust.
pub const SATURATED: u128 = u128::MAX;

/// `raw`, unless it is [`SATURATED`].
pub fn exact(raw: u128) -> Option<u128> {
    (raw != SATURATED).then_some(raw)
}

/// An amount in raw units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Amount<'a> {
    /// Raw units; [`SATURATED`] when the read saturated (see [`exact`]).
    pub raw: u128,
    /// Decimals.
    pub decimals: u8,
    /// Currency symbol.
    pub symbol: &'a str,
}

/// One balance cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Balance<'a> {
    /// Not read yet.
    Pending,
    /// The chain's balances could not be read.
    Unread,
    /// Read, but this one call failed.
    Failed,
    /// Read.
    Read(Amount<'a>),
}

impl<'a> Balance<'a> {
    /// The amount, when there is one.
    pub fn amount(self) -> Option<Amount<'a>> {
        match self {
            Balance::Read(amount) => Some(amount),
            _ => None,
        }
    }
}

/// One signer's balances on one chain.
#[derive(Debug, Clone)]
pub struct BalanceRow<'a> {
    /// The chain.
    pub chain: &'a ChainState,
    /// The native balance.
    pub native: Balance<'a>,
    /// Every built-in token on the chain, in the chain's order.
    pub tokens: Vec<(&'a TokenPlan, Balance<'a>)>,
    /// How the balances were read.
    pub via: Option<Via>,
}

impl<'a> BalanceRow<'a> {
    /// The balance of token `name`; `None` when the chain has no such
    /// token.
    pub fn token(&self, name: &str) -> Option<Balance<'a>> {
        self.tokens
            .iter()
            .find(|(token, _)| token.name == name)
            .map(|(_, balance)| *balance)
    }
}

/// `signer`'s balances on each of `chains`.
pub fn balance_rows<'a>(
    signer: &SignerPlan,
    chains: impl IntoIterator<Item = &'a ChainState>,
) -> Vec<BalanceRow<'a>> {
    chains
        .into_iter()
        .map(|chain| {
            let holding: Option<&Holding> = chain
                .balances
                .as_ref()
                .and_then(|sheet| sheet.holders.iter().find(|h| h.signer == signer.name));
            let unread = if chain.balance_fault.is_some() {
                Balance::Unread
            } else {
                Balance::Pending
            };
            let read = |value: Option<u128>, decimals, symbol| match value {
                Some(raw) => Balance::Read(Amount {
                    raw,
                    decimals,
                    symbol,
                }),
                None => Balance::Failed,
            };
            BalanceRow {
                chain,
                native: holding.map_or(unread, |h| {
                    read(h.native, NATIVE_DECIMALS, chain.info.native_symbol)
                }),
                tokens: chain
                    .info
                    .tokens
                    .iter()
                    .enumerate()
                    .map(|(i, token)| {
                        let balance = holding.map_or(unread, |h| {
                            let value = h.tokens.get(i).copied().flatten();
                            read(value, token.decimals, &token.symbol)
                        });
                        (token, balance)
                    })
                    .collect(),
                via: chain.balances.as_ref().map(|sheet| sheet.via),
            }
        })
        .collect()
}

/// The window the Traffic tab shows: the gateway's short one.
pub fn traffic_window(traffic: &TrafficSnapshot) -> Option<&TrafficWindow> {
    traffic
        .windows
        .iter()
        .find(|w| w.window_secs == Window::Short.secs())
}
