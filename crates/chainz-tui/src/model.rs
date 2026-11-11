//! The dashboard's state: what the workers report ([`Update`]) and what
//! the renderer and `--once --json` read ([`State`]).
//!
//! Everything here is plain data and pure functions, so classification
//! (ok / lag / slow / down) is tested without a network or a terminal.

use std::collections::VecDeque;

use chainz_rpc::{ErrorClass, HealthSample, HealthStore, LAG_THRESHOLD, RpcError};

use crate::plan::{ChainPlan, Plan, SignerPlan};

/// A chain head this many blocks behind the best head seen for its chain
/// is shown as `lag N`.
pub const CHAIN_LAG_MIN: u64 = 3;
/// Never call an endpoint slow below this median latency.
pub const SLOW_FLOOR_MS: u32 = 200;
/// An endpoint is slow when its median is this many times the median of
/// all chains (and above [`SLOW_FLOOR_MS`]).
pub const SLOW_FACTOR: u32 = 3;
/// Latency samples kept per chain for the sparkline and the median.
pub const SAMPLE_CAP: usize = 64;
/// Samples the median latency is taken over.
pub const P50_WINDOW: usize = 16;
/// Request records kept for the Traffic feed.
pub const FEED_CAP: usize = 500;

/// A failed request: its class and a message that is safe to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    /// Coarse class.
    pub class: ErrorClass,
    /// Human-readable text; never a URL or a header value.
    pub message: String,
}

