//! Endpoint health: the one place that says what "healthy", "lagging" and
//! "failing" mean, how endpoints are ranked, and how probe results are
//! recorded in the [`HealthStore`].
//!
//! # Lag thresholds
//!
//! Three questions use "how far behind" with different answers, on
//! purpose:
//!
//! - [`ENDPOINT_LAG`] (20 blocks): is an endpoint lagging its peers? Used to
//!   rank endpoints (`doctor`, the dashboard). Generous: a
//!   provider a few blocks behind is still a fine default.
//! - [`CHAIN_LAG`] (3 blocks): is a chain's head behind where it should be?
//!   The dashboard's chain status.
//! - [`FAILOVER_LAG`] (2 blocks): may the gateway send a read that failed
//!   over to this endpoint? Strict: a read must not go back in time.
//!
//! An endpoint the gateway refuses as a failover target can therefore rank
//! as healthy here.

use std::time::Duration;

use crate::capabilities::Capabilities;
use crate::error::ErrorClass;
use crate::history::{HealthSample, HealthStore};
use crate::probe::ProbeResult;

/// An endpoint more than this many blocks behind the best head among its
/// peers is lagging.
pub const ENDPOINT_LAG: u64 = 20;
/// A chain whose head is this many blocks behind or more is lagging.
pub const CHAIN_LAG: u64 = 3;
/// A failover endpoint must be within this many blocks of the chain's head
/// high-water mark to take a read.
pub const FAILOVER_LAG: u64 = 2;
/// Successful samples the median latency ([`p50`]) is taken over.
pub const P50_WINDOW: usize = 16;
/// Recorded samples taken this long before the newest of a chain's are not
/// compared with it ([`judge_latest`]): they say nothing about how far
/// behind their endpoint is now. Two of the dashboard's 15 s probe rounds.
pub const ROUND_WINDOW_MS: u64 = 30_000;

/// Median latency over the last [`P50_WINDOW`] successful samples of a
/// history (oldest first, as [`HealthStore::samples`] gives them); the
/// lower median for even counts. `None` when none succeeded.
pub fn p50<'a, I>(samples: I) -> Option<Duration>
where
    I: IntoIterator<Item = &'a HealthSample>,
    I::IntoIter: DoubleEndedIterator,
{
    let latencies = samples
        .into_iter()
        .rev()
        .filter(|s| s.error.is_none())
        .take(P50_WINDOW)
        .map(|s| s.latency_ms);
    median(latencies).map(|ms| Duration::from_millis(u64::from(ms)))
}

/// The median of `values`, the lower one for even counts (`percentile(_,
/// 50)`); `None` for none.
pub fn median(values: impl IntoIterator<Item = u32>) -> Option<u32> {
    let mut values: Vec<u32> = values.into_iter().collect();
    (!values.is_empty()).then(|| percentile(&mut values, 50))
}

/// Nearest-rank percentile of `samples` (sorted in place); the default
/// (zero) for none. Every latency figure chainz reports uses it.
pub fn percentile<T: Ord + Copy + Default>(samples: &mut [T], pct: usize) -> T {
    if samples.is_empty() {
        return T::default();
    }
    samples.sort_unstable();
    let rank = (samples.len() * pct).div_ceil(100).max(1);
    samples[rank - 1]
}

/// One observation of an endpoint: a fresh probe, or a recorded sample.
pub trait Observation {
    /// The head block it reported, if it answered.
    fn head(&self) -> Option<u64>;
    /// Why it failed, if it did.
    fn error(&self) -> Option<ErrorClass>;
    /// How long it took.
    fn latency(&self) -> Duration;
}

impl Observation for ProbeResult {
    fn head(&self) -> Option<u64> {
        self.head
    }
    fn error(&self) -> Option<ErrorClass> {
        self.error.as_ref().map(|e| e.class())
    }
    fn latency(&self) -> Duration {
        self.latency
    }
}

