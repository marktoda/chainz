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

pub use state::{ChainState, EndpointState, State};
pub use status::{EndpointStatus, PROBE_EVERY_MS, Status};
pub use traffic::{FEED_CAP, FeedItem, GatewayChain, TrafficRow, TrafficSnapshot, TrafficWindow};
#[cfg(test)]
pub use update::FaultClass;
pub use update::{Fault, GatewayInfo, Head, Holding, Refresh, Sheet, Update, Via};

fn push_capped<T>(queue: &mut VecDeque<T>, item: T, cap: usize) {
    queue.push_back(item);
    while queue.len() > cap {
        queue.pop_front();
    }
}
