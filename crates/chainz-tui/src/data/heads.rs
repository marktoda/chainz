//! Chain heads: one request per chain per tick, paced by the chain's own
//! block time.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chainz_rpc::{BlockId, RpcClient, RpcError, Target};
use serde_json::Value;

use super::{Shared, Signals, Tuning, abi};
use crate::model::{Fault, Head, Update, now_ms};
use crate::plan::ChainPlan;

/// How long to wait before the next head poll: the chain's block time,
/// kept within `[min_pace, max_pace]` (3–12 s by default). An unknown block
/// time polls at the slow end.
///
/// The floor is 3 s rather than the block time of the fastest chains
/// because every poll is a billed request on a metered API key, and several
/// chains usually share one key.
///
/// `block_time` derives from timestamps an RPC endpoint reported, so it is
/// untrusted: it is clamped as a float, before it becomes a `Duration`
/// (`Duration::from_secs_f64` panics on a value that does not fit).
pub fn pace(block_time: Option<f64>, tuning: &Tuning) -> Duration {
    let (min, max) = (tuning.min_pace, tuning.max_pace.max(tuning.min_pace));
    match block_time {
        Some(secs) if secs.is_finite() && secs > 0.0 => {
            let clamped = secs.clamp(min.as_secs_f64(), max.as_secs_f64());
            Duration::try_from_secs_f64(clamped).unwrap_or(max)
        }
        _ => max,
    }
}

/// A block time above this is not a block time: the timestamps are wrong
/// (or the endpoint is lying), and nothing should be derived from them.
pub const MAX_BLOCK_TIME: f64 = 3_600.0;

/// Average block time from the `(number, timestamp)` pairs seen so far.
#[derive(Debug, Default)]
pub struct BlockClock {
    seen: VecDeque<(u64, u64)>,
}

impl BlockClock {
    /// Observations kept; the estimate spans from the oldest to the newest.
    const CAPACITY: usize = 64;

    /// Record a block; out-of-order and repeated heads are ignored.
    pub fn observe(&mut self, number: u64, timestamp: u64) {
        match self.seen.back() {
            Some((newest, _)) if number <= *newest => {}
            _ => self.seen.push_back((number, timestamp)),
        }
        while self.seen.len() > Self::CAPACITY {
            self.seen.pop_front();
        }
    }

    /// Record an older block to measure from.
    pub fn anchor(&mut self, number: u64, timestamp: u64) {
        match self.seen.front() {
            Some((oldest, _)) if number >= *oldest => {}
            _ => self.seen.push_front((number, timestamp)),
        }
    }

    /// Seconds per block, once two different blocks have been seen.
    ///
    /// `None` when the pair is implausible: timestamps that do not advance
    /// with the block number, or an implied block time over
    /// [`MAX_BLOCK_TIME`]. The inputs come from an untrusted endpoint.
    pub fn block_time(&self) -> Option<f64> {
        let (first, first_at) = *self.seen.front()?;
        let (last, last_at) = *self.seen.back()?;
        let blocks = last.checked_sub(first).filter(|n| *n > 0)?;
        let elapsed = last_at.checked_sub(first_at).filter(|s| *s > 0)?;
        let secs = elapsed as f64 / blocks as f64;
        (secs.is_finite() && secs <= MAX_BLOCK_TIME).then_some(secs)
    }
}

/// The poller for one chain.
pub(super) struct Poller {
    shared: Arc<Shared>,
    chain: ChainPlan,
    clock: BlockClock,
    /// Tries left to fetch the look-back block.
    anchor_tries: u8,
    /// Successful polls to let pass before fetching it: the live poller
    /// keeps its first tick to one request.
    anchor_after: u8,
    /// Consecutive failed polls.
    failures: u32,
}

impl Poller {
    /// A poller that learns the block time on its first poll (`--once`).
    pub(super) fn new(shared: Arc<Shared>, chain: ChainPlan) -> Self {
        Poller {
            shared,
            chain,
            clock: BlockClock::default(),
            anchor_tries: 3,
            anchor_after: 0,
            failures: 0,
        }
    }

