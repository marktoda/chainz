//! Health probes: `eth_chainId` + `eth_blockNumber` in one batch.

use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::mpsc;

use crate::client::{RpcClient, expect_chain};
use crate::error::RpcError;
use crate::hex;
use crate::target::Target;

/// The deadline for one whole probe.
const PROBE_DEADLINE: Duration = Duration::from_secs(4);

/// The outcome of one probe.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// Wall time of the probe (up to the deadline on timeout).
    pub latency: Duration,
    /// The reported head block; `None` whenever the probe failed.
    pub head: Option<u64>,
    /// Why the probe failed, if it did.
    pub error: Option<RpcError>,
}

impl ProbeResult {
    /// Whether the probe succeeded.
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// One batch `[eth_chainId, eth_blockNumber]` under [`PROBE_DEADLINE`]; a
/// chain-ID mismatch is an error.
pub async fn probe(client: &RpcClient, target: &Target, expected_chain_id: u64) -> ProbeResult {
    let start = Instant::now();
    let outcome = tokio::time::timeout(PROBE_DEADLINE, run(client, target, expected_chain_id))
        .await
        .unwrap_or(Err(RpcError::Timeout));
    let latency = start.elapsed();
    match outcome {
        Ok(head) => ProbeResult {
            latency,
            head: Some(head),
            error: None,
        },
        Err(e) => ProbeResult {
            latency,
            head: None,
            error: Some(e),
        },
    }
}

async fn run(client: &RpcClient, target: &Target, expected: u64) -> Result<u64, RpcError> {
    let mut results = client
        .batch(
            target,
            &[("eth_chainId", json!([])), ("eth_blockNumber", json!([]))],
        )
        .await?
        .into_iter();
    let (Some(chain), Some(head)) = (results.next(), results.next()) else {
        return Err(RpcError::InvalidResponse("short batch response".into()));
    };
    expect_chain(expected, hex::value_u64(&chain?)?)?;
    hex::value_u64(&head?)
}

/// Probe every target concurrently; yields `(index, result)` in completion
/// order, exactly `targets.len()` items, then closes.
///
/// Must be called from within a tokio runtime.
pub fn probe_all(
    client: &RpcClient,
    targets: Vec<Target>,
    expected_chain_id: u64,
) -> mpsc::Receiver<(usize, ProbeResult)> {
    let (tx, rx) = mpsc::channel(targets.len().max(1));
    for (i, target) in targets.into_iter().enumerate() {
        let tx = tx.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let result = probe(&client, &target, expected_chain_id).await;
            let _ = tx.send((i, result)).await;
        });
    }
    rx
}
