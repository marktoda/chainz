#![cfg(unix)]

use std::collections::BTreeMap;
use std::time::Duration;

use chainz_rpc::ErrorClass;
use chainz_rpc::control::*;

fn status() -> Status {
    Status {
        version: "2.0.0".into(),
        pid: 42,
        port: 18545,
        uptime_secs: 10,
        chains: vec![ChainStatus {
            chain: "base".into(),
            chain_id: 8453,
            active: "infura".into(),
            failover: vec!["llama".into()],
            head: Some(123),
        }],
    }
}

fn metrics(requests: u64) -> MetricsSnapshot {
    MetricsSnapshot {
        at_ms: 1,
        windows: vec![MetricsWindow {
            window_secs: 60,
            rows: vec![MetricsRow {
                chain: "base".into(),
                endpoint: "infura".into(),
                method: "eth_call".into(),
                requests,
                errors: BTreeMap::from([(ErrorClass::Timeout, 2), (ErrorClass::RateLimited, 1)]),
                p50_ms: 40,
                p95_ms: 90,
                rate_limited: 1,
                failovers: 0,
                bytes: 1024,
            }],
        }],
    }
}

fn record(id: u64, ok: bool) -> RequestRecord {
    RequestRecord {
        id,
        at_ms: 1000 + id,
        chain: "base".into(),
        method: "eth_call".into(),
        params_summary: "@usdc.balanceOf".into(),
        upstream: "infura".into(),
        attempts: 1,
        latency_ms: 12,
        status: if ok {
            RecordStatus::Ok
        } else {
            RecordStatus::Error {
                class: ErrorClass::Rpc,
                code: Some(-32000),
            }
        },
        response_bytes: 64,
    }
}

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

async fn server(dir: &tempfile::TempDir) -> MockControlServer {
    MockControlServer::start(&dir.path().join("serve.sock"), status(), metrics(5))
        .await
        .unwrap()
}

#[tokio::test]
async fn status_and_metrics() {
    let dir = socket_dir();
    let server = server(&dir).await;
    let client = ControlClient::new(server.path());
    assert_eq!(client.status().await.unwrap(), status());
    assert_eq!(client.metrics().await.unwrap(), metrics(5));
    server.set_metrics(metrics(9));
    assert_eq!(client.metrics().await.unwrap(), metrics(9));
}

#[tokio::test]
async fn socket_mode_and_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    let dir = socket_dir();
    let server = server(&dir).await;
    let path = server.path().to_path_buf();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    drop(server);
    assert!(!path.exists());
    assert!(ControlClient::new(&path).status().await.is_err());
}

#[tokio::test]
async fn reload_counter() {
    let dir = socket_dir();
    let server = server(&dir).await;
    let client = ControlClient::new(server.path());
    assert_eq!(server.reload_count(), 0);
    client.reload().await.unwrap();
    client.reload().await.unwrap();
    assert_eq!(server.reload_count(), 2);
}

#[tokio::test]
async fn events_stream_to_every_client() {
    let dir = socket_dir();
    let server = server(&dir).await;
    let client = ControlClient::new(server.path());
    let mut a = client.events().await.unwrap();
    let mut b = client.events().await.unwrap();
    assert_eq!(server.event_subscribers(), 2);
    for i in 0..3 {
        server.push_event(record(i, i != 1));
    }
    for rx in [&mut a, &mut b] {
        for i in 0..3 {
            let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(got, record(i, i != 1));
        }
    }
    // Dropping a receiver closes its connection.
    drop(a);
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.event_subscribers() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Stopping the server closes the stream.
    drop(server);
    let end = tokio::time::timeout(Duration::from_secs(5), b.recv())
        .await
        .unwrap();
    assert!(end.is_none());
}

#[tokio::test]
async fn unknown_path_is_404() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = socket_dir();
    let server = server(&dir).await;
    let mut s = tokio::net::UnixStream::connect(server.path())
        .await
        .unwrap();
    s.write_all(b"GET /nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 404"), "{out}");
}

#[test]
fn record_status_serde_shape() {
    let v = serde_json::to_value(record(1, false).status).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"kind":"error","class":"rpc","code":-32000})
    );
    assert_eq!(
        serde_json::to_value(RecordStatus::Ok).unwrap(),
        serde_json::json!({"kind":"ok"})
    );
    let m = serde_json::to_string(&metrics(1)).unwrap();
    assert!(
        m.contains(r#""errors":{"timeout":2,"rate_limited":1}"#),
        "{m}"
    );
}

#[tokio::test]
async fn events_backlog_and_follow() {
    let dir = socket_dir();
    let server = server(&dir).await;
    let client = ControlClient::new(server.path());
    for i in 0..5 {
        server.push_event(record(i, true));
    }
    // The last two buffered records, then the end of the stream.
    let mut replay = client
        .events_with(EventsOptions {
            backlog: 2,
            follow: false,
        })
        .await
        .unwrap();
    assert_eq!(replay.recv().await.unwrap().id, 3);
    assert_eq!(replay.recv().await.unwrap().id, 4);
    assert!(replay.recv().await.is_none());
    assert_eq!(server.event_subscribers(), 0);

    // Backlog, then live records, with nothing lost in between.
    let mut tail = client
        .events_with(EventsOptions {
            backlog: 100,
            follow: true,
        })
        .await
        .unwrap();
    server.push_event(record(5, true));
    for i in 0..6 {
        let got = tokio::time::timeout(Duration::from_secs(5), tail.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.id, i);
    }

    // The default is live records only.
    assert_eq!(EventsOptions::default().path(), EVENTS_PATH);
    let options = EventsOptions {
        backlog: 7,
        follow: false,
    };
    assert_eq!(options.path(), "/_chainz/events?backlog=7&follow=false");
    assert_eq!(
        EventsOptions::from_query("backlog=7&follow=false&x=1"),
        options
    );
    assert_eq!(EventsOptions::from_query(""), EventsOptions::default());
}

#[tokio::test]
async fn long_socket_paths_work() {
    // Deeper than sockaddr_un allows (about 107 bytes).
    let dir = socket_dir();
    let deep = dir.path().join("d".repeat(60)).join("e".repeat(60));
    std::fs::create_dir_all(&deep).unwrap();
    let socket = deep.join("serve.sock");
    assert!(socket.as_os_str().len() > 108);
    if !cfg!(target_os = "linux") {
        return;
    }
    let server = MockControlServer::start(&socket, status(), metrics(1))
        .await
        .unwrap();
    assert!(socket.exists());
    let client = ControlClient::new(&socket);
    assert_eq!(client.status().await.unwrap(), status());
    drop(server);
    assert!(!socket.exists());
}

#[tokio::test]
async fn a_wedged_gateway_times_out() {
    // Accepts connections, reads the request, never answers.
    let dir = socket_dir();
    let socket = dir.path().join("serve.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let hold = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            open.push(stream);
        }
    });
    let client = ControlClient::new(&socket).with_timeout(Duration::from_millis(200));
    let started = std::time::Instant::now();
    let error = client.status().await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
    assert_eq!(
        client.reload().await.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert_eq!(
        client.events().await.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    hold.abort();
    let _ = hold.await;
}
