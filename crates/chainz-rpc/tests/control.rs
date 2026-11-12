//! The control client against servers that misbehave. The real server, and
//! the client against it, are tested in `chainz-serve` (`control.rs`).

use std::collections::BTreeMap;

use chainz_rpc::ErrorClass;
use chainz_rpc::control::*;

/// A temp dir short enough for a unix socket path (SUN_LEN is ~104-108
/// bytes; a deep `$TMPDIR` would overflow it).
fn socket_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    if dir.path().join("serve.sock").as_os_str().len() < 100 {
        return dir;
    }
    tempfile::Builder::new()
        .prefix("cz")
        .tempdir_in("/tmp")
        .unwrap()
}

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
    let dir = socket_dir();
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

/// An older (2.0) gateway ignores `after`: the client drops what it didn't
/// ask for.
#[tokio::test]
async fn after_id_is_honoured_by_the_client_too() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = socket_dir();
    let socket = dir.path().join("serve.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let serve = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf).await;
        let mut body = Vec::new();
        for id in 1..=4 {
            let record = RequestRecord {
                id,
                at_ms: id,
                chain: "eth".into(),
                method: "eth_call".into(),
                params_summary: String::new(),
                upstream: "a".into(),
                attempts: 1,
                latency_ms: 1,
                status: RecordStatus::Ok,
                response_bytes: 0,
            };
            body.extend(write_line(&record).unwrap());
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
    });
    let mut events = ControlClient::new(&socket)
        .events(EventsOptions {
            backlog: Backlog::All,
            after_id: Some(2),
            follow: false,
        })
        .await
        .unwrap();
    let mut ids = Vec::new();
    while let Some(record) = events.recv().await {
        ids.push(record.id);
    }
    assert_eq!(ids, [3, 4]);
    serve.await.unwrap();
}