impl From<&RpcError> for Fault {
    fn from(error: &RpcError) -> Self {
        Fault {
            class: error.class(),
            message: error.to_string(),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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

/// The gateway's view of one chain.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GatewayChain {
    /// Chain name.
    pub chain: String,
    /// Active endpoint.
    pub active: String,
    /// Head high-water mark.
    pub head: Option<u64>,
}

/// Totals for one method or one endpoint within a metrics window.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
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

/// One metrics window, aggregated two ways.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
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

/// Gateway status plus metrics at one instant.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct TrafficSnapshot {
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

/// One proxied request in the live feed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FeedItem {
    /// Record ID.
    pub id: u64,
    /// Arrival time, unix milliseconds.
    pub at_ms: u64,
    /// Chain name.
    pub chain: String,
    /// JSON-RPC method.
    pub method: String,
    /// The gateway's labelled, already-redacted parameter summary.
    pub summary: String,
    /// Endpoint that served it.
    pub upstream: String,
    /// Attempts across endpoints.
    pub attempts: u32,
    /// Total latency.
    pub latency_ms: u32,
    /// Error class and JSON-RPC code, when it failed.
    pub error: Option<(ErrorClass, Option<i64>)>,
    /// Response size.
    pub bytes: u64,
}

/// A message from a worker to the UI.
#[derive(Debug, Clone)]
pub enum Update {
    /// A chain's head poll finished.
    Head {
        /// Chain name.
        chain: String,
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
        /// When, unix milliseconds.
        at_ms: u64,
        /// Endpoint name and outcome, for every probed endpoint.
        results: Vec<(String, Probe)>,
    },
    /// A chain's balances were read.
    Balances {
        /// Chain name.
        chain: String,
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

/// One endpoint's live state.
#[derive(Debug, Clone)]
pub struct EndpointState {
    /// Endpoint name.
    pub name: String,
    /// Displayable URL (redacted unless `--reveal`).
    pub url: String,
    /// Whether it is the chain's active endpoint.
    pub active: bool,
    /// `ws://` or `wss://`.
    pub websocket: bool,
    /// Why it cannot be used, when its config does not resolve.
    pub invalid: Option<String>,
    /// Cached capability tags.
    pub tags: Vec<String>,
    /// Probe history, oldest first: the health store's plus this session's.
    pub samples: VecDeque<HealthSample>,
    /// Whether the latest sample is from this session.
    pub live: bool,
    /// The latest failure's message.
    pub message: Option<String>,
}

impl EndpointState {
    /// The most recent probe.
    pub fn latest(&self) -> Option<&HealthSample> {
        self.samples.back()
    }

    /// Median latency over the recent successful probes.
    pub fn p50(&self) -> Option<u32> {
        median(
            self.samples
                .iter()
                .rev()
                .filter(|s| s.error.is_none())
                .take(P50_WINDOW)
                .map(|s| s.latency_ms),
        )
    }
}

/// One chain's live state.
#[derive(Debug, Clone)]
pub struct ChainState {
    /// Its plan.
    pub plan: ChainPlan,
    /// The last head read successfully.
    pub head: Option<Head>,
    /// When the last poll finished, unix milliseconds.
    pub polled_at_ms: Option<u64>,
    /// Why the last poll failed, if it did.
    pub fault: Option<Fault>,
    /// Whether the last poll went through the gateway.
    pub via_gateway: bool,
    /// Average block time, seconds.
    pub block_time: Option<f64>,
    /// Blocks behind the best head known when the last poll landed.
    pub lag: u64,
    /// Latency history of the active endpoint, oldest first; `None` is a
    /// failure. Seeded from the health store, extended by every poll.
    pub samples: VecDeque<Option<u32>>,
    /// Its endpoints, in config order.
    pub endpoints: Vec<EndpointState>,
    /// When its endpoints were last probed in this session.
    pub probed_at_ms: Option<u64>,
    /// The last balances read successfully.
    pub balances: Option<Sheet>,
    /// When balances were last read, unix milliseconds.
    pub balances_at_ms: Option<u64>,
    /// Why the last balance read failed, if it did.
    pub balance_fault: Option<Fault>,
}

impl ChainState {
    fn new(plan: &ChainPlan, store: &HealthStore) -> Self {
        let endpoints: Vec<EndpointState> = plan
            .endpoints
            .iter()
            .map(|e| EndpointState {
                name: e.name.clone(),
                url: e.url.clone(),
                active: e.name == plan.active,
                websocket: e.websocket,
                invalid: e.target.as_ref().err().cloned(),
                tags: store
                    .capabilities(plan.id, &e.name)
                    .map(|c| c.tags())
                    .unwrap_or_else(|| {
                        if e.websocket {
                            vec!["ws".to_string()]
                        } else {
                            Vec::new()
                        }
                    }),
                samples: store.samples(plan.id, &e.name).into(),
                live: false,
                message: None,
            })
            .collect();
        let samples = endpoints
            .iter()
            .find(|e| e.active)
            .map(|e| {
                e.samples
                    .iter()
                    .map(|s| s.error.is_none().then_some(s.latency_ms))
                    .collect()
            })
            .unwrap_or_default();
        ChainState {
            plan: plan.clone(),
            head: None,
            polled_at_ms: None,
            fault: None,
            via_gateway: false,
            block_time: None,
            lag: 0,
            samples,
            endpoints,
            probed_at_ms: None,
            balances: None,
            balances_at_ms: None,
            balance_fault: None,
        }
    }

    /// Median latency of the active endpoint over the recent polls.
    pub fn p50(&self) -> Option<u32> {
        median(
            self.samples
                .iter()
                .rev()
                .flatten()
                .take(P50_WINDOW)
                .copied(),
        )
    }

    /// Seconds since the head block was produced.
    pub fn block_age(&self, now_ms: u64) -> Option<u64> {
        let timestamp = self.head.as_ref()?.timestamp?;
        Some((now_ms / 1_000).saturating_sub(timestamp))
    }

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

    /// The best head any endpoint of this chain reported in this session.
    fn best_probed_head(&self) -> Option<u64> {
        self.endpoints
            .iter()
            .filter(|e| e.live)
            .filter_map(|e| e.latest()?.head)
            .max()
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

fn median(values: impl Iterator<Item = u32>) -> Option<u32> {
    let mut values: Vec<u32> = values.collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[(values.len() - 1) / 2])
}

/// Everything the dashboard knows right now.
#[derive(Debug, Clone, Default)]
pub struct State {
    /// Chains, in config order.
    pub chains: Vec<ChainState>,
    /// Signers, in config order.
    pub signers: Vec<SignerPlan>,
    /// Token label names: the Balances tab's columns.
    pub token_names: Vec<String>,
    /// The running gateway, if any.
    pub gateway: Option<GatewayInfo>,
    /// The gateway's latest status and metrics.
    pub traffic: Option<TrafficSnapshot>,
    /// Why the gateway's control socket could not be read.
    pub traffic_error: Option<String>,
    /// The most recent proxied requests, oldest first.
    pub feed: VecDeque<FeedItem>,
}

impl State {
    /// A fresh state for `plan`, seeded with the persisted health history.
    pub fn new(plan: &Plan, store: &HealthStore) -> Self {
        State {
            chains: plan
                .chains
                .iter()
                .map(|c| ChainState::new(c, store))
                .collect(),
            signers: plan.signers.clone(),
            token_names: plan.token_names(),
            ..State::default()
        }
    }

    /// A state for a reloaded `plan` that keeps what is still true of the
    /// old one, so a config change does not blank the screen.
    pub fn rebuilt(&self, plan: &Plan, store: &HealthStore) -> Self {
        let mut next = State::new(plan, store);
        for chain in &mut next.chains {
            let Some(old) = self
                .chains
                .iter()
                .find(|c| c.plan.name == chain.plan.name && c.plan.id == chain.plan.id)
            else {
                continue;
            };
            chain.block_time = old.block_time;
            chain.balances = old.balances.clone();
            chain.balances_at_ms = old.balances_at_ms;
            if old.plan.active == chain.plan.active {
                chain.head = old.head.clone();
                chain.polled_at_ms = old.polled_at_ms;
                chain.fault = old.fault.clone();
                chain.via_gateway = old.via_gateway;
                chain.lag = old.lag;
                chain.samples = old.samples.clone();
            }
            chain.probed_at_ms = old.probed_at_ms;
            for endpoint in &mut chain.endpoints {
                if let Some(previous) = old.endpoints.iter().find(|e| e.name == endpoint.name) {
                    endpoint.samples = previous.samples.clone();
                    endpoint.live = previous.live;
                    endpoint.message = previous.message.clone();
                }
            }
        }
        next.gateway = self.gateway.clone();
        next.traffic = self.traffic.clone();
        next.traffic_error = self.traffic_error.clone();
        next.feed = self.feed.clone();
        next
    }

    /// The chain named `name`.
    pub fn chain(&self, name: &str) -> Option<&ChainState> {
        self.chains.iter().find(|c| c.plan.name == name)
    }

    /// The median latency above which a chain is `slow`: three times the
    /// median across chains, and never under [`SLOW_FLOOR_MS`].
    pub fn slow_threshold(&self) -> u32 {
        median(self.chains.iter().filter_map(ChainState::p50)).map_or(SLOW_FLOOR_MS, |m| {
            m.saturating_mul(SLOW_FACTOR).max(SLOW_FLOOR_MS)
        })
    }

    /// Requests per second over the last `window_secs` of the feed.
    pub fn feed_rate(&self, now_ms: u64, window_secs: u64) -> f64 {
        let since = now_ms.saturating_sub(window_secs * 1_000);
        let recent = self.feed.iter().rev().take_while(|r| r.at_ms >= since);
        recent.count() as f64 / window_secs.max(1) as f64
    }

    /// Fold one worker message into the state.
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Head {
                chain,
                at_ms,
                via_gateway,
                result,
                block_time,
            } => {
                let gateway_head = self.traffic.as_ref().and_then(|t| {
                    t.chains
                        .iter()
                        .find(|c| c.chain == chain)
                        .and_then(|c| c.head)
                });
                let Some(c) = self.chains.iter_mut().find(|c| c.plan.name == chain) else {
                    return;
                };
                c.polled_at_ms = Some(at_ms);
                c.via_gateway = via_gateway;
                if block_time.is_some() {
                    c.block_time = block_time;
                }
                match result {
                    Ok(head) => {
                        push_capped(&mut c.samples, Some(head.latency_ms), SAMPLE_CAP);
                        let best = c.best_probed_head().max(gateway_head);
                        c.lag = lag_blocks(&head, best, c.block_time, at_ms);
                        c.head = Some(head);
                        c.fault = None;
                    }
                    Err(fault) => {
                        push_capped(&mut c.samples, None, SAMPLE_CAP);
                        c.fault = Some(fault);
                    }
                }
            }
            Update::Probes {
                chain,
                at_ms,
                results,
            } => {
                let Some(c) = self.chains.iter_mut().find(|c| c.plan.name == chain) else {
                    return;
                };
                c.probed_at_ms = Some(at_ms);
                for (name, probe) in results {
                    let Some(e) = c.endpoints.iter_mut().find(|e| e.name == name) else {
                        continue;
                    };
                    e.live = true;
                    e.message = probe.error.as_ref().map(|f| f.message.clone());
                    let sample = HealthSample {
                        at_ms,
                        latency_ms: probe.latency_ms,
                        head: probe.head,
                        error: probe.error.map(|f| f.class),
                    };
                    push_capped(&mut e.samples, sample, HealthStore::CAPACITY);
                }
            }
            Update::Balances {
                chain,
                at_ms,
                result,
            } => {
                let Some(c) = self.chains.iter_mut().find(|c| c.plan.name == chain) else {
                    return;
                };
                c.balances_at_ms = Some(at_ms);
                match result {
                    Ok(sheet) => {
                        c.balances = Some(sheet);
                        c.balance_fault = None;
                    }
                    Err(fault) => c.balance_fault = Some(fault),
                }
            }
            Update::Gateway(info) => {
                if info.is_none() {
                    self.traffic = None;
                    self.traffic_error = None;
                }
                self.gateway = info;
            }
            Update::Traffic(Ok(snapshot)) => {
                self.traffic = Some(snapshot);
                self.traffic_error = None;
            }
            Update::Traffic(Err(message)) => self.traffic_error = Some(message),
            // A resubscription replays the backlog: skip what is already shown.
            Update::Feed(item) => {
                let shown = |f: &FeedItem| f.id == item.id && f.at_ms == item.at_ms;
                if !self.feed.iter().any(shown) {
                    push_capped(&mut self.feed, item, FEED_CAP);
                }
            }
            // Handled by the event loop, which owns the config.
            Update::ConfigChanged => {}
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::testing::{chain, plan};

    const NOW: u64 = 1_700_000_000_000;

    fn head(number: u64, age_secs: u64) -> Head {
        Head {
            number,
            timestamp: Some(NOW / 1_000 - age_secs),
            base_fee: None,
            latency_ms: 50,
        }
    }

    #[test]
    fn lag_is_measured_against_the_best_known_head() {
        let h = head(1_000, 2);
        assert_eq!(lag_blocks(&h, None, Some(2.0), NOW), 0);
        assert_eq!(lag_blocks(&h, Some(1_000), Some(2.0), NOW), 0);
        assert_eq!(lag_blocks(&h, Some(1_003), Some(2.0), NOW), 3);
        // A peer that is itself behind does not make this one "ahead".
        assert_eq!(lag_blocks(&h, Some(990), Some(2.0), NOW), 0);
    }

    #[test]
    fn lag_is_also_measured_against_the_clock() {
        // A 2 s chain whose newest block is 10 minutes old has stalled,
        // even with no peer to compare against: about 300 blocks behind.
        assert_eq!(lag_blocks(&head(1_000, 600), None, Some(2.0), NOW), 300);
        // Ordinary jitter, a missed slot and some clock skew are not lag.
        assert_eq!(lag_blocks(&head(1_000, 29), None, Some(2.0), NOW), 0);
        assert_eq!(lag_blocks(&head(1_000, 75), None, Some(12.0), NOW), 0);
        assert_eq!(lag_blocks(&head(1_000, 90), None, Some(12.0), NOW), 7);
        // A block "from the future" (skewed clock) is not lag either.
        let mut future = head(1_000, 0);
        future.timestamp = Some(NOW / 1_000 + 30);
        assert_eq!(lag_blocks(&future, None, Some(2.0), NOW), 0);
        // Without a block time or a timestamp, only peers can tell.
        assert_eq!(lag_blocks(&head(1_000, 600), None, None, NOW), 0);
        future.timestamp = None;
        assert_eq!(lag_blocks(&future, Some(1_010), Some(2.0), NOW), 10);
        // The larger of the two wins.
        assert_eq!(
            lag_blocks(&head(1_000, 600), Some(1_500), Some(2.0), NOW),
            500
        );
    }

    fn state() -> State {
        let plan = plan(vec![
            chain(
                "eth",
                1,
                &[
                    ("a", "http://127.0.0.1:1"),
                    ("b", "http://127.0.0.1:2"),
                    ("c", "http://127.0.0.1:3"),
                    ("d", "ws://127.0.0.1:4"),
                ],
            ),
            chain("base", 8453, &[("a", "http://127.0.0.1:5")]),
            chain("arb", 42161, &[("a", "http://127.0.0.1:6")]),
        ]);
        let store = HealthStore::load(std::path::Path::new("/nonexistent/chainz")).unwrap();
        State::new(&plan, &store)
    }

    fn polled(chain: &str, number: u64, latency_ms: u32) -> Update {
        Update::Head {
            chain: chain.into(),
            at_ms: NOW,
            via_gateway: false,
            result: Ok(Head {
                latency_ms,
                ..head(number, 1)
            }),
            block_time: Some(2.0),
        }
    }

    fn failed(chain: &str, class: ErrorClass) -> Update {
        Update::Head {
            chain: chain.into(),
            at_ms: NOW,
            via_gateway: false,
            result: Err(Fault {
                class,
                message: "failed".into(),
            }),
            block_time: None,
        }
    }

    fn probes(results: &[(&str, Option<u64>, Option<ErrorClass>)]) -> Update {
        Update::Probes {
            chain: "eth".into(),
            at_ms: NOW,
            results: results
                .iter()
                .map(|(name, head, error)| {
                    let probe = Probe {
                        latency_ms: 40,
                        head: *head,
                        error: error.map(|class| Fault {
                            class,
                            message: "failed".into(),
                        }),
                    };
                    (name.to_string(), probe)
                })
                .collect(),
        }
    }

    #[test]
    fn chain_status_is_pending_ok_lag_slow_or_down() {
        let mut state = state();
        let status = |state: &State, i: usize| state.chains[i].status(state.slow_threshold());
        assert_eq!(status(&state, 0), Status::Pending);

        state.apply(polled("eth", 1_000, 50));
        assert_eq!(status(&state, 0), Status::Ok);

        // Two blocks behind a peer is noise; three is lag.
        state.apply(probes(&[("b", Some(1_002), None)]));
        state.apply(polled("eth", 1_000, 50));
        assert_eq!(status(&state, 0), Status::Ok);
        state.apply(probes(&[("b", Some(1_003), None)]));
        assert_eq!(
            status(&state, 0),
            Status::Ok,
            "judged when the head is polled"
        );
        state.apply(polled("eth", 1_000, 50));
        assert_eq!(status(&state, 0), Status::Lag(3));
        // Catching up clears it.
        state.apply(polled("eth", 1_003, 50));
        assert_eq!(status(&state, 0), Status::Ok);

        // A failed poll is down, whatever came before; the head is kept.
        state.apply(failed("eth", ErrorClass::Timeout));
        assert_eq!(status(&state, 0), Status::Down(ErrorClass::Timeout));
        assert_eq!(state.chains[0].head.as_ref().unwrap().number, 1_003);
        assert_eq!(state.chains[0].samples.back(), Some(&None));
        state.apply(polled("eth", 1_004, 50));
        assert_eq!(status(&state, 0), Status::Ok);

        // The gateway's head high-water mark counts as a peer.
        state.apply(Update::Traffic(Ok(TrafficSnapshot {
            chains: vec![GatewayChain {
                chain: "base".into(),
                active: "a".into(),
                head: Some(2_010),
            }],
            ..TrafficSnapshot::default()
        })));
        state.apply(polled("base", 2_000, 50));
        assert_eq!(status(&state, 1), Status::Lag(10));

        // Lag outranks slow; down outranks both.
        for _ in 0..8 {
            state.apply(polled("base", 2_000, 5_000));
        }
        assert_eq!(status(&state, 1), Status::Lag(10));
        state.apply(failed("base", ErrorClass::RateLimited));
        assert_eq!(status(&state, 1), Status::Down(ErrorClass::RateLimited));
        // An update for a chain that no longer exists is dropped.
        state.apply(polled("gone", 1, 1));
    }

    #[test]
    fn slow_is_relative_to_the_other_chains_with_a_floor() {
        let mut state = state();
        let status = |state: &State, i: usize| state.chains[i].status(state.slow_threshold());
        assert_eq!(state.slow_threshold(), SLOW_FLOOR_MS, "no data: the floor");
        // Everyone around 50 ms: 199 ms is fine, 200 ms is slow.
        state.apply(polled("eth", 1, 50));
        state.apply(polled("base", 1, 55));
        state.apply(polled("arb", 1, 199));
        assert_eq!(state.slow_threshold(), 200);
        assert_eq!(status(&state, 2), Status::Ok);
        for _ in 0..3 {
            state.apply(polled("arb", 1, 240));
        }
        assert_eq!(status(&state, 2), Status::Slow);
        // One spike does not make a chain slow: it is the median.
        state.apply(polled("eth", 1, 3_000));
        assert_eq!(status(&state, 0), Status::Ok);
        // On a slow link everything is slow, so nothing is: 3× the median.
        let mut far = super::tests::state();
        far.apply(polled("eth", 1, 400));
        far.apply(polled("base", 1, 450));
        far.apply(polled("arb", 1, 500));
        assert_eq!(far.slow_threshold(), 1_350);
        assert!((0..3).all(|i| status(&far, i) == Status::Ok));
    }

    #[test]
    fn endpoints_lag_past_twenty_blocks_behind_the_best() {
        let mut state = state();
        let statuses = |state: &State| -> Vec<EndpointStatus> {
            state.chains[0]
                .endpoint_statuses()
                .iter()
                .map(|s| s.0)
                .collect()
        };
        assert_eq!(
            statuses(&state),
            [
                EndpointStatus::Unknown,
                EndpointStatus::Unknown,
                EndpointStatus::Unknown,
                EndpointStatus::Websocket,
            ]
        );
        state.apply(probes(&[
            ("a", Some(1_000), None),
            ("b", Some(980), None),
            ("c", Some(979), None),
        ]));
        assert_eq!(
            statuses(&state)[..3],
            [
                EndpointStatus::Ok,
                EndpointStatus::Ok,
                EndpointStatus::Lagging(21)
            ]
        );
        // A failing endpoint's old head does not set the bar.
        state.apply(probes(&[
            ("a", None, Some(ErrorClass::Connect)),
            ("b", Some(990), None),
            ("c", Some(985), None),
        ]));
        assert_eq!(
            statuses(&state)[..3],
            [
                EndpointStatus::Failing(ErrorClass::Connect),
                EndpointStatus::Ok,
                EndpointStatus::Ok,
            ]
        );
        assert!(state.chains[0].endpoints[0].live);
        assert_eq!(state.chains[0].endpoints[1].p50(), Some(40));
        assert_eq!(state.chains[0].probed_at_ms, Some(NOW));
    }

    #[test]
    fn a_reloaded_plan_keeps_what_is_still_true() {
        let mut state = state();
        state.apply(polled("eth", 1_000, 50));
        state.apply(polled("base", 2_000, 60));
        state.apply(probes(&[("b", Some(1_000), None)]));
        state.apply(Update::Gateway(Some(GatewayInfo { port: 18545 })));

        // `eth` switches to endpoint b; `arb` is removed; `op` is new.
        let mut eth = state.chains[0].plan.clone();
        eth.active = "b".into();
        let next = plan(vec![
            eth,
            state.chains[1].plan.clone(),
            chain("op", 10, &[("a", "http://127.0.0.1:7")]),
        ]);
        let store = HealthStore::load(std::path::Path::new("/nonexistent/chainz")).unwrap();
        let rebuilt = state.rebuilt(&next, &store);

        let names: Vec<&str> = rebuilt
            .chains
            .iter()
            .map(|c| c.plan.name.as_str())
            .collect();
        assert_eq!(names, ["eth", "base", "op"]);
        // The head came from the old endpoint: it is not b's.
        assert!(rebuilt.chains[0].head.is_none());
        assert_eq!(rebuilt.chains[0].block_time, Some(2.0));
        assert!(rebuilt.chains[0].endpoints[1].active);
        assert!(
            rebuilt.chains[0].endpoints[1].live,
            "probe history is per endpoint"
        );
        // An untouched chain keeps everything.
        assert_eq!(rebuilt.chains[1].head.as_ref().unwrap().number, 2_000);
        assert!(rebuilt.chains[2].head.is_none());
        assert_eq!(rebuilt.gateway, Some(GatewayInfo { port: 18545 }));
    }

    #[test]
    fn feed_is_capped_and_its_rate_counts_the_recent_window() {
        let mut state = state();
        for i in 0..(FEED_CAP as u64 + 50) {
            state.apply(Update::Feed(FeedItem {
                id: i,
                at_ms: NOW - 20_000 + i * 40,
                chain: "eth".into(),
                method: "eth_call".into(),
                summary: String::new(),
                upstream: "a".into(),
                attempts: 1,
                latency_ms: 10,
                error: None,
                bytes: 0,
            }));
        }
        assert_eq!(state.feed.len(), FEED_CAP);
        assert_eq!(state.feed.front().unwrap().id, 50);
        // A replayed record (the backlog after a reconnect) is not doubled.
        let again = state.feed.back().unwrap().clone();
        state.apply(Update::Feed(again));
        assert_eq!(state.feed.len(), FEED_CAP);
        assert_eq!(state.feed.front().unwrap().id, 50);
        // 25 requests a second, over the last 10 s.
        let newest = state.feed.back().unwrap().at_ms;
        assert_eq!(state.feed_rate(newest, 10), 25.1);
        assert_eq!(state.feed_rate(newest + 60_000, 10), 0.0);
    }
}
