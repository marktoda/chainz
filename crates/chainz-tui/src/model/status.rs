//! Classification: ok / lag / slow / down for chains, ok / lagging /
//! failing for endpoints.

use chainz_rpc::{ErrorClass, LAG_THRESHOLD};

use super::state::ChainState;
use super::update::Head;

/// A chain head this many blocks behind the best head seen for its chain
/// is shown as `lag N`.
pub const CHAIN_LAG_MIN: u64 = 3;
/// Never call an endpoint slow below this median latency.
pub const SLOW_FLOOR_MS: u32 = 200;
/// An endpoint is slow when its median is this many times the median of
/// all chains (and above [`SLOW_FLOOR_MS`]).
pub const SLOW_FACTOR: u32 = 3;

/// The health of a chain's active endpoint, as shown in the Chains tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Not polled yet.
    Pending,
    /// Answering, current and fast.
    Ok,
    /// This many blocks behind.
    Lag(u64),
    /// Answering, but with a high median latency.
    Slow,
    /// The last poll failed.
    Down(ErrorClass),
}

impl Status {
    /// The stable lowercase word used in JSON.
    pub fn word(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Ok => "ok",
            Status::Lag(_) => "lag",
            Status::Slow => "slow",
            Status::Down(_) => "down",
        }
    }
}

/// The health of one endpoint, as shown in the Endpoints tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointStatus {
    /// Never probed.
    Unknown,
    /// Answered within [`LAG_THRESHOLD`] blocks of the best head.
    Ok,
    /// More than [`LAG_THRESHOLD`] blocks behind the best head.
    Lagging(u64),
    /// The last probe failed.
    Failing(ErrorClass),
    /// A websocket endpoint: listed, not probed.
    Websocket,
    /// The endpoint's URL or variables do not resolve.
    Invalid,
}

impl EndpointStatus {
    /// The stable lowercase word used in JSON.
    pub fn word(self) -> &'static str {
        match self {
            EndpointStatus::Unknown => "unknown",
            EndpointStatus::Ok => "ok",
            EndpointStatus::Lagging(_) => "lagging",
            EndpointStatus::Failing(_) => "failing",
            EndpointStatus::Websocket => "websocket",
            EndpointStatus::Invalid => "invalid",
        }
    }
}

impl ChainState {
    /// Classify the chain; `slow_ms` is [`State::slow_threshold`].
    pub fn status(&self, slow_ms: u32) -> Status {
        if let Some(fault) = &self.fault {
            Status::Down(fault.class)
        } else if self.head.is_none() {
            Status::Pending
        } else if self.lag >= CHAIN_LAG_MIN {
            Status::Lag(self.lag)
        } else if self.p50().is_some_and(|p| p >= slow_ms) {
            Status::Slow
        } else {
            Status::Ok
        }
    }

    /// Status and lag of every endpoint, in order.
    pub fn endpoint_statuses(&self) -> Vec<(EndpointStatus, Option<u64>)> {
        let best = self
            .endpoints
            .iter()
            .filter_map(|e| e.latest().filter(|s| s.error.is_none())?.head)
            .max();
        self.endpoints
            .iter()
            .map(|e| {
                if e.invalid.is_some() {
                    return (EndpointStatus::Invalid, None);
                }
                if e.websocket {
                    return (EndpointStatus::Websocket, None);
                }
                let Some(sample) = e.latest() else {
                    return (EndpointStatus::Unknown, None);
                };
                if let Some(class) = sample.error {
                    return (EndpointStatus::Failing(class), None);
                }
                let behind = best
                    .zip(sample.head)
                    .map(|(best, head)| best.saturating_sub(head));
                match behind {
                    Some(n) if n > LAG_THRESHOLD => (EndpointStatus::Lagging(n), behind),
                    _ => (EndpointStatus::Ok, behind),
                }
            })
            .collect()
    }
}

/// How far `head` trails: against the best head known for the chain, and
/// against the clock when the block is much older than the chain's block
/// time allows (a node that stopped syncing has nobody to compare with).
pub fn lag_blocks(
    head: &Head,
    best_known: Option<u64>,
    block_time: Option<f64>,
    now_ms: u64,
) -> u64 {
    let behind = best_known.map_or(0, |best| best.saturating_sub(head.number));
    let stale = match (head.timestamp, block_time) {
        (Some(timestamp), Some(block_time)) if block_time > 0.0 => {
            let age = (now_ms / 1_000).saturating_sub(timestamp) as f64;
            // Slack for clock skew, missed slots and sequencer pauses.
            if age > 5.0 * block_time + 20.0 {
                (age / block_time) as u64
            } else {
                0
            }
        }
        _ => 0,
    };
    behind.max(stale)
}

pub(super) fn median(values: impl Iterator<Item = u32>) -> Option<u32> {
    let mut values: Vec<u32> = values.collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[(values.len() - 1) / 2])
}