    /// A poller that learns the block time on its second poll.
    pub(super) fn patient(shared: Arc<Shared>, chain: ChainPlan) -> Self {
        Poller {
            anchor_after: 1,
            ..Poller::new(shared, chain)
        }
    }

    /// Poll forever, starting after `delay` and waking early on a refresh.
    pub(super) async fn run(mut self, delay: Duration, signals: Arc<Signals>) {
        tokio::time::sleep(delay).await;
        loop {
            let update = self.step().await;
            self.shared.send(update);
            let tuning = &self.shared.tuning;
            let delay = match self.failures {
                // Nobody is watching: poll at the ceiling until a key.
                0 if self.shared.idle() => tuning.max_pace,
                // The block time is about to be measured: come back at
                // the floor rather than the slow pace an unknown one gets.
                0 if self.clock.block_time().is_none() && self.anchor_tries > 0 => tuning.min_pace,
                0 => pace(self.clock.block_time(), tuning),
                // Back off a failing endpoint: 1×, 2×, then 4× the error pace.
                n => tuning.error_pace * (1 << (n - 1).min(2)),
            };
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = signals.chains.notified() => {}
            }
        }
    }

    /// One poll.
    pub(super) async fn step(&mut self) -> Update {
        let (target, via_gateway) = self.shared.route(&self.chain);
        let deadline = self.shared.tuning.request_timeout;
        let result = match target {
            Err(fault) => Err(fault),
            Ok(target) => {
                let client = self.shared.client.clone();
                let polled = tokio::time::timeout(deadline, poll_head(&client, &target))
                    .await
                    .unwrap_or(Err(RpcError::Timeout));
                if let Ok(head) = &polled
                    && let Some(timestamp) = head.timestamp
                {
                    self.clock.observe(head.number, timestamp);
                    self.anchor(&client, &target, head.number, deadline).await;
                }
                polled.map_err(|e| Fault::from(&e))
            }
        };
        self.failures = match result {
            Ok(_) => 0,
            Err(_) => self.failures.saturating_add(1),
        };
        Update::Head {
            chain: self.chain.name.clone(),
            at_ms: now_ms(),
            via_gateway,
            result,
            block_time: self.clock.block_time(),
        }
    }

    /// Once per chain: read a block `block_span` back, so the block time is
    /// known from the first poll instead of after several.
    async fn anchor(&mut self, client: &RpcClient, target: &Target, head: u64, deadline: Duration) {
        if self.anchor_tries == 0 || head == 0 {
            return;
        }
        if self.anchor_after > 0 {
            self.anchor_after -= 1;
            return;
        }
        self.anchor_tries -= 1;
        let number = head.saturating_sub(self.shared.tuning.block_span);
        let block = client.get_block(target, BlockId::Number(number), false);
        if let Ok(Ok(Some(block))) = tokio::time::timeout(deadline, block).await
            && let Some(timestamp) = field(&block, "timestamp")
        {
            self.clock
                .anchor(number, u64::try_from(timestamp).unwrap_or(u64::MAX));
            self.anchor_tries = 0;
        }
    }
}

fn field(block: &Value, name: &str) -> Option<u128> {
    abi::quantity(block.get(name)?.as_str()?)
}

/// The head, from one `eth_getBlockByNumber("latest")`.
///
/// The design pairs it with `eth_blockNumber` in a batch, but the block
/// already carries its number, and on metered providers the second call
/// doubles the cost of every tick for nothing.
async fn poll_head(client: &RpcClient, target: &Target) -> Result<Head, RpcError> {
    let start = Instant::now();
    let block = client.get_block(target, BlockId::Latest, false).await?;
    let latency_ms = u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX);
    let as_u64 = |n: u128| u64::try_from(n).unwrap_or(u64::MAX);
    let block = block.filter(Value::is_object);
    match block.as_ref().and_then(|b| field(b, "number")) {
        Some(number) => Ok(Head {
            number: as_u64(number),
            timestamp: block
                .as_ref()
                .and_then(|b| field(b, "timestamp"))
                .map(as_u64),
            base_fee: block.as_ref().and_then(|b| field(b, "baseFeePerGas")),
            latency_ms,
        }),
        None => Err(RpcError::InvalidResponse("no head in response".into())),
    }
}
