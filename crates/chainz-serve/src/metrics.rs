//! Sliding-window metrics per (chain, endpoint, method).
//!
//! Two rings per series: twelve 5-second buckets for the 1-minute window and
//! fifteen 1-minute buckets for the 15-minute window. A row counts upstream
//! attempts, so a request that failed over appears on both endpoints. A
//! request the endpoint's own rate limiter turned away was never sent: it
//! counts as rate-limited (and as a failover when it moved on), not as an
//! attempt.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;

use chainz_rpc::ErrorClass;
use chainz_rpc::control::{MetricsRow, MetricsSnapshot, MetricsWindow};

/// The short window, in seconds.
pub const SHORT_WINDOW: u64 = 60;
/// The long window, in seconds.
pub const LONG_WINDOW: u64 = 900;

const SHORT_BUCKET: u64 = 5;
const LONG_BUCKET: u64 = 60;
/// Latency samples kept per bucket (overwritten round-robin beyond this).
const SAMPLES: usize = 256;
/// Series kept; attempts beyond this are counted under method `other`
/// until series with no traffic left in either window are dropped.
const MAX_SERIES: usize = 4096;

/// One upstream attempt.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// Attempt latency.
    pub latency_ms: u32,
    /// The error class, if the attempt failed.
    pub error: Option<ErrorClass>,
    /// The endpoint answered 429.
    pub rate_limited: bool,
    /// The request moved on to the next endpoint after this attempt.
    pub failover: bool,
    /// Response bytes.
    pub bytes: u64,
}

#[derive(Debug, Default, Clone)]
struct Bucket {
    requests: u64,
    errors: BTreeMap<ErrorClass, u64>,
    rate_limited: u64,
    failovers: u64,
    bytes: u64,
    latencies: Vec<u32>,
}

impl Bucket {
    fn add(&mut self, s: &Sample) {
        if self.latencies.len() < SAMPLES {
            self.latencies.push(s.latency_ms);
        } else {
            let slot = (self.requests % SAMPLES as u64) as usize;
            self.latencies[slot] = s.latency_ms;
        }
        self.requests += 1;
        if let Some(class) = s.error {
            *self.errors.entry(class).or_default() += 1;
        }
        self.rate_limited += u64::from(s.rate_limited);
        self.failovers += u64::from(s.failover);
        self.bytes += s.bytes;
    }

    fn refuse(&mut self, failover: bool) {
        self.rate_limited += 1;
        self.failovers += u64::from(failover);
    }

    fn merge(&mut self, other: &Bucket) {
        self.requests += other.requests;
        for (class, n) in &other.errors {
            *self.errors.entry(*class).or_default() += n;
        }
        self.rate_limited += other.rate_limited;
        self.failovers += other.failovers;
        self.bytes += other.bytes;
        self.latencies.extend_from_slice(&other.latencies);
    }
}

#[derive(Debug)]
struct Ring {
    width: u64,
    slots: u64,
    buckets: VecDeque<(u64, Bucket)>,
}

impl Ring {
    fn new(width: u64, window: u64) -> Self {
        Ring {
            width,
            slots: window / width,
            buckets: VecDeque::new(),
        }
    }

    fn trim(&mut self, now: u64) {
        let epoch = now / self.width;
        while let Some((e, _)) = self.buckets.front() {
            if e + self.slots <= epoch {
                self.buckets.pop_front();
            } else {
                break;
            }
        }
    }

    /// The bucket for `now`.
    fn current(&mut self, now: u64) -> &mut Bucket {
        self.trim(now);
        let epoch = now / self.width;
        if self.buckets.back().is_none_or(|(e, _)| *e != epoch) {
            self.buckets.push_back((epoch, Bucket::default()));
        }
        &mut self.buckets.back_mut().expect("just pushed").1
    }

    /// Whether nothing is left in the window at `now`.
    fn is_empty(&mut self, now: u64) -> bool {
        self.trim(now);
        self.buckets.is_empty()
    }

    fn total(&mut self, now: u64) -> Option<Bucket> {
        self.trim(now);
        let mut buckets = self.buckets.iter();
        let mut total = buckets.next()?.1.clone();
        for (_, b) in buckets {
            total.merge(b);
        }
        Some(total)
    }
}

#[derive(Debug)]
struct Series {
    short: Ring,
    long: Ring,
}

type Key = (String, String, String);

/// A one-line overview of the short window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Upstream attempts.
    pub requests: u64,
    /// Failed attempts.
    pub errors: u64,
    /// 429 responses.
    pub rate_limited: u64,
    /// Failovers.
    pub failovers: u64,
    /// Median attempt latency.
    pub p50_ms: u32,
}

/// The gateway's metrics store.
#[derive(Debug, Default)]
pub struct Metrics {
    store: Mutex<Store>,
}

