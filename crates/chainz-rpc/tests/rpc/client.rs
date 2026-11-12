
use std::time::Duration;

use chainz_rpc::{BlockId, ErrorClass, RpcClient, RpcError, Target};
use crate::common::*;
use serde_json::{Value, json};

async fn request_err(resp: Reply) -> RpcError {
    let server = Mock::http(move |_, _| resp.clone());
    RpcClient::new()
        .request::<Value>(&server.target(1), "eth_chainId", json!([]))
        .await
        .unwrap_err()
}

#[tokio::test]
async fn classifies_each_error_kind() {
    let cases: Vec<(Reply, RpcError)> = vec![
        (
            Reply::status(429).header("Retry-After", "7"),
            RpcError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            },
        ),
        (
            Reply::status(429),
            RpcError::RateLimited { retry_after: None },
        ),
        (Reply::status(500), RpcError::Http(500)),
        (Reply::status(401), RpcError::Http(401)),
        (
            Reply::json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}})),
            RpcError::Rpc {
                code: -32000,
                message: "boom".into(),
            },
        ),
    ];
    for (resp, expected) in cases {
        let err = request_err(resp).await;
        assert_eq!(err, expected);
        assert_redacted(&err, &err.to_string());
    }

    for body in ["not json", "[]", r#"{"jsonrpc":"2.0","id":1}"#] {
        let err = request_err(Reply::raw(200, body)).await;
        assert_eq!(err.class(), ErrorClass::Invalid, "{body}: {err:?}");
        assert_redacted(&err, &err.to_string());
    }
}

#[tokio::test]
async fn classifies_timeout_and_connect() {
    let server =
        Mock::http(|_, req| Reply::result(req, json!("0x1")).delay(Duration::from_secs(5)));
    let err = RpcClient::with_timeout(Duration::from_millis(200))
        .chain_id(&server.target(1))
        .await
        .unwrap_err();
    assert_eq!(err, RpcError::Timeout);

    let err = RpcClient::new().chain_id(&dead_target()).await.unwrap_err();
    assert_eq!(err, RpcError::Connect);
    assert_redacted(&err, &err.to_string());
}

#[tokio::test]
async fn redirects_are_not_followed() {
    // Following a redirect would hand the target's API-key header to
    // whichever host the redirect names.
    let elsewhere = Mock::http(|_, req| Reply::result(req, json!("0x1")));
    for status in [301, 302, 307, 308] {
        let location = format!("http://localhost:{}/stolen", elsewhere.port);
        let server =
            Mock::http(move |_, _| Reply::status(status).header("Location", &location));
        let err = RpcClient::new()
            .chain_id(&server.target(1))
            .await
            .unwrap_err();
        assert_eq!(err, RpcError::Http(status));
        assert_eq!(server.hits(), 1);
    }
    assert_eq!(elsewhere.hits(), 0, "a redirect was followed");
}

#[tokio::test]
async fn websocket_targets_are_unsupported() {
    let t = Target::new(1, "ws", &format!("wss://example.invalid/{URL_SECRET}"), &[]).unwrap();
    let err = RpcClient::new().block_number(&t).await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Unsupported);
    assert_redacted(&err, &err.to_string());
}

#[tokio::test]
async fn sends_headers_and_jsonrpc_envelope() {
    let server = Mock::http(|_, req| Reply::result(req, json!("0x2105")));
    let id = RpcClient::new()
        .chain_id(&server.target(8453))
        .await
        .unwrap();
    assert_eq!(id, 8453);
    let req = &server.requests()[0];
    assert!(req.head.starts_with("post /v3/"));
    assert_eq!(req.header("x-api-key").as_deref(), Some(HEADER_SECRET));
    assert_eq!(
        req.header("content-type").as_deref(),
        Some("application/json")
    );
    let body = req.json();
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["method"], "eth_chainId");
}

#[tokio::test]
async fn batch_matches_results_by_id() {
    // Answer in reverse order, with an error for the middle call.
    let server = Mock::http(|_, req| {
        let calls = req.json();
        let mut out: Vec<Value> = calls
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let id = c["id"].clone();
                match c["method"].as_str().unwrap() {
                    "fail" => {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"nope"}})
                    }
                    m => json!({"jsonrpc":"2.0","id":id,"result":m}),
                }
            })
            .collect();
        out.reverse();
        Reply::json(Value::Array(out))
    });
    let results = RpcClient::new()
        .batch(
            &server.target(1),
            &[("a", json!([])), ("fail", json!([])), ("c", json!([]))],
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0], Ok(json!("a")));
    assert_eq!(
        results[1],
        Err(RpcError::Rpc {
            code: -32601,
            message: "nope".into()
        })
    );
    assert_eq!(results[2], Ok(json!("c")));
}

#[tokio::test]
async fn batch_missing_id_and_whole_batch_error() {
    let server =
        Mock::http(|_, _| Reply::json(json!([{"jsonrpc":"2.0","id":1,"result":"0x1"}])));
    let results = RpcClient::new()
        .batch(&server.target(1), &[("a", json!([])), ("b", json!([]))])
        .await
        .unwrap();
    assert_eq!(
        results[0].as_ref().unwrap_err().class(),
        ErrorClass::Invalid
    );
    assert_eq!(results[1], Ok(json!("0x1")));

    let server = Mock::http(|_, _| {
        Reply::json(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"batch too large"}}),
        )
    });
    let err = RpcClient::new()
        .batch(&server.target(1), &[("a", json!([]))])
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::Rpc { code: -32600, .. }));
}

#[tokio::test]
async fn typed_helpers() {
    let server = Mock::http(|_, req| {
        let params = req.json()["params"].clone();
        match req.method().as_str() {
            "eth_chainId" => Reply::result(req, json!("0x1")),
            "eth_blockNumber" => Reply::result(req, json!("0x10")),
            "eth_getBalance" => {
                assert_eq!(params[1], "0x5");
                Reply::result(req, json!("0xde0b6b3a7640000"))
            }
            "eth_call" => {
                assert_eq!(params[0]["data"], "0x70a08231");
                assert_eq!(params[1], "latest");
                Reply::result(req, json!("0x00ff"))
            }
            "eth_getBlockByNumber" => {
                if params[0] == "finalized" {
                    Reply::result(req, json!({"number":"0x1"}))
                } else {
                    Reply::result(req, Value::Null)
                }
            }
            _ => Reply::rpc_error(req, -32601, "method not found"),
        }
    });
    let c = RpcClient::new();
    let t = server.target(1);
    c.check_chain_id(&t, 1).await.unwrap();
    assert_eq!(
        c.check_chain_id(&t, 10).await.unwrap_err(),
        RpcError::ChainMismatch {
            expected: 10,
            actual: 1
        }
    );
    assert_eq!(c.block_number(&t).await.unwrap(), 16);
    assert_eq!(
        c.call(&t, "0x00", &[0x70, 0xa0, 0x82, 0x31], BlockId::Latest)
            .await
            .unwrap(),
        vec![0x00, 0xff]
    );
    assert!(c.get_block(&t, BlockId::Finalized).await.unwrap().is_some());
    assert!(
        c.get_block(&t, BlockId::Number(99))
            .await
            .unwrap()
            .is_none()
    );
    let err = c
        .request::<Value>(&t, "eth_foo", json!([]))
        .await
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Rpc);
}
