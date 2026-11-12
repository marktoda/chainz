//! The state the renderer and `--once --json` read, and how updates fold
//! into it.

use std::collections::VecDeque;

use chainz_rpc::health::{self, P50_WINDOW};
use chainz_rpc::{HealthSample, HealthStore};

use super::push_capped;
use super::status::{SLOW_FACTOR, SLOW_FLOOR_MS, lag_blocks, median};
use super::traffic::{FEED_CAP, FeedItem, TrafficSnapshot};
use super::update::{Fault, GatewayInfo, Head, Sheet, Update};
use crate::plan::{ChainInfo, ChainPlan, EndpointInfo, Plan, SignerPlan, Source};

/// The span the Traffic tab's request rate is measured over.
pub const FEED_RATE_SECS: u64 = 10;

/// One endpoint's live state.
#[derive(Debug, Clone)]
pub struct EndpointState {
    /// Name, displayable URL (redacted unless `--reveal`), websocket, and
    /// why it cannot be used.
    pub info: EndpointInfo,
    /// Whether it is the chain's active endpoint in the config (the one
    /// `s` replaces; not necessarily the one serving, see
    /// [`State::serving`]).
    pub active: bool,
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
        health::p50(&self.samples).map(|p50| u32::try_from(p50.as_millis()).unwrap_or(u32::MAX))
    }
}

