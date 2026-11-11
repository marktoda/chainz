//! The dashboard's state: what the workers report ([`Update`]) and what
//! the renderer and `--once --json` read ([`State`]).
//!
//! Everything here is plain data and pure functions, so classification
//! (ok / lag / slow / down) is tested without a network or a terminal.

mod state;
mod status;
#[cfg(test)]
mod tests;
mod traffic;
mod update;

use std::collections::VecDeque;

pub use state::{ChainState, EndpointState, P50_WINDOW, SAMPLE_CAP, State};
pub use status::{CHAIN_LAG_MIN, EndpointStatus, SLOW_FACTOR, SLOW_FLOOR_MS, Status, lag_blocks};
pub use traffic::{FEED_CAP, FeedItem, GatewayChain, TrafficRow, TrafficSnapshot, TrafficWindow};
pub use update::{Fault, GatewayInfo, Head, Holding, Probe, Sheet, Update, Via};

fn push_capped<T>(queue: &mut VecDeque<T>, item: T, cap: usize) {
    queue.push_back(item);
    while queue.len() > cap {
        queue.pop_front();
    }
}

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
