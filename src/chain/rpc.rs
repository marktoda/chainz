use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use anyhow::Result;
use std::time::{Duration, Instant};

/// Global deadline for a single RPC health probe in interactive sweeps.
pub const CHECK_DEADLINE: Duration = Duration::from_secs(4);

/// Test whether an (already-expanded) RPC URL serves the expected chain id.
/// No sweep deadline: explicit single-URL validation keeps the provider's
/// own 10s connect timeout. The single definition of "is this RPC healthy".
pub async fn check_url(rpc_url: &str, expected_chain_id: u64) -> Result<()> {
    let provider = create_provider(rpc_url).await?;
    let chain_id = provider
        .get_chain_id()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}: {}", rpc_url, e))?;
    if chain_id != expected_chain_id {
        anyhow::bail!(
            "Chain ID mismatch on {}: expected {}, got {}",
            rpc_url,
            expected_chain_id,
            chain_id
        );
    }
    Ok(())
}

/// One health probe under CHECK_DEADLINE, with measured latency.
pub async fn probe(url: &str, expected_chain_id: u64) -> (bool, Duration) {
    let start = Instant::now();
    let healthy = tokio::time::timeout(CHECK_DEADLINE, check_url(url, expected_chain_id))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
    (healthy, start.elapsed())
}

pub struct ProbeResult {
    pub index: usize,
    pub healthy: bool,
    pub latency: Duration,
}

/// Probe URLs concurrently, yielding each result as it lands (completion
/// order). The receiver yields exactly `urls.len()` results, then closes.
pub fn probe_urls(
    urls: &[String],
    expected_chain_id: u64,
) -> tokio::sync::mpsc::Receiver<ProbeResult> {
    let (tx, rx) = tokio::sync::mpsc::channel(urls.len().max(1));
    for (index, url) in urls.iter().cloned().enumerate() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let (healthy, latency) = probe(&url, expected_chain_id).await;
            let _ = tx
                .send(ProbeResult {
                    index,
                    healthy,
                    latency,
                })
                .await;
        });
    }
    rx
}

/// Picker ordering: healthy probes fastest-first, then unhealthy ones in
/// their original order.
pub fn rank_by_health(results: &[ProbeResult]) -> Vec<usize> {
    let mut healthy: Vec<&ProbeResult> = results.iter().filter(|r| r.healthy).collect();
    healthy.sort_by_key(|r| r.latency);
    let mut unhealthy: Vec<&ProbeResult> = results.iter().filter(|r| !r.healthy).collect();
    unhealthy.sort_by_key(|r| r.index);
    healthy
        .into_iter()
        .chain(unhealthy)
        .map(|r| r.index)
        .collect()
}

/// Collecting wrapper: one health flag per input URL, in input order.
pub async fn check_urls(urls: &[String], expected_chain_id: u64) -> Vec<bool> {
    let mut results = vec![false; urls.len()];
    let mut rx = probe_urls(urls, expected_chain_id);
    while let Some(result) = rx.recv().await {
        results[result.index] = result.healthy;
    }
    results
}

async fn create_provider(rpc_url: &str) -> Result<DynProvider> {
    let provider = tokio::time::timeout(
        Duration::from_secs(10),
        ProviderBuilder::new().connect(rpc_url),
    )
    .await
    .map_err(|_| anyhow::anyhow!("RPC connection timed out: {}", rpc_url))??;
    Ok(provider.erased())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn result(index: usize, healthy: bool, ms: u64) -> ProbeResult {
        ProbeResult {
            index,
            healthy,
            latency: Duration::from_millis(ms),
        }
    }

    #[test]
    fn rank_healthy_fastest_first_then_unhealthy_in_order() {
        let results = vec![
            result(0, false, 4000),
            result(1, true, 150),
            result(2, true, 20),
            result(3, false, 4000),
        ];
        assert_eq!(rank_by_health(&results), vec![2, 1, 0, 3]);
    }

    #[tokio::test]
    async fn probe_urls_reports_every_url() {
        // connection-refused fails fast; no network needed
        let urls = vec![
            "http://localhost:1".to_string(),
            "http://localhost:2".to_string(),
        ];
        let mut rx = probe_urls(&urls, 1);
        let mut seen = Vec::new();
        while let Some(result) = rx.recv().await {
            assert!(!result.healthy);
            seen.push(result.index);
        }
        seen.sort();
        assert_eq!(seen, vec![0, 1]);
    }

    #[tokio::test]
    async fn probe_urls_empty_input_closes_immediately() {
        let mut rx = probe_urls(&[], 1);
        assert!(rx.recv().await.is_none());
    }
}
