//! Chain heads: one request per chain per tick, paced by the chain's own
//! block time.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chainz_rpc::{BlockId, RpcClient, RpcError, Target};
use serde_json::Value;

use super::{Shared, Signals, Tuning};
use crate::model::{Fault, Head, Update};
use crate::plan::ChainPlan;
use chainz_sys::now_ms;

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

/// The longest `Retry-After` a poller honours: past this, a hostile or
/// confused server could park a chain for good.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);

/// What the wait before a chain's next head poll depends on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Pacing {
    /// No key pressed for `idle_after`.
    pub(super) idle: bool,
    /// Consecutive failed polls.
    pub(super) failures: u32,
    /// The last failure's `Retry-After`.
    pub(super) retry_after: Option<Duration>,
    /// The measured block time.
    pub(super) block_time: Option<f64>,
    /// The block time is about to be measured (the look-back is pending).
    pub(super) measuring: bool,
}

/// The wait before the next head poll.
///
/// - Healthy and idle: the ceiling (`max_pace`), whatever the block time.
/// - Healthy, block time about to be measured: the floor (`min_pace`),
///   rather than the slow pace an unknown block time gets.
/// - Healthy: [`pace`].
/// - Failing: `error_pace`, doubling per failure (1×, 2×, 4×), and never
///   past the ceiling, which an idle dashboard waits outright. Only the
///   server can stretch it further: a `Retry-After` (up to
///   [`MAX_RETRY_AFTER`]) is honoured even past the ceiling, since polling
///   sooner would only be refused again on the same metered key.
pub(super) fn next_delay(p: Pacing, tuning: &Tuning) -> Duration {
    let ceiling = tuning.max_pace.max(tuning.min_pace);
    if p.failures > 0 {
        let backoff = if p.idle {
            ceiling
        } else {
            let doublings = (p.failures - 1).min(2);
            tuning
                .error_pace
                .saturating_mul(1 << doublings)
                .min(ceiling)
        };
        let asked = p.retry_after.unwrap_or_default().min(MAX_RETRY_AFTER);
        return backoff.max(asked);
    }
    if p.idle {
        ceiling
    } else if p.measuring {
        tuning.min_pace
    } else {
        pace(p.block_time, tuning)
    }
}

/// A block time above this is not a block time: the timestamps are wrong
/// (or the endpoint is lying), and nothing should be derived from them.
pub const MAX_BLOCK_TIME: f64 = 3_600.0;

/// More blocks per second than any chain makes, with headroom: a jump
/// faster than this between two heads is not believed.
pub const MAX_BLOCK_RATE: u64 = 1_000;

/// How far a head may fall back (a reorg, a lagging peer behind the
/// gateway) before the clock takes it for a new chain of events.
pub const MAX_STEP_BACK: u64 = 64;

/// Whether `later` can follow `earlier` on a real chain: higher, with a
/// timestamp that does not run backwards, at no more than
/// [`MAX_BLOCK_RATE`] blocks a second and no more than [`MAX_BLOCK_TIME`]
/// seconds a block.
fn plausible(earlier: (u64, u64), later: (u64, u64)) -> bool {
    let (Some(blocks), Some(secs)) = (
        later.0.checked_sub(earlier.0),
        later.1.checked_sub(earlier.1),
    ) else {
        return false;
    };
    blocks > 0
        && blocks <= secs.saturating_add(1).saturating_mul(MAX_BLOCK_RATE)
        && secs as f64 / blocks as f64 <= MAX_BLOCK_TIME
}

/// Average block time from the `(number, timestamp)` pairs seen so far.
///
/// The pairs come from an untrusted endpoint. One that does not fit with
/// the newest (a head at `u64::MAX`, a timestamp centuries ahead) starts
/// the clock over from itself, rather than being kept or, worse, becoming
/// a high-water mark that every honest head after it is measured against.
/// The next honest head does not fit with a bogus one either, so the clock
/// recovers after a single bad poll.
#[derive(Debug, Clone, Default)]
pub struct BlockClock {
    seen: VecDeque<(u64, u64)>,
}

