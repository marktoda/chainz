//! The control client against servers that misbehave. The real server, and
//! the client against it, are tested in `chainz-serve` (`control.rs`).

use std::collections::BTreeMap;

use chainz_rpc::ErrorClass;
use chainz_rpc::control::*;

#[test]
fn record_status_serde_shape() {
    let status = RecordStatus::Error {
        class: ErrorClass::Rpc,
        code: Some(-32000),
    };
    assert_eq!(
        serde_json::to_value(status).unwrap(),
        serde_json::json!({"kind":"error","class":"rpc","code":-32000})
    );
    assert_eq!(
        serde_json::to_value(RecordStatus::Ok).unwrap(),
        serde_json::json!({"kind":"ok"})
    );
    let row = MetricsRow {
        chain: "base".into(),
        endpoint: "infura".into(),
        method: "eth_call".into(),
        requests: 1,
        errors: BTreeMap::from([(ErrorClass::Timeout, 2), (ErrorClass::RateLimited, 1)]),
        p50_ms: 40,
        p95_ms: 90,
        rate_limited: 1,
        failovers: 0,
        bytes: 1024,
    };
    let m = serde_json::to_string(&row).unwrap();
    assert!(
        m.contains(r#""errors":{"timeout":2,"rate_limited":1}"#),
        "{m}"
    );
}

#[tokio::test]
async fn an_error_answer_carries_its_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = chainz_testkit::short_tempdir();
    let socket = dir.path().join("serve.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let serve = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf).await;
        let body = "config.toml: unknown chain `nope`\n";
        let answer = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(answer.as_bytes()).await.unwrap();
    });
    let error = ControlClient::new(&socket).reload().await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "control API POST /_chainz/reload returned HTTP 500: config.toml: unknown chain `nope`"
    );
    serve.await.unwrap();
}
