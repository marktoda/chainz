//! Endpoint capability probing (archive, debug, trace, log range, websocket).

use serde_json::{Value, json};

use crate::client::{BlockId, RpcClient};
use crate::error::RpcError;
use crate::hex;
use crate::target::Target;

/// The `eth_getLogs` block ranges tried, largest first.
pub const LOG_RANGES: [u64; 5] = [100_000, 10_000, 2_000, 500, 100];

/// A topic that matches no real log, keeping `eth_getLogs` responses tiny.
const NO_MATCH_TOPIC: &str = "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";
const ZERO_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

/// What an endpoint supports. `None` means unknown (not probed, or the probe
/// was inconclusive because of a transport error).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    /// Serves historical state (`eth_getBalance` at block 1).
    pub archive: Option<bool>,
    /// Serves `debug_traceTransaction`.
    pub debug_trace: Option<bool>,
    /// Serves `trace_block`.
    pub trace_block: Option<bool>,
    /// The largest `eth_getLogs` block range accepted among [`LOG_RANGES`];
    /// `None` when unknown or when even the smallest range failed.
    pub max_logs_range: Option<u64>,
    /// The URL is `ws://` or `wss://`.
    pub websocket: bool,
    /// When these were probed, in unix seconds.
    pub probed_at: Option<u64>,
}

impl Capabilities {
    /// Display tags for the known-true capabilities, e.g.
    /// `["archive", "debug", "trace", "logs:10000", "ws"]`.
    pub fn tags(&self) -> Vec<String> {
        let mut tags = Vec::new();
        if self.archive == Some(true) {
            tags.push("archive".to_string());
        }
        if self.debug_trace == Some(true) {
            tags.push("debug".to_string());
        }
        if self.trace_block == Some(true) {
            tags.push("trace".to_string());
        }
        if let Some(range) = self.max_logs_range {
            tags.push(format!("logs:{range}"));
        }
        if self.websocket {
            tags.push("ws".to_string());
        }
        tags
    }
}

/// Probe what `target` supports.
///
/// - archive: `eth_getBalance(zero address, block 1)` succeeds;
/// - debug / trace: `debug_traceTransaction(zero hash)` / `trace_block(1)`
///   errors with something other than "method missing";
/// - max_logs_range: the largest of [`LOG_RANGES`] for which `eth_getLogs`
///   over `[head - range, head]` with a never-matching topic succeeds;
/// - websocket: [`Target::is_websocket`]. Websocket targets are otherwise not
///   probed (all other fields stay `None`).
pub async fn probe_capabilities(client: &RpcClient, target: &Target) -> Capabilities {
    let mut caps = Capabilities {
        websocket: target.is_websocket(),
        probed_at: Some(chainz_sys::now_secs()),
        ..Capabilities::default()
    };
    if caps.websocket {
        return caps;
    }
    let (archive, debug, trace, logs) = tokio::join!(
        async {
            match client
                .get_balance(target, ZERO_ADDRESS, BlockId::Number(1))
                .await
            {
                Ok(_) => Some(true),
                Err(e) => refusal(&e),
            }
        },
        method_supported(client, target, "debug_traceTransaction", json!([ZERO_HASH])),
        method_supported(client, target, "trace_block", json!([hex::quantity(1)])),
        max_logs_range(client, target),
    );
    caps.archive = archive;
    caps.debug_trace = debug;
    caps.trace_block = trace;
    caps.max_logs_range = logs;
    caps
}

/// `Some(false)` when the error is a definite refusal by the server,
/// `None` when it is a transport problem that says nothing about support.
fn refusal(e: &RpcError) -> Option<bool> {
    match e {
        RpcError::Rpc { .. } | RpcError::Http(400 | 403 | 404 | 405 | 501) => Some(false),
        _ => None,
    }
}

/// Whether an RPC error means "this method does not exist here".
fn is_method_missing(code: i64, message: &str) -> bool {
    if code == -32601 {
        return true;
    }
    let m = message.to_ascii_lowercase();
    m.contains("method")
        && [
            "not found",
            "not supported",
            "does not exist",
            "not available",
            "unsupported",
            "not allowed",
            "not enabled",
            "disabled",
            "not whitelisted",
        ]
        .iter()
        .any(|p| m.contains(p))
}

async fn method_supported(
    client: &RpcClient,
    target: &Target,
    method: &str,
    params: Value,
) -> Option<bool> {
    match client.request::<Value>(target, method, params).await {
        Ok(_) => Some(true),
        Err(RpcError::Rpc { code, message }) => Some(!is_method_missing(code, &message)),
        Err(e) => refusal(&e),
    }
}

async fn max_logs_range(client: &RpcClient, target: &Target) -> Option<u64> {
    let head = client.block_number(target).await.ok()?;
    for range in LOG_RANGES {
        let filter = json!({
            "fromBlock": hex::quantity(head.saturating_sub(range)),
            "toBlock": hex::quantity(head),
            "topics": [NO_MATCH_TOPIC],
        });
        match client
            .request::<Value>(target, "eth_getLogs", json!([filter]))
            .await
        {
            Ok(_) => return Some(range),
            // Only a refusal of the range says to try a smaller one. A
            // timeout, a 429 or a 5xx says nothing about the limit, and
            // a smaller range taken from one would be cached as the truth.
            Err(e) if refuses_range(&e) => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Whether an `eth_getLogs` failure is the server refusing the range: an
/// RPC error or a 4xx status (429 is [`RpcError::RateLimited`], not this).
fn refuses_range(e: &RpcError) -> bool {
    matches!(e, RpcError::Rpc { .. } | RpcError::Http(400..=499))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_only_known_true() {
        let caps = Capabilities {
            archive: Some(true),
            debug_trace: Some(false),
            trace_block: None,
            max_logs_range: Some(10_000),
            websocket: true,
            probed_at: Some(1),
        };
        assert_eq!(caps.tags(), vec!["archive", "logs:10000", "ws"]);
        assert!(Capabilities::default().tags().is_empty());
    }

    #[test]
    fn method_missing_rules() {
        assert!(is_method_missing(-32601, "whatever"));
        assert!(is_method_missing(
            -32602,
            "the method debug_x does not exist"
        ));
        assert!(!is_method_missing(-32000, "transaction not found"));
        assert!(!is_method_missing(-32602, "invalid argument 0"));
    }

    #[test]
    fn only_a_refusal_steps_the_log_range_down() {
        let rpc = RpcError::Rpc {
            code: -32005,
            message: "range too large".into(),
        };
        assert!(refuses_range(&rpc));
        assert!(refuses_range(&RpcError::Http(400)));
        assert!(refuses_range(&RpcError::Http(413)));
        for e in [
            RpcError::Timeout,
            RpcError::Connect,
            RpcError::RateLimited { retry_after: None },
            RpcError::Http(500),
            RpcError::Http(503),
            RpcError::InvalidResponse("x".into()),
        ] {
            assert!(!refuses_range(&e), "{e:?}");
        }
    }
}
