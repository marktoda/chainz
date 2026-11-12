use crate::common::*;
use chainz_rpc::{Capabilities, RpcClient, Target, probe_capabilities};
use serde_json::json;

#[tokio::test]
async fn probes_each_capability() {
    let server = Mock::http(|_, req| {
        let params = req.json()["params"].clone();
        match req.method().as_str() {
            "eth_getBalance" => Reply::result(req, json!("0x0")),
            "debug_traceTransaction" => Reply::rpc_error(req, -32000, "transaction not found"),
            "trace_block" => Reply::rpc_error(req, -32601, "the method trace_block does not exist"),
            "eth_blockNumber" => Reply::result(req, json!("0x100000")),
            "eth_getLogs" => {
                let from = u64::from_str_radix(
                    params[0]["fromBlock"]
                        .as_str()
                        .unwrap()
                        .trim_start_matches("0x"),
                    16,
                )
                .unwrap();
                assert!(params[0]["topics"].is_array());
                if 0x100000 - from <= 10_000 {
                    Reply::result(req, json!([]))
                } else {
                    Reply::rpc_error(req, -32005, "query exceeds max block range")
                }
            }
            _ => Reply::rpc_error(req, -32601, "method not found"),
        }
    });
    let caps = probe_capabilities(&RpcClient::new(), &server.target(1)).await;
    assert_eq!(caps.archive, Some(true));
    assert_eq!(caps.debug_trace, Some(true));
    assert_eq!(caps.trace_block, Some(false));
    assert_eq!(caps.max_logs_range, Some(10_000));
    assert!(!caps.websocket);
    assert!(caps.probed_at.is_some());
    assert_eq!(caps.tags(), vec!["archive", "debug", "logs:10000"]);
}

#[tokio::test]
async fn pruned_node_and_websocket() {
    let server = Mock::http(|_, req| match req.method().as_str() {
        "eth_getBalance" => Reply::rpc_error(req, -32000, "missing trie node"),
        _ => Reply::rpc_error(req, -32601, "method not found"),
    });
    let caps = probe_capabilities(&RpcClient::new(), &server.target(1)).await;
    assert_eq!(caps.archive, Some(false));
    assert_eq!(caps.debug_trace, Some(false));
    assert_eq!(caps.max_logs_range, None);

    let ws = Target::new(1, "ws", "wss://example.invalid/x", &[]).unwrap();
    let caps = probe_capabilities(&RpcClient::new(), &ws).await;
    assert_eq!(
        caps,
        Capabilities {
            websocket: true,
            probed_at: caps.probed_at,
            ..Default::default()
        }
    );
    assert_eq!(caps.tags(), vec!["ws"]);
}

#[tokio::test]
async fn a_transient_logs_failure_leaves_the_range_unknown() {
    // A 503 (or a 429, or a timeout) on the largest range says nothing
    // about the limit: no smaller range may be cached as the answer.
    for status in [429, 503] {
        let server = Mock::http(move |_, req| match req.method().as_str() {
            "eth_blockNumber" => Reply::result(req, json!("0x100000")),
            "eth_getLogs" => Reply::status(status),
            _ => Reply::rpc_error(req, -32601, "method not found"),
        });
        let caps = probe_capabilities(&RpcClient::new(), &server.target(1)).await;
        assert_eq!(caps.max_logs_range, None, "HTTP {status}");
        let logs = server
            .requests()
            .iter()
            .filter(|r| r.method() == "eth_getLogs")
            .count();
        assert_eq!(logs, 1, "HTTP {status}: stepped down");
    }
}
