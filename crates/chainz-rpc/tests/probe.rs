mod common;

use chainz_rpc::{ErrorClass, Health, RpcClient, RpcError, classify, probe, probe_all, rank};
use common::*;
use serde_json::{Value, json};

/// A mock answering the probe batch with the given chain ID and head.
async fn node(chain_id: u64, head: u64) -> MockHttp {
    MockHttp::start(move |_, req| {
        let out: Vec<Value> = req
            .json()
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let v = match c["method"].as_str().unwrap() {
                    "eth_chainId" => format!("0x{chain_id:x}"),
                    _ => format!("0x{head:x}"),
                };
                json!({"jsonrpc":"2.0","id":c["id"],"result":v})
            })
            .collect();
        Resp::json(Value::Array(out))
    })
    .await
}

#[tokio::test]
async fn probe_ok_and_chain_mismatch() {
    let server = node(8453, 1000).await;
    let client = RpcClient::new();

    let r = probe(&client, &server.target(8453), 8453).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(r.head, Some(1000));
    assert_eq!(server.hits(), 1, "one batch request");

    let r = probe(&client, &server.target(8453), 1).await;
    assert!(!r.is_ok());
    assert_eq!(
        r.error,
        Some(RpcError::ChainMismatch {
            expected: 1,
            actual: 8453
        })
    );
    assert_eq!(r.head, None);
    assert_redacted(&r, "");
}

#[tokio::test]
async fn probe_all_yields_each_index_once_then_closes() {
    let a = node(1, 1000).await;
    let b = node(1, 950).await;
    let targets = vec![a.target(1), b.target(1), dead_target()];
    let mut rx = probe_all(&RpcClient::new(), targets, 1);
    let mut got = Vec::new();
    while let Some(item) = rx.recv().await {
        got.push(item);
    }
    got.sort_by_key(|(i, _)| *i);
    assert_eq!(
        got.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    let results: Vec<_> = got.into_iter().map(|(_, r)| r).collect();
    assert_eq!(
        classify(&results),
        vec![
            Health::Healthy,
            Health::Lagging { behind: 50 },
            Health::Failing(ErrorClass::Connect)
        ]
    );
    assert_eq!(rank(&results, &[]), vec![0, 1, 2]);

    let mut empty = probe_all(&RpcClient::new(), vec![], 1);
    assert!(empty.recv().await.is_none());
}