impl Observation for HealthSample {
    fn head(&self) -> Option<u64> {
        self.head
    }
    fn error(&self) -> Option<ErrorClass> {
        self.error
    }
    fn latency(&self) -> Duration {
        Duration::from_millis(u64::from(self.latency_ms))
    }
}

impl<O: Observation + ?Sized> Observation for &O {
    fn head(&self) -> Option<u64> {
        (**self).head()
    }
    fn error(&self) -> Option<ErrorClass> {
        (**self).error()
    }
    fn latency(&self) -> Duration {
        (**self).latency()
    }
}

/// The health of one endpoint relative to its peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Answered, and within [`ENDPOINT_LAG`] blocks of the best head.
    Healthy,
    /// Answered, but more than [`ENDPOINT_LAG`] blocks behind the best head.
    Lagging {
        /// How many blocks behind the best head.
        behind: u64,
    },
    /// Failed with this class of error.
    Failing(ErrorClass),
}

impl Health {
    fn order(self) -> u8 {
        match self {
            Health::Healthy => 0,
            Health::Lagging { .. } => 1,
            Health::Failing(_) => 2,
        }
    }
}

/// Classify each observation. Lagging = more than [`ENDPOINT_LAG`] blocks
/// behind the best head among the successful ones.
pub fn classify<O: Observation>(observations: &[O]) -> Vec<Health> {
    let best = observations
        .iter()
        .filter(|o| o.error().is_none())
        .filter_map(Observation::head)
        .max();
    observations
        .iter()
        .map(|o| match (o.error(), o.head(), best) {
            (Some(class), _, _) => Health::Failing(class),
            (None, Some(head), Some(best)) if best.saturating_sub(head) > ENDPOINT_LAG => {
                Health::Lagging {
                    behind: best - head,
                }
            }
            _ => Health::Healthy,
        })
        .collect()
}

/// Observations classified and ranked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranking {
    /// The health of each observation, by index.
    pub health: Vec<Health>,
    /// Indices best-first: healthy < lagging < failing; healthy and
    /// lagging by latency, ties by index; failing ones are not ranked
    /// (they come last, by index: how fast a refusal came says nothing).
    pub order: Vec<usize>,
}

impl Ranking {
    /// Rank by each observation's own latency.
    pub fn new<O: Observation>(observations: &[O]) -> Ranking {
        Ranking::with_p50(observations, &[])
    }

    /// Rank by `p50[i]` where it is `Some` (a recorded median beats one
    /// probe), else by observation `i`'s own latency.
    pub fn with_p50<O: Observation>(observations: &[O], p50: &[Option<Duration>]) -> Ranking {
        let health = classify(observations);
        let mut order: Vec<usize> = (0..observations.len()).collect();
        order.sort_by_key(|&i| {
            let latency = match health[i] {
                Health::Failing(_) => Duration::ZERO,
                _ => p50
                    .get(i)
                    .copied()
                    .flatten()
                    .unwrap_or_else(|| observations[i].latency()),
            };
            (health[i].order(), latency, i)
        });
        Ranking { health, order }
    }

    /// The best index, if there is anything to rank.
    pub fn best(&self) -> Option<usize> {
        self.order.first().copied()
    }

    /// The best index when it is healthy.
    pub fn best_healthy(&self) -> Option<usize> {
        self.best().filter(|&i| self.health[i] == Health::Healthy)
    }
}

/// One endpoint's latest recorded sample, judged against its peers'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Judged {
    /// Its health.
    pub health: Health,
    /// Blocks behind the best head among its peers, when it reported one.
    pub behind: Option<u64>,
}

