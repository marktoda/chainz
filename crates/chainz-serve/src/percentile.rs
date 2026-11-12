//! The percentile behind every latency figure the gateway reports (also
//! used by the latency example, so its numbers read like `serve status`).

/// Nearest-rank percentile of `samples` (sorted in place); the default
/// (0) for no samples.
pub(crate) fn percentile<T: Ord + Copy + Default>(samples: &mut [T], pct: usize) -> T {
    if samples.is_empty() {
        return T::default();
    }
    samples.sort_unstable();
    let rank = (samples.len() * pct).div_ceil(100).max(1);
    samples[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        assert_eq!(percentile::<u32>(&mut [], 50), 0);
        assert_eq!(percentile(&mut [7], 95), 7);
        assert_eq!(percentile(&mut [4, 1, 3, 2], 50), 2);
        assert_eq!(percentile(&mut [4, 1, 3, 2], 95), 4);
    }
}
