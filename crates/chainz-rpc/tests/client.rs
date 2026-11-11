mod common;

use std::time::Duration;

use chainz_rpc::{BlockId, ErrorClass, RpcClient, RpcError, Target};
use common::*;
use serde_json::{Value, json};

async fn request_err(resp: Resp) -> RpcError {
    let server = MockHttp::start(move |_, _| resp.clone()).await;
    RpcClient::new()
        .request::<Value>(&server.target(1), "eth_chainId", json!([]))
        .await
        .unwrap_err()
}

#[tokio::test]
async fn classifies_each_error_kind() {
    let cases: Vec<(Resp, RpcError)> = vec![
        (
            Resp::status(429).header("Retry-After", "7"),
            RpcError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            },
        ),
        (
            Resp::status(429),
            RpcError::RateLimited { retry_after: None },
        ),
        (Resp::status(500), RpcError::Http(500)),
        (Resp::status(401), RpcError::Http(401)),
        (
            Resp::json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}})),
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
        let err = request_err(Resp::raw(200, body)).await;
        assert_eq!(err.class(), ErrorClass::Invalid, "{body}: {err:?}");
        assert_redacted(&err, &err.to_string());
    }
}

#[tokio::test]
async fn classifies_timeout_and_connect() {
    let server =
        MockHttp::start(|_, req| Resp::result(req, json!("0x1")).delay(Duration::from_secs(5)))
            .await;
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
    let elsewhere = MockHttp::start(|_, req| Resp::result(req, json!("0x1"))).await;
    for status in [301, 302, 307, 308] {
        let location = format!("http://localhost:{}/stolen", elsewhere.port);
        let server = MockHttp::start(move |_, _| Resp::status(status).header("Location", &location))
            .await;
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
    let server = MockHttp::start(|_, req| Resp::result(req, json!("0x2105"))).await;
    let id = RpcClient::new()
        .chain_id(&server.target(8453))
        .await
        .unwrap();
    assert_eq!(id, 8453);
    let req = &server.requests()[0];
    assert!(req.head.starts_with("POST /v3/"));
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
    let server = MockHttp::start(|_, req| {
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
        Resp::json(Value::Array(out))
    })
    .await;
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
        MockHttp::start(|_, _| Resp::json(json!([{"jsonrpc":"2.0","id":1,"result":"0x1"}]))).await;
    let results = RpcClient::new()
        .batch(&server.target(1), &[("a", json!([])), ("b", json!([]))])
        .await
        .unwrap();
    assert_eq!(
        results[0].as_ref().unwrap_err().class(),
        ErrorClass::Invalid
    );
    assert_eq!(results[1], Ok(json!("0x1")));

    let server = MockHttp::start(|_, _| {
        Resp::json(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"batch too large"}}),
        )
    })
    .await;
    let err = RpcClient::new()
        .batch(&server.target(1), &[("a", json!([]))])
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::Rpc { code: -32600, .. }));
}

#[tokio::test]
async fn typed_helpers() {
    let server = MockHttp::start(|_, req| {
        let params = req.json()["params"].clone();
        match req.method().as_str() {
            "eth_chainId" => Resp::result(req, json!("0x1")),
            "eth_blockNumber" => Resp::result(req, json!("0x10")),
            "eth_getBalance" => {
                assert_eq!(params[1], "0x5");
                Resp::result(req, json!("0xde0b6b3a7640000"))
            }
            "eth_call" => {
                assert_eq!(params[0]["data"], "0x70a08231");
                assert_eq!(params[1], "latest");
                Resp::result(req, json!("0x00ff"))
            }
            "eth_getBlockByNumber" => {
                if params[0] == "finalized" {
                    Resp::result(req, json!({"number":"0x1"}))
                } else {
                    Resp::result(req, Value::Null)
                }
            }
            "eth_feeHistory" => {
                assert_eq!(params, json!(["0x4", "latest", [25.0, 75.0]]));
                Resp::result(req, json!({"oldestBlock":"0x1"}))
            }
            _ => Resp::rpc_error(req, -32601, "method not found"),
        }
    })
    .await;
    let c = RpcClient::default();
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
        c.get_balance(
            &t,
            "0x0000000000000000000000000000000000000000",
            BlockId::Number(5)
        )
        .await
        .unwrap(),
        1_000_000_000_000_000_000
    );
    assert_eq!(
        c.call(&t, "0x00", &[0x70, 0xa0, 0x82, 0x31], BlockId::Latest)
            .await
            .unwrap(),
        vec![0x00, 0xff]
    );
    assert!(
        c.get_block(&t, BlockId::Finalized, false)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        c.get_block(&t, BlockId::Number(99), true)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        c.fee_history(&t, 4, BlockId::Latest, &[25.0, 75.0])
            .await
            .unwrap()["oldestBlock"],
        "0x1"
    );
    let err = c
        .request::<Value>(&t, "eth_foo", json!([]))
        .await
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Rpc);
}