/// Judge each endpoint's latest recorded sample (`None`: never probed)
/// against the others': [`classify`] over the samples taken within
/// [`ROUND_WINDOW_MS`] of the newest one. `None` for an endpoint never
/// probed, or probed too long before the rest to compare.
pub fn judge_latest(latest: &[Option<&HealthSample>]) -> Vec<Option<Judged>> {
    let newest = latest.iter().flatten().map(|s| s.at_ms).max().unwrap_or(0);
    let current: Vec<Option<&HealthSample>> = latest
        .iter()
        .map(|s| s.filter(|s| s.at_ms.saturating_add(ROUND_WINDOW_MS) >= newest))
        .collect();
    let observed: Vec<&HealthSample> = current.iter().flatten().copied().collect();
    let best = observed
        .iter()
        .filter(|s| s.error.is_none())
        .filter_map(|s| s.head)
        .max();
    let mut health = classify(&observed).into_iter();
    current
        .into_iter()
        .map(|sample| {
            let sample = sample?;
            Some(Judged {
                health: health.next()?,
                behind: best.zip(sample.head).map(|(b, h)| b.saturating_sub(h)),
            })
        })
        .collect()
}

/// One endpoint's probe, ready to record.
#[derive(Debug, Clone)]
pub struct EndpointProbe {
    /// The chain ID the endpoint was probed for.
    pub chain_id: u64,
    /// The endpoint's name.
    pub endpoint: String,
    /// The probe.
    pub result: ProbeResult,
    /// Its capabilities, when they were probed too.
    pub capabilities: Option<Capabilities>,
}

impl Observation for EndpointProbe {
    fn head(&self) -> Option<u64> {
        self.result.head()
    }
    fn error(&self) -> Option<ErrorClass> {
        self.result.error()
    }
    fn latency(&self) -> Duration {
        self.result.latency()
    }
}