#[derive(Debug, Default)]
struct Store {
    series: HashMap<Key, Series>,
    /// When dead series were last dropped to make room (unix seconds).
    pruned_at: Option<u64>,
}

impl Store {
    /// The series for `key`, or for `other` when the store is full of
    /// live series.
    fn series(&mut self, now: u64, chain: &str, endpoint: &str, method: &str) -> &mut Series {
        let mut key = (chain.to_string(), endpoint.to_string(), method.to_string());
        if !self.series.contains_key(&key) && self.series.len() >= MAX_SERIES {
            // Full: drop dead series first (at most once a second, so a
            // store full of live ones doesn't rescan on every attempt).
            if self.pruned_at != Some(now) {
                self.pruned_at = Some(now);
                self.prune(now);
            }
            if self.series.len() >= MAX_SERIES {
                key.2 = "other".to_string();
            }
        }
        self.series.entry(key).or_insert_with(|| Series {
            short: Ring::new(SHORT_BUCKET, SHORT_WINDOW),
            long: Ring::new(LONG_BUCKET, LONG_WINDOW),
        })
    }

    /// Drop series with no traffic left in the long window.
    fn prune(&mut self, now: u64) {
        self.series.retain(|_, s| !s.long.is_empty(now));
    }
}

impl Metrics {
    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record one upstream attempt at unix time `now` (seconds).
    pub fn record(&self, now: u64, chain: &str, endpoint: &str, method: &str, sample: &Sample) {
        let mut store = self.lock();
        let series = store.series(now, chain, endpoint, method);
        series.short.current(now).add(sample);
        series.long.current(now).add(sample);
    }

    /// Record a request the endpoint's rate limiter turned away before it
    /// was sent; `failover` when it moved on to the next endpoint.
    pub fn record_refusal(
        &self,
        now: u64,
        chain: &str,
        endpoint: &str,
        method: &str,
        failover: bool,
    ) {
        let mut store = self.lock();
        let series = store.series(now, chain, endpoint, method);
        series.short.current(now).refuse(failover);
        series.long.current(now).refuse(failover);
    }

    /// Both windows at unix time `now` (seconds). Rows are sorted by chain,
    /// endpoint and method; series with no traffic left are dropped.
    pub fn snapshot(&self, now: u64) -> MetricsSnapshot {
        let mut store = self.lock();
        let series = &mut store.series;
        let mut short = Vec::new();
        let mut long = Vec::new();
        series.retain(|key, s| {
            if let Some(total) = s.short.total(now) {
                short.push(row(key, total));
            }
            match s.long.total(now) {
                Some(total) => {
                    long.push(row(key, total));
                    true
                }
                None => false,
            }
        });
        let sort = |rows: &mut Vec<MetricsRow>| {
            rows.sort_by(|a, b| {
                (&a.chain, &a.endpoint, &a.method).cmp(&(&b.chain, &b.endpoint, &b.method))
            });
        };
        sort(&mut short);
        sort(&mut long);
        MetricsSnapshot {
            at_ms: now * 1000,
            windows: vec![
                MetricsWindow {
                    window_secs: SHORT_WINDOW,
                    rows: short,
                },
                MetricsWindow {
                    window_secs: LONG_WINDOW,
                    rows: long,
                },
            ],
        }
    }

    /// Totals over the short window. Series with no traffic left are
    /// dropped here too, so they don't pile up while nobody asks for a
    /// snapshot.
    pub fn summary(&self, now: u64) -> Summary {
        let mut store = self.lock();
        store.prune(now);
        let mut all = Bucket::default();
        for s in store.series.values_mut() {
            if let Some(total) = s.short.total(now) {
                all.merge(&total);
            }
        }
        Summary {
            requests: all.requests,
            errors: all.errors.values().sum(),
            rate_limited: all.rate_limited,
            failovers: all.failovers,
            p50_ms: percentile(&mut all.latencies, 50),
        }
    }
}

fn row(key: &Key, mut total: Bucket) -> MetricsRow {
    MetricsRow {
        chain: key.0.clone(),
        endpoint: key.1.clone(),
        method: key.2.clone(),
        requests: total.requests,
        p50_ms: percentile(&mut total.latencies, 50),
        p95_ms: percentile(&mut total.latencies, 95),
        errors: total.errors,
        rate_limited: total.rate_limited,
        failovers: total.failovers,
        bytes: total.bytes,
    }
}

