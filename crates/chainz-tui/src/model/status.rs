//! Classification: ok / lag / slow / down for chains, ok / lagging /
//! failing for endpoints.

use chainz_rpc::health::CHAIN_LAG;
use chainz_rpc::{ErrorClass, Health, HealthSample, classify};

use super::state::ChainState;
use super::update::{FaultClass, Head};

/// Never call an endpoint slow below this median latency.
pub const SLOW_FLOOR_MS: u32 = 200;
/// An endpoint is slow when its median is this many times the median of
/// all chains (and above [`SLOW_FLOOR_MS`]).
pub const SLOW_FACTOR: u32 = 3;
/// How often the chain on screen has its endpoints probed.
pub const PROBE_EVERY_MS: u64 = 15_000;
/// Endpoint probes this much older than the newest one are not compared
/// with it: two probe intervals.
pub const PROBE_WINDOW_MS: u64 = 2 * PROBE_EVERY_MS;

/// The health of a chain's active endpoint, as shown in the Chains tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Not polled yet.
    Pending,
    /// Answering, current and fast.
    Ok,
    /// This many blocks behind ([`CHAIN_LAG`] or more).
    Lag(u64),
    /// Answering, but with a high median latency.
    Slow,
    /// The last poll failed.
    Down(FaultClass),
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

/// The health of one endpoint, as shown in the Endpoints tab: the
/// probes' [`Health`], plus what the dashboard knows without probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointStatus {
    /// Never probed, or not within [`PROBE_WINDOW_MS`] of the newest probe.
    Unknown,
    /// Answered within [`ENDPOINT_LAG`](chainz_rpc::health::ENDPOINT_LAG) blocks of the best head.
    Ok,
    /// More than [`ENDPOINT_LAG`](chainz_rpc::health::ENDPOINT_LAG) blocks behind the best head.
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
        } else if self.lag >= CHAIN_LAG {
            Status::Lag(self.lag)
        } else if self.p50().is_some_and(|p| p >= slow_ms) {
            Status::Slow
        } else {
            Status::Ok
        }
    }

    /// Status and lag of every endpoint, in order.
    pub fn endpoint_statuses(&self) -> Vec<(EndpointStatus, Option<u64>)> {
        // Probes are compared within one round: a sample from long before
        // the newest says nothing about how far behind its endpoint is now.
        let newest = self
            .endpoints
            .iter()
            .filter_map(|e| Some(e.latest()?.at_ms))
            .max()
            .unwrap_or(0);
        let current: Vec<Option<&HealthSample>> = self
            .endpoints
            .iter()
            .map(|e| {
                e.latest()
                    .filter(|s| s.at_ms.saturating_add(PROBE_WINDOW_MS) >= newest)
            })
            .collect();
        let observed: Vec<&HealthSample> = current.iter().flatten().copied().collect();
        let mut health = classify(&observed).into_iter();
        let best = observed
            .iter()
            .filter(|s| s.error.is_none())
            .filter_map(|s| s.head)
            .max();
        self.endpoints
            .iter()
            .zip(current)
            .map(|(e, sample)| {
                // In step with `observed`: one health per current sample.
                let health = sample.and_then(|_| health.next());
                if e.info.invalid.is_some() {
                    return (EndpointStatus::Invalid, None);
                }
                if e.info.websocket {
                    return (EndpointStatus::Websocket, None);
                }
                let (Some(sample), Some(health)) = (sample, health) else {
                    return (EndpointStatus::Unknown, None);
                };
                let behind = best
                    .zip(sample.head)
                    .map(|(best, head)| best.saturating_sub(head));
                match health {
                    Health::Failing(class) => (EndpointStatus::Failing(class), None),
                    Health::Lagging { behind: n } => (EndpointStatus::Lagging(n), Some(n)),
                    Health::Healthy => (EndpointStatus::Ok, behind),
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