/// Record `probes` in `store` (a sample each, plus capabilities where
/// probed) and save it; see [`HealthStore::save_or_warn`].
pub fn record<'a>(
    store: &mut HealthStore,
    probes: impl IntoIterator<Item = &'a EndpointProbe>,
) -> Option<String> {
    for probe in probes {
        store.record(
            probe.chain_id,
            &probe.endpoint,
            HealthSample::from_probe(&probe.result),
        );
        if let Some(capabilities) = &probe.capabilities {
            store.set_capabilities(probe.chain_id, &probe.endpoint, capabilities.clone());
        }
    }
    store.save_or_warn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RpcError;

    fn ok(ms: u64, head: u64) -> ProbeResult {
        ProbeResult {
            latency: Duration::from_millis(ms),
            head: Some(head),
            error: None,
        }
    }

    fn err(ms: u64, e: RpcError) -> ProbeResult {
        ProbeResult {
            latency: Duration::from_millis(ms),
            head: None,
            error: Some(e),
        }
    }

    #[test]
    fn classify_lagging() {
        let r = [
            ok(10, 1000),
            ok(10, 980),
            ok(10, 979),
            err(1, RpcError::Timeout),
        ];
        assert_eq!(
            classify(&r),
            vec![
                Health::Healthy,
                Health::Healthy,
                Health::Lagging { behind: 21 },
                Health::Failing(ErrorClass::Timeout),
            ]
        );
        assert!(classify::<ProbeResult>(&[]).is_empty());
    }

    #[test]
    fn samples_classify_like_probes() {
        let samples: Vec<HealthSample> = [ok(10, 1000), ok(10, 979), err(1, RpcError::Connect)]
            .iter()
            .map(HealthSample::from_probe)
            .collect();
        assert_eq!(
            classify(&samples),
            vec![
                Health::Healthy,
                Health::Lagging { behind: 21 },
                Health::Failing(ErrorClass::Connect),
            ]
        );
    }

    #[test]
    fn rank_orders_by_class_then_latency_then_index() {
        let r = [
            err(1, RpcError::Connect), // 0 failing
            ok(50, 900),               // 1 lagging
            ok(30, 1000),              // 2 healthy
            ok(20, 1000),              // 3 healthy, faster
            ok(30, 1000),              // 4 healthy, tie with 2
        ];
        let ranking = Ranking::new(&r);
        assert_eq!(ranking.order, vec![3, 2, 4, 1, 0]);
        assert_eq!(ranking.best_healthy(), Some(3));
        // p50 overrides probe latency where present; short p50 slice is fine.
        let p50 = [None, None, Some(Duration::from_millis(5))];
        assert_eq!(Ranking::with_p50(&r, &p50).order, vec![2, 3, 4, 1, 0]);
        // Failures are not ranked by how fast they failed: index order.
        let down = [err(9, RpcError::Connect), err(1, RpcError::Timeout)];
        assert_eq!(Ranking::new(&down).order, vec![0, 1]);
        // Nothing healthy: no best healthy endpoint.
        let failing = Ranking::new(&[err(1, RpcError::Timeout)]);
        assert_eq!((failing.best(), failing.best_healthy()), (Some(0), None));
        assert_eq!(Ranking::new::<ProbeResult>(&[]).best(), None);
    }

    #[test]
    fn p50_is_the_lower_median_of_the_last_successes() {
        let sample = |latency_ms, ok: bool| HealthSample {
            at_ms: 0,
            latency_ms,
            head: ok.then_some(100),
            error: (!ok).then_some(ErrorClass::Timeout),
        };
        assert_eq!(p50(&[]), None);
        assert_eq!(p50(&[sample(5, false)]), None);
        // 4 old slow samples fall out of the window of 16; failures are
        // ignored; 16 left, lower median of 1..=16 is 8.
        let mut samples: Vec<HealthSample> = (0..4).map(|_| sample(1000, true)).collect();
        for ms in 1..=16 {
            samples.push(sample(ms, true));
            samples.push(sample(1, false));
        }
        assert_eq!(p50(&samples), Some(Duration::from_millis(8)));
        assert_eq!(median([3, 1, 2]), Some(2));
        assert_eq!(median([4, 1, 3, 2]), Some(2));
        assert_eq!(median([]), None);
        assert_eq!(percentile::<u32>(&mut [], 50), 0);
        assert_eq!(percentile(&mut [7], 95), 7);
        assert_eq!(percentile(&mut [4, 1, 3, 2], 95), 4);
    }

    #[test]
    fn thresholds_keep_their_values() {
        assert_eq!((ENDPOINT_LAG, CHAIN_LAG, FAILOVER_LAG), (20, 3, 2));
        assert_eq!(P50_WINDOW, 16);
    }

    #[test]
    fn latest_samples_are_judged_within_one_round() {
        let sample = |at_ms, head| HealthSample {
            at_ms,
            latency_ms: 10,
            head: Some(head),
            error: None,
        };
        let now = 1_000_000;
        // `b` is far behind, but its sample is older than a round: not
        // compared, so not lagging either.
        let a = sample(now, 1_000);
        let b = sample(now - ROUND_WINDOW_MS - 1, 900);
        let c = sample(now - 1_000, 990);
        let judged = judge_latest(&[Some(&a), Some(&b), None, Some(&c)]);
        let healthy = |behind| {
            Some(Judged {
                health: Health::Healthy,
                behind: Some(behind),
            })
        };
        assert_eq!(judged, [healthy(0), None, None, healthy(10)]);
        let d = sample(now - ROUND_WINDOW_MS, 900);
        assert_eq!(
            judge_latest(&[Some(&a), Some(&d)])[1].unwrap().health,
            Health::Lagging { behind: 100 }
        );
    }

    #[test]
    fn record_saves_samples_and_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = HealthStore::load(dir.path()).unwrap();
        let probes = [
            EndpointProbe {
                chain_id: 1,
                endpoint: "a".into(),
                result: ok(12, 100),
                capabilities: Some(Capabilities {
                    archive: Some(true),
                    ..Capabilities::default()
                }),
            },
            EndpointProbe {
                chain_id: 1,
                endpoint: "b".into(),
                result: err(4, RpcError::Timeout),
                capabilities: None,
            },
        ];
        assert_eq!(record(&mut store, &probes), None);
        let loaded = HealthStore::load(dir.path()).unwrap();
        assert_eq!(loaded.latest(1, "a").unwrap().head, Some(100));
        assert_eq!(
            loaded.latest(1, "b").unwrap().error,
            Some(ErrorClass::Timeout)
        );
        assert_eq!(loaded.capabilities(1, "a").unwrap().archive, Some(true));
        assert!(loaded.capabilities(1, "b").is_none());
    }
}
