//! The control API's server side: the one handler for every control
//! request. The gateway serves it over its live state; the `testing`
//! feature serves it over canned state (`testing::ControlServer`). The
//! protocol itself is [`chainz_rpc::control`]; the records behind
//! `/_chainz/events` are kept in [`crate::events`].

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use bytes::Bytes;
use chainz_rpc::control::{
    ControlRoute, EventsOptions, MetricsSnapshot, RequestRecord, Status, write_line,
};
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, Incoming};
use hyper::header::{ALLOW, CONTENT_TYPE, HeaderValue};
use hyper::{Request, Response, StatusCode};
use tokio::sync::{broadcast, watch};

use crate::events::EventRing;
use crate::http::{BoxBody, full, json, plain};

/// What the control API serves.
pub(crate) trait ControlState: Send + Sync + 'static {
    /// `/_chainz/status`.
    fn status(&self) -> Status;
    /// `/_chainz/metrics`.
    fn metrics(&self) -> MetricsSnapshot;
    /// The records behind `/_chainz/events`.
    fn events(&self) -> &EventRing;
    /// `/_chainz/reload`: re-read the config; on error the old one stays.
    fn reload(&self) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// One control request. `stopped` ends the live part of events streams.
pub(crate) async fn handle<S: ControlState>(
    state: &S,
    request: Request<Incoming>,
    stopped: watch::Receiver<bool>,
) -> Response<BoxBody> {
    let Some(route) = ControlRoute::from_path(request.uri().path()) else {
        return plain(StatusCode::NOT_FOUND, "");
    };
    if request.method().as_str() != route.method() {
        let mut response = plain(StatusCode::METHOD_NOT_ALLOWED, "");
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static(route.method()));
        return response;
    }
    match route {
        ControlRoute::Status => json_reply(&state.status()),
        ControlRoute::Metrics => json_reply(&state.metrics()),
        ControlRoute::Reload => match state.reload().await {
            Ok(()) => plain(StatusCode::NO_CONTENT, ""),
            Err(error) => {
                let mut response = Response::new(full(format!("{error:#}\n")));
                *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                response
            }
        },
        ControlRoute::Events => {
            let options = match EventsOptions::from_query(request.uri().query().unwrap_or("")) {
                Ok(options) => options,
                Err(error) => {
                    let mut response = Response::new(full(format!("{error}\n")));
                    *response.status_mut() = StatusCode::BAD_REQUEST;
                    return response;
                }
            };
            let (backlog, live) = state.events().subscribe(options);
            let body = EventsBody::new(backlog, live, options, stopped);
            let mut response = Response::new(body.boxed_unsync());
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson"),
            );
            response
        }
    }
}

fn json_reply<T: serde::Serialize>(value: &T) -> Response<BoxBody> {
    match serde_json::to_vec(value) {
        Ok(body) => json(StatusCode::OK, body),
        Err(_) => plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            "can't encode the answer\n",
        ),
    }
}

/// The NDJSON events stream: the backlog in one chunk, then live records
/// until the client goes away or the server stops. The body drives
/// itself, so it ends with its connection and leaves no task behind.
struct EventsBody {
    backlog: Option<Bytes>,
    next: Option<NextLine>,
}

type NextLine = Pin<Box<dyn Future<Output = Option<(Bytes, Live)>> + Send>>;

/// What a following events stream waits on.
struct Live {
    records: broadcast::Receiver<RequestRecord>,
    options: EventsOptions,
    stopped: watch::Receiver<bool>,
}

impl EventsBody {
    fn new(
        backlog: Vec<RequestRecord>,
        live: Option<broadcast::Receiver<RequestRecord>>,
        options: EventsOptions,
        stopped: watch::Receiver<bool>,
    ) -> Self {
        let chunk: Vec<u8> = backlog
            .iter()
            .filter_map(|record| write_line(record).ok())
            .flatten()
            .collect();
        EventsBody {
            backlog: (!chunk.is_empty()).then(|| Bytes::from(chunk)),
            next: live.map(|records| {
                Box::pin(next_line(Live {
                    records,
                    options,
                    stopped,
                })) as NextLine
            }),
        }
    }
}