impl BlockClock {
    /// Observations kept; the estimate spans from the oldest to the newest.
    const CAPACITY: usize = 64;

    /// Record a block. A repeated head and a small step back (a reorg)
    /// are ignored; anything else implausible resets the clock.
    pub fn observe(&mut self, number: u64, timestamp: u64) {
        let block = (number, timestamp);
        match self.seen.back() {
            None => self.seen.push_back(block),
            Some(&newest) if plausible(newest, block) => self.seen.push_back(block),
            Some(&(newest, _)) if number <= newest && newest - number <= MAX_STEP_BACK => {}
            Some(_) => {
                self.seen.clear();
                self.seen.push_back(block);
            }
        }
        while self.seen.len() > Self::CAPACITY {
            self.seen.pop_front();
        }
    }

    /// Record an older block to measure from; one that does not fit with
    /// the oldest seen is ignored.
    pub fn anchor(&mut self, number: u64, timestamp: u64) {
        match self.seen.front() {
            Some(&oldest) if plausible((number, timestamp), oldest) => {
                self.seen.push_front((number, timestamp));
            }
            _ => {}
        }
    }

    /// Seconds per block, once two different blocks have been seen.
    ///
    /// `None` when the pair is implausible: timestamps that do not advance
    /// with the block number, or an implied block time over
    /// `MAX_BLOCK_TIME`. The inputs come from an untrusted endpoint.
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
    /// The `Retry-After` of the last failure, if it had one.
    retry_after: Option<Duration>,
}

impl Poller {
    /// A poller that learns the block time on its first poll (`--once`).
    ///
    /// What an earlier poller learned about the chain (its block clock, and
    /// whether the look-back block was read) carries over: an engine
    /// restart does not cost another look-back request.
    pub(super) fn new(shared: Arc<Shared>, chain: ChainPlan) -> Self {
        let learned = shared.lock_learned().get(&chain.id).cloned();
        let (clock, anchor_tries) = learned.unwrap_or((BlockClock::default(), 3));
        Poller {
            shared,
            chain,
            clock,
            anchor_tries,
            anchor_after: 0,
            failures: 0,
            retry_after: None,
        }
    }

    /// A poller that learns the block time on its second poll.
    pub(super) fn patient(shared: Arc<Shared>, chain: ChainPlan) -> Self {
        Poller {
            anchor_after: 1,
            ..Poller::new(shared, chain)
        }
    }

    /// Poll forever: first after `offset` (the provider stagger), then
    /// every [`next_delay`], and early on a refresh. A refresh wakes every
    /// poller at once, so each waits its `offset` again before polling:
    /// chains on one provider stay a beat apart.
    pub(super) async fn run(mut self, offset: Duration, signals: Arc<Signals>) {
        tokio::time::sleep(offset).await;
        loop {
            let update = self.step().await;
            self.shared.send(update);
            let delay = next_delay(self.pacing(), &self.shared.tuning);
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = signals.chains.notified() => tokio::time::sleep(offset).await,
            }
        }
    }

    /// What the next wait depends on, as of the last poll.
    pub(super) fn pacing(&self) -> Pacing {
        Pacing {
            idle: self.shared.idle(),
            failures: self.failures,
            retry_after: self.retry_after,
            block_time: self.clock.block_time(),
            measuring: self.clock.block_time().is_none() && self.anchor_tries > 0,
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
        self.retry_after = result.as_ref().err().and_then(|f| f.retry_after);
        self.shared
            .lock_learned()
            .insert(self.chain.id, (self.clock.clone(), self.anchor_tries));
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
    chainz_rpc::hex::parse_quantity_saturating(block.get(name)?.as_str()?).ok()
}

/// The head, from one `eth_getBlockByNumber("latest")`.
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