/// Nearest-rank percentile; 0 for no samples.
fn percentile(samples: &mut [u32], pct: usize) -> u32 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let rank = (samples.len() * pct).div_ceil(100).max(1);
    samples[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(latency_ms: u32) -> Sample {
        Sample {
            latency_ms,
            bytes: 10,
            ..Sample::default()
        }
    }

    #[test]
    fn windows_slide() {
        let m = Metrics::default();
        let t0 = 1_000_000;
        for i in 0..100 {
            m.record(t0, "base", "infura", "eth_call", &ok(i + 1));
        }
        m.record(
            t0 + 30,
            "base",
            "infura",
            "eth_call",
            &Sample {
                latency_ms: 500,
                error: Some(ErrorClass::RateLimited),
                rate_limited: true,
                failover: true,
                bytes: 0,
            },
        );
        let snap = m.snapshot(t0 + 31);
        assert_eq!(snap.windows[0].window_secs, 60);
        assert_eq!(snap.windows[1].window_secs, 900);
        let short = &snap.windows[0].rows[0];
        assert_eq!(
            (short.requests, short.rate_limited, short.failovers),
            (101, 1, 1)
        );
        assert_eq!(short.errors[&ErrorClass::RateLimited], 1);
        assert_eq!((short.p50_ms, short.p95_ms), (51, 96));
        assert_eq!(short.bytes, 1000);

        // 90 s on: the first burst left the short window but not the long.
        let snap = m.snapshot(t0 + 80);
        assert_eq!(snap.windows[0].rows[0].requests, 1);
        assert_eq!(snap.windows[1].rows[0].requests, 101);
        // Past both windows the series is dropped.
        let snap = m.snapshot(t0 + 2000);
        assert!(snap.windows.iter().all(|w| w.rows.is_empty()));
        assert!(m.lock().series.is_empty());
    }

    #[test]
    fn rows_are_keyed_and_sorted() {
        let m = Metrics::default();
        m.record(10, "eth", "llama", "eth_call", &ok(5));
        m.record(10, "base", "infura", "eth_getLogs", &ok(5));
        m.record(10, "base", "infura", "eth_call", &ok(5));
        let rows = &m.snapshot(10).windows[0].rows;
        let keys: Vec<_> = rows
            .iter()
            .map(|r| (r.chain.as_str(), r.endpoint.as_str(), r.method.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                ("base", "infura", "eth_call"),
                ("base", "infura", "eth_getLogs"),
                ("eth", "llama", "eth_call")
            ]
        );
        let s = m.summary(10);
        assert_eq!((s.requests, s.errors, s.p50_ms), (3, 0, 5));
    }

    #[test]
    fn series_count_is_bounded() {
        let m = Metrics::default();
        for i in 0..(MAX_SERIES + 50) {
            m.record(10, "eth", "a", &format!("m{i}"), &ok(1));
        }
        let rows = &m.snapshot(10).windows[0].rows;
        assert_eq!(rows.len(), MAX_SERIES + 1);
        let other = rows.iter().find(|r| r.method == "other").unwrap();
        assert_eq!(other.requests, 50);
    }

    #[test]
    fn dead_series_make_room_without_a_snapshot() {
        let m = Metrics::default();
        for i in 0..MAX_SERIES {
            m.record(10, "eth", "a", &format!("m{i}"), &ok(1));
        }
        // Past the long window every series is dead: a new method gets its
        // own row rather than `other`, with nobody calling `snapshot`.
        let later = 10 + LONG_WINDOW + LONG_BUCKET;
        m.record(later, "eth", "a", "eth_call", &ok(1));
        let store = m.lock();
        assert_eq!(store.series.len(), 1);
        assert!(store.series.keys().all(|k| k.2 == "eth_call"));
        drop(store);

        // `summary` (the live status line) prunes too.
        m.record(later, "eth", "a", "eth_getLogs", &ok(1));
        m.summary(later + LONG_WINDOW + LONG_BUCKET);
        assert!(m.lock().series.is_empty());
    }

    #[test]
    fn refusals_are_not_attempts() {
        let m = Metrics::default();
        m.record(10, "eth", "a", "eth_call", &ok(40));
        m.record_refusal(10, "eth", "a", "eth_call", true);
        m.record_refusal(10, "eth", "a", "eth_call", false);
        let row = &m.snapshot(10).windows[0].rows[0];
        assert_eq!(row.requests, 1, "a refusal is not an upstream attempt");
        assert_eq!((row.rate_limited, row.failovers), (2, 1));
        assert_eq!((row.p50_ms, row.p95_ms), (40, 40), "no zero latencies");
        assert!(row.errors.is_empty());
        let s = m.summary(10);
        assert_eq!((s.requests, s.rate_limited, s.p50_ms), (1, 2, 40));
    }

    #[test]
    fn percentiles() {
        assert_eq!(percentile(&mut [], 50), 0);
        assert_eq!(percentile(&mut [7], 95), 7);
        assert_eq!(percentile(&mut [4, 1, 3, 2], 50), 2);
        assert_eq!(percentile(&mut [4, 1, 3, 2], 95), 4);
    }
}
