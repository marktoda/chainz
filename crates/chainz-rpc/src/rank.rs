//! Endpoint health classification and ranking from probe results.

use std::time::Duration;

use crate::error::ErrorClass;
use crate::probe::{LAG_THRESHOLD, ProbeResult};

/// The health of one endpoint relative to its peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Answered, and within [`LAG_THRESHOLD`] blocks of the best head.
    Healthy,
    /// Answered, but more than [`LAG_THRESHOLD`] blocks behind the best head.
    Lagging {
        /// How many blocks behind the best head.
        behind: u64,
    },
    /// The probe failed with this class of error.
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

/// Classify each result. Lagging = more than [`LAG_THRESHOLD`] blocks behind
/// the best head among the successful `results`.
pub fn classify(results: &[ProbeResult]) -> Vec<Health> {
    let best = results
        .iter()
        .filter(|r| r.is_ok())
        .filter_map(|r| r.head)
        .max();
    results
        .iter()
        .map(|r| match (&r.error, r.head, best) {
            (Some(e), _, _) => Health::Failing(e.class()),
            (None, Some(head), Some(best)) if best.saturating_sub(head) > LAG_THRESHOLD => {
                Health::Lagging {
                    behind: best - head,
                }
            }
            _ => Health::Healthy,
        })
        .collect()
}

/// Indices best-first: healthy < lagging < failing; within a class by `p50`
/// (when `Some` for that index) else probe latency; ties by index.
pub fn rank(results: &[ProbeResult], p50: &[Option<Duration>]) -> Vec<usize> {
    let health = classify(results);
    let mut idx: Vec<usize> = (0..results.len()).collect();
    idx.sort_by_key(|&i| {
        let latency = p50.get(i).copied().flatten().unwrap_or(results[i].latency);
        (health[i].order(), latency, i)
    });
    idx
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
        assert!(classify(&[]).is_empty());
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
        assert_eq!(rank(&r, &[]), vec![3, 2, 4, 1, 0]);
        // p50 overrides probe latency where present; short p50 slice is fine.
        let p50 = [None, None, Some(Duration::from_millis(5))];
        assert_eq!(rank(&r, &p50), vec![2, 3, 4, 1, 0]);
    }
}