/// One chain's live state.
#[derive(Debug, Clone)]
pub struct ChainState {
    /// What is shown about it.
    pub info: ChainInfo,
    /// Where its head and balances are read from; updates from another
    /// source are dropped.
    pub source: Source,
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
    /// failure. Seeded from the health store, extended by every poll, and
    /// as long as the store keeps ([`HealthStore::CAPACITY`]).
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
        let id = plan.info.id;
        let endpoints: Vec<EndpointState> = plan
            .endpoints
            .iter()
            .map(|e| EndpointState {
                info: e.info.clone(),
                active: e.info.name == plan.info.active,
                tags: store
                    .capabilities(id, &e.info.name)
                    .map(|c| c.tags())
                    .unwrap_or_else(|| {
                        if e.info.websocket {
                            vec!["ws".to_string()]
                        } else {
                            Vec::new()
                        }
                    }),
                samples: store.samples(id, &e.info.name).into(),
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
            info: plan.info.clone(),
            source: plan.source,
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

    /// Median latency of the active endpoint over the recent polls (the
    /// same window as the probes' [`health::p50`]).
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

    /// The best head any endpoint of this chain reported in this session.
    fn best_probed_head(&self) -> Option<u64> {
        self.endpoints
            .iter()
            .filter(|e| e.live)
            .filter_map(|e| e.latest()?.head)
            .max()
    }
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
    /// old one, so a config change does not blank the screen. What was read
    /// from the chain's source (head, latency, lag) is kept only where the
    /// source is the same ([`Plan::inherit`]).
    pub fn rebuilt(&self, plan: &Plan, store: &HealthStore) -> Self {
        let mut next = State::new(plan, store);
        for chain in &mut next.chains {
            let Some(old) = self
                .chains
                .iter()
                .find(|c| c.info.name == chain.info.name && c.info.id == chain.info.id)
            else {
                continue;
            };
            chain.block_time = old.block_time;
            chain.balances = old.balances.clone();
            chain.balances_at_ms = old.balances_at_ms;
            if old.source == chain.source {
                chain.head = old.head.clone();
                chain.polled_at_ms = old.polled_at_ms;
                chain.fault = old.fault.clone();
                chain.via_gateway = old.via_gateway;
                chain.lag = old.lag;
                chain.samples = old.samples.clone();
            }
            chain.probed_at_ms = old.probed_at_ms;
            for endpoint in &mut chain.endpoints {
                if let Some(previous) = old
                    .endpoints
                    .iter()
                    .find(|e| e.info.name == endpoint.info.name)
                {
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

    /// The endpoint serving `chain`: the one the gateway reports serving
    /// it from when the last head poll went through the gateway (it may
    /// have failed over from the configured one), else the configured
    /// active endpoint. The RPC column, `y`, the Endpoints tab's marker
    /// and the JSON all name this one.
    pub fn serving<'a>(&'a self, chain: &'a ChainState) -> &'a str {
        let from_gateway = chain
            .via_gateway
            .then_some(self.traffic.as_ref())
            .flatten()
            .and_then(|t| t.chains.iter().find(|c| c.chain == chain.info.name))
            .map(|c| c.active.as_str());
        from_gateway.unwrap_or(&chain.info.active)
    }

    /// The median latency above which a chain is `slow`: three times the
    /// median across chains, and never under [`SLOW_FLOOR_MS`].
    pub fn slow_threshold(&self) -> u32 {
        median(self.chains.iter().filter_map(ChainState::p50)).map_or(SLOW_FLOOR_MS, |m| {
            m.saturating_mul(SLOW_FACTOR).max(SLOW_FLOOR_MS)
        })
    }

    /// Requests per second over the last [`FEED_RATE_SECS`] of the feed.
    pub fn feed_rate(&self, now_ms: u64) -> f64 {
        let since = now_ms.saturating_sub(FEED_RATE_SECS * 1_000);
        let recent = self.feed.iter().rev().take_while(|r| r.at_ms >= since);
        recent.count() as f64 / FEED_RATE_SECS as f64
    }

    /// The chain named `name`, if it still reads from `source`: what was
    /// read from another source (an endpoint since replaced) is dropped.
    fn chain_from(&mut self, name: &str, source: Source) -> Option<&mut ChainState> {
        self.chains
            .iter_mut()
            .find(|c| c.info.name == name && c.source == source)
    }

    /// Fold one worker message into the state.
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Head {
                chain,
                source,
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
                let Some(c) = self.chain_from(&chain, source) else {
                    return;
                };
                c.polled_at_ms = Some(at_ms);
                c.via_gateway = via_gateway;
                if block_time.is_some() {
                    c.block_time = block_time;
                }
                match result {
                    Ok(head) => {
                        push_capped(&mut c.samples, Some(head.latency_ms), HealthStore::CAPACITY);
                        let best = c.best_probed_head().max(gateway_head);
                        c.lag = lag_blocks(&head, best, c.block_time, at_ms);
                        c.head = Some(head);
                        c.fault = None;
                    }
                    Err(fault) => {
                        push_capped(&mut c.samples, None, HealthStore::CAPACITY);
                        c.fault = Some(fault);
                    }
                }
            }
            Update::Probes {
                chain,
                source,
                at_ms,
                results,
            } => {
                let Some(c) = self.chain_from(&chain, source) else {
                    return;
                };
                c.probed_at_ms = Some(at_ms);
                for (name, sample, message) in results {
                    let Some(e) = c.endpoints.iter_mut().find(|e| e.info.name == name) else {
                        continue;
                    };
                    e.live = true;
                    e.message = message;
                    push_capped(&mut e.samples, sample, HealthStore::CAPACITY);
                }
            }
            Update::Balances {
                chain,
                source,
                at_ms,
                result,
            } => {
                let Some(c) = self.chain_from(&chain, source) else {
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
            // A poll that was under way when the gateway stopped.
            Update::Traffic(_) | Update::Feed(_) if self.gateway.is_none() => {}
            Update::Traffic(Ok(snapshot)) => {
                self.traffic = Some(snapshot);
                self.traffic_error = None;
            }
            Update::Traffic(Err(message)) => self.traffic_error = Some(message),
            // A resubscription replays the backlog: keep only what is newer
            // than the newest record shown, so nothing doubles and nothing
            // lands out of order.
            Update::Feed(item) => {
                let newer = self
                    .feed
                    .back()
                    .is_none_or(|last| (item.at_ms, item.id) > (last.at_ms, last.id));
                if newer {
                    push_capped(&mut self.feed, item, FEED_CAP);
                }
            }
        }
    }
}
