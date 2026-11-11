mod common;

use chainz_rpc::{Capabilities, RpcClient, Target, probe_capabilities};
use common::*;
use serde_json::json;

#[tokio::test]
async fn probes_each_capability() {
    let server = MockHttp::start(|_, req| {
        let params = req.json()["params"].clone();
        match req.method().as_str() {
            "eth_getBalance" => Resp::result(req, json!("0x0")),
            "debug_traceTransaction" => Resp::rpc_error(req, -32000, "transaction not found"),
            "trace_block" => Resp::rpc_error(req, -32601, "the method trace_block does not exist"),
            "eth_blockNumber" => Resp::result(req, json!("0x100000")),
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
                    Resp::result(req, json!([]))
                } else {
                    Resp::rpc_error(req, -32005, "query exceeds max block range")
                }
            }
            _ => Resp::rpc_error(req, -32601, "method not found"),
        }
    })
    .await;
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
    let server = MockHttp::start(|_, req| match req.method().as_str() {
        "eth_getBalance" => Resp::rpc_error(req, -32000, "missing trie node"),
        _ => Resp::rpc_error(req, -32601, "method not found"),
    })
    .await;
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
