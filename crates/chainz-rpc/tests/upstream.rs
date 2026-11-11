mod common;

use std::time::{Duration, Instant};

use bytes::Bytes;
use chainz_rpc::upstream::{HttpUpstream, RetryOn, RetryPolicy, Upstream, rpc_error_code};
use chainz_rpc::{RpcClient, RpcError};
use common::*;
use serde_json::json;

const BODY: &str = r#"{"jsonrpc":"2.0","id":7,"method":"eth_blockNumber","params":[]}"#;

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(20),
        ..RetryPolicy::same_endpoint_transient()
    }
}

async fn send(
    server: &MockHttp,
    policy: &RetryPolicy,
) -> Result<chainz_rpc::UpstreamResponse, RpcError> {
    let up = HttpUpstream::new(RpcClient::new());
    up.send(
        &server.target(1),
        Bytes::from_static(BODY.as_bytes()),
        policy,
    )
    .await
}

fn ok(req: &Req) -> Resp {
    Resp::result(req, json!("0x10"))
}

#[tokio::test]
async fn forwards_body_and_headers_verbatim() {
    let server = MockHttp::start(|_, req| ok(req)).await;
    let r = send(&server, &RetryPolicy::none()).await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.attempts, 1);
    let req = &server.requests()[0];
    assert_eq!(req.body, BODY.as_bytes());
    assert_eq!(req.header("x-api-key").as_deref(), Some(HEADER_SECRET));
    assert_eq!(
        req.header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["id"],
        7
    );
}

#[tokio::test]
async fn short_429_is_waited_out_on_same_target() {
    let server = MockHttp::start(|i, req| {
        if i == 0 {
            Resp::status(429).header("Retry-After", "1")
        } else {
            ok(req)
        }
    })
    .await;
    let start = Instant::now();
    let r = send(&server, &fast_policy()).await.unwrap();
    assert!(start.elapsed() >= Duration::from_secs(1));
    assert_eq!(r.attempts, 2);
    assert_eq!(r.retry_after, Some(Duration::from_secs(1)));
    assert_eq!(server.hits(), 2);
}

#[tokio::test]
async fn missing_retry_after_uses_backoff() {
    let server = MockHttp::start(|i, req| if i == 0 { Resp::status(429) } else { ok(req) }).await;
    let r = send(&server, &fast_policy()).await.unwrap();
    assert_eq!(r.attempts, 2);
    assert_eq!(r.retry_after, None);
}

#[tokio::test]
async fn long_429_returns_rate_limited_without_waiting() {
    let server = MockHttp::start(|_, _| Resp::status(429).header("Retry-After", "30")).await;
    let start = Instant::now();
    let err = send(&server, &fast_policy()).await.unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(
        err,
        RpcError::RateLimited {
            retry_after: Some(Duration::from_secs(30))
        }
    );
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn gateway_status_is_retried_then_exhausted() {
    let server = MockHttp::start(|i, req| if i < 2 { Resp::status(503) } else { ok(req) }).await;
    let r = send(&server, &fast_policy()).await.unwrap();
    assert_eq!(r.attempts, 3);

    let server = MockHttp::start(|_, _| Resp::status(502)).await;
    let err = send(&server, &fast_policy()).await.unwrap_err();
    assert_eq!(err, RpcError::Http(502));
    assert_eq!(server.hits(), 3);
}

#[tokio::test]
async fn limit_exceeded_is_retried_only_when_enabled() {
    let server = MockHttp::start(|i, req| {
        if i == 0 {
            Resp::rpc_error(req, -32005, "limit exceeded")
        } else {
            ok(req)
        }
    })
    .await;
    let r = send(&server, &fast_policy()).await.unwrap();
    assert_eq!(r.attempts, 2);
    assert_eq!(rpc_error_code(&r.body), None);

    // Disabled: the -32005 body is returned as Ok.
    let server = MockHttp::start(|_, req| Resp::rpc_error(req, -32005, "limit exceeded")).await;
    let policy = RetryPolicy {
        retry_on: RetryOn {
            limit_exceeded: false,
            ..RetryOn::ALL
        },
        ..fast_policy()
    };
    let r = send(&server, &policy).await.unwrap();
    assert_eq!(r.attempts, 1);
    assert_eq!(rpc_error_code(&r.body), Some(-32005));

    // Exhausted: the last -32005 body is returned as Ok.
    let server = MockHttp::start(|_, req| Resp::rpc_error(req, -32005, "limit exceeded")).await;
    let r = send(&server, &fast_policy()).await.unwrap();
    assert_eq!(r.attempts, 3);
    assert_eq!(rpc_error_code(&r.body), Some(-32005));
}

#[tokio::test]
async fn non_retryable_status_fails_after_one_attempt() {
    let server = MockHttp::start(|_, _| Resp::status(400)).await;
    let err = send(&server, &fast_policy()).await.unwrap_err();
    assert_eq!(err, RpcError::Http(400));
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn policy_none_makes_a_single_attempt() {
    for resp in [
        Resp::status(503),
        Resp::status(429).header("Retry-After", "0"),
    ] {
        let server = MockHttp::start(move |_, _| resp.clone()).await;
        let err = send(&server, &RetryPolicy::none()).await.unwrap_err();
        assert!(matches!(
            err,
            RpcError::Http(503) | RpcError::RateLimited { .. }
        ));
        assert_eq!(server.hits(), 1);
    }
}

#[tokio::test]
async fn timeouts_and_connect_errors_are_retried() {
    let server = MockHttp::start(|i, req| {
        if i == 0 {
            ok(req).delay(Duration::from_secs(5))
        } else {
            ok(req)
        }
    })
    .await;
    let policy = RetryPolicy {
        timeout: Duration::from_millis(200),
        ..fast_policy()
    };
    let r = send(&server, &policy).await.unwrap();
    assert_eq!(r.attempts, 2);

    let up = HttpUpstream::new(RpcClient::new());
    let start = Instant::now();
    let err = up
        .send(
            &dead_target(),
            Bytes::from_static(BODY.as_bytes()),
            &fast_policy(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, RpcError::Connect);
    assert!(
        start.elapsed() >= Duration::from_millis(15),
        "backed off twice"
    );
    assert_redacted(&err, &err.to_string());
}

#[tokio::test]
async fn upstream_is_object_safe() {
    let server = MockHttp::start(|_, req| ok(req)).await;
    let up: Box<dyn Upstream> = Box::new(HttpUpstream::new(RpcClient::new()));
    let policy = RetryPolicy::none();
    let r = up
        .send(
            &server.target(1),
            Bytes::from_static(BODY.as_bytes()),
            &policy,
        )
        .await
        .unwrap();
    assert_eq!(r.status, 200);
}
