//! What the workers report: one [`Update`] per finished request.

use std::time::Duration;

use chainz_rpc::{ErrorClass, RpcError};

use super::traffic::{FeedItem, TrafficSnapshot};
use crate::plan::Source;

/// A failed request: its class and a message that is safe to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    /// Coarse class.
    pub class: ErrorClass,
    /// Human-readable text; never a URL or a header value.
    pub message: String,
    /// The JSON-RPC error code, when the endpoint answered with one.
    pub code: Option<i64>,
    /// How long a rate-limited endpoint asked to be left alone
    /// (`Retry-After`), when it said.
    pub retry_after: Option<Duration>,
    /// The endpoint asked to slow down
    /// ([`RpcError::is_rate_limited`]).
    pub rate_limited: bool,
}

impl Fault {
    /// A fault with no code and no retry hint.
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Fault {
            class,
            message: message.into(),
            code: None,
            retry_after: None,
            rate_limited: false,
        }
    }
}

impl From<&RpcError> for Fault {
    fn from(error: &RpcError) -> Self {
        Fault {
            class: error.class(),
            message: error.to_string(),
            code: match error {
                RpcError::Rpc { code, .. } => Some(*code),
                _ => None,
            },
            retry_after: match error {
                RpcError::RateLimited { retry_after } => *retry_after,
                _ => None,
            },
            rate_limited: error.is_rate_limited(),
        }
    }
}

/// One successful head poll.
#[derive(Debug, Clone, PartialEq)]
pub struct Head {
    /// Block number.
    pub number: u64,
    /// Block timestamp, unix seconds.
    pub timestamp: Option<u64>,
    /// `baseFeePerGas` in wei, on EIP-1559 chains.
    pub base_fee: Option<u128>,
    /// Round-trip time of the request.
    pub latency_ms: u32,
}

/// One endpoint probe.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    /// Round-trip time (up to the deadline).
    pub latency_ms: u32,
    /// Reported head.
    pub head: Option<u64>,
    /// Why it failed.
    pub error: Option<Fault>,
}

/// How a balance sheet was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// One Multicall3 `aggregate3` call.
    Multicall3,
    /// A JSON-RPC batch of `eth_getBalance` and `eth_call`.
    Batch,
}

/// One signer's balances on one chain.
#[derive(Debug, Clone, PartialEq)]
pub struct Holding {
    /// Signer name.
    pub signer: String,
    /// Native balance in wei.
    pub native: Option<u128>,
    /// Raw token balances, aligned with the chain's `tokens`.
    pub tokens: Vec<Option<u128>>,
}

/// Every signer's balances on one chain.
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    /// How it was read.
    pub via: Via,
    /// One entry per signer with a cached address.
    pub holders: Vec<Holding>,
}

/// What the header shows about a running gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayInfo {
    /// Loopback port.
    pub port: u16,
}

/// A message from a worker to the UI.
#[derive(Debug, Clone)]
pub enum Update {
    /// A chain's head poll finished.
    Head {
        /// Chain name.
        chain: String,
        /// The source it was read from.
        source: Source,
        /// When, unix milliseconds.
        at_ms: u64,
        /// Whether the request went through the gateway.
        via_gateway: bool,
        /// The outcome.
        result: Result<Head, Fault>,
        /// The current average block time estimate, seconds.
        block_time: Option<f64>,
    },
    /// A probe round over a chain's endpoints finished.
    Probes {
        /// Chain name.
        chain: String,
        /// The plan's source for the chain when the round started.
        source: Source,
        /// When, unix milliseconds.
        at_ms: u64,
        /// Endpoint name and outcome, for every probed endpoint.
        results: Vec<(String, Probe)>,
    },
    /// A chain's balances were read.
    Balances {
        /// Chain name.
        chain: String,
        /// The source they were read from.
        source: Source,
        /// When, unix milliseconds.
        at_ms: u64,
        /// The outcome.
        result: Result<Sheet, Fault>,
    },
    /// The gateway started or stopped.
    Gateway(Option<GatewayInfo>),
    /// A status-and-metrics poll of the gateway finished.
    Traffic(Result<TrafficSnapshot, String>),
    /// The gateway proxied a request.
    Feed(FeedItem),
    /// `config.toml` changed on disk.
    ConfigChanged,
}

/// Which worker a refresh is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Poll every chain head now.
    Chains,
    /// Probe the focused chain's endpoints now.
    Endpoints,
    /// Read every balance now.
    Balances,
    /// Poll the gateway's metrics now.
    Traffic,
}