/// The next live record as a line, or `None` when the stream is over.
async fn next_line(mut live: Live) -> Option<(Bytes, Live)> {
    loop {
        let record = tokio::select! {
            record = live.records.recv() => record,
            _ = live.stopped.changed() => return None,
        };
        match record {
            Ok(record) if live.options.wants(record.id) => match write_line(&record) {
                Ok(line) => return Some((Bytes::from(line), live)),
                Err(_) => continue,
            },
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

impl Body for EventsBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if let Some(chunk) = self.backlog.take() {
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        let Some(next) = self.next.as_mut() else {
            return Poll::Ready(None);
        };
        match next.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some((line, live))) => {
                self.next = Some(Box::pin(next_line(live)));
                Poll::Ready(Some(Ok(Frame::data(line))))
            }
            Poll::Ready(None) => {
                self.next = None;
                Poll::Ready(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use chainz_rpc::ErrorClass;
    use chainz_rpc::control::*;
    use chainz_rpc::error::SERVER_ERROR;

    use crate::events::EVENTS_BUFFER;
    use crate::testing::ControlServer;

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
                    errors: BTreeMap::from([
                        (ErrorClass::Timeout, 2),
                        (ErrorClass::RateLimited, 1),
                    ]),
                    p50_ms: 40,
                    p95_ms: 90,
                    rate_limited: 1,
                    failovers: 0,
                    bytes: 1024,
                }],
                totals: MetricsTotals::default(),
                chains: BTreeMap::new(),
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
                    code: Some(SERVER_ERROR),
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

    async fn server(dir: &tempfile::TempDir) -> ControlServer {
        ControlServer::start(&dir.path().join("serve.sock"), status(), metrics(5))
            .await
            .unwrap()
    }

    async fn raw(server: &ControlServer, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::UnixStream::connect(server.path())
            .await
            .unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
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
        let mut a = client.events(EventsOptions::default()).await.unwrap();
        let mut b = client.events(EventsOptions::default()).await.unwrap();
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
    async fn unknown_paths_wrong_methods_and_bad_queries() {
        let dir = socket_dir();
        let server = server(&dir).await;
        let out = raw(
            &server,
            "GET /nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 404"), "{out}");
        let out = raw(
            &server,
            "GET /_chainz/reload HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 405"), "{out}");
        assert!(out.to_ascii_lowercase().contains("allow: post"), "{out}");
        let out = raw(
            &server,
            "GET /_chainz/events?backlog=lots HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
        assert!(out.contains("`backlog` must be a number"), "{out}");
    }

    #[tokio::test]
    async fn events_backlog_after_and_follow() {
        let dir = socket_dir();
        let server = server(&dir).await;
        let client = ControlClient::new(server.path());
        for i in 0..5 {
            server.push_event(record(i, true));
        }
        // The last two buffered records, then the end of the stream.
        let last_two = EventsOptions {
            backlog: Backlog::Last(2),
            after_id: None,
            follow: false,
        };
        assert_eq!(ids(&client, last_two).await, [3, 4]);
        assert_eq!(server.event_subscribers(), 0);
        // Everything after an ID.
        let after = EventsOptions {
            backlog: Backlog::All,
            after_id: Some(1),
            follow: false,
        };
        assert_eq!(ids(&client, after).await, [2, 3, 4]);

        // Backlog, then live records, with nothing lost in between; `after`
        // applies to live records too.
        let mut tail = client
            .events(EventsOptions {
                backlog: Backlog::Last(100),
                after_id: Some(2),
                follow: true,
            })
            .await
            .unwrap();
        server.push_event(record(1, true));
        server.push_event(record(5, true));
        for i in [3, 4, 5] {
            let got = tokio::time::timeout(Duration::from_secs(5), tail.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(got.id, i);
        }
    }

    /// The IDs a stream that doesn't follow delivers.
    async fn ids(client: &ControlClient, options: EventsOptions) -> Vec<u64> {
        let mut events = client.events(options).await.unwrap();
        let mut ids = Vec::new();
        while let Some(record) = events.recv().await {
            ids.push(record.id);
        }
        ids
    }

    #[tokio::test]
    async fn the_ring_is_bounded() {
        let dir = socket_dir();
        let server = server(&dir).await;
        for i in 0..(EVENTS_BUFFER as u64 + 5) {
            server.push_event(record(i, true));
        }
        let all = EventsOptions {
            backlog: Backlog::All,
            after_id: None,
            follow: false,
        };
        let ids = ids(&ControlClient::new(server.path()), all).await;
        assert_eq!((ids.len(), ids[0]), (EVENTS_BUFFER, 5));
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
        let server = ControlServer::start(&socket, status(), metrics(1))
            .await
            .unwrap();
        assert!(socket.exists());
        let client = ControlClient::new(&socket);
        assert_eq!(client.status().await.unwrap(), status());
        drop(server);
        assert!(!socket.exists());
    }
}
