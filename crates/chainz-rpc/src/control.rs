//! The gateway's control API: types, a client and an in-process mock server.
//!
//! **Provisional**: the gateway owns these types and may change them.
//!
//! Protocol: HTTP/1.1 over the unix socket `serve.sock` (mode 0600), one
//! request per connection (`Connection: close`).
//!
//! | Request | Response |
//! | --- | --- |
//! | `GET /_chainz/status` | [`Status`] JSON |
//! | `GET /_chainz/metrics` | [`MetricsSnapshot`] JSON |
//! | `GET /_chainz/events[?backlog=N][&follow=false]` | `application/x-ndjson` stream of [`RequestRecord`], one per line: first the last `N` buffered records (default 0, oldest first), then live records until either side closes. With `follow=false` the stream ends after the backlog. |
//! | `POST /_chainz/reload` | `204 No Content` |
//! | anything else | `404 Not Found` |
//!
//! Response bodies may be framed by `Content-Length`, chunked transfer
//! encoding, or connection close; the client accepts all three.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use crate::error::ErrorClass;

/// Path of the status endpoint.
pub const STATUS_PATH: &str = "/_chainz/status";
/// Path of the metrics endpoint.
pub const METRICS_PATH: &str = "/_chainz/metrics";
/// Path of the events stream.
pub const EVENTS_PATH: &str = "/_chainz/events";
/// Path of the reload endpoint.
pub const RELOAD_PATH: &str = "/_chainz/reload";

/// Records the gateway (and the mock) keep for `backlog`.
pub const EVENTS_BUFFER: usize = 10_000;

/// What an events stream delivers. The default is live records only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventsOptions {
    /// Replay up to this many buffered records first, oldest first.
    pub backlog: usize,
    /// Keep streaming live records after the backlog.
    pub follow: bool,
}

impl Default for EventsOptions {
    fn default() -> Self {
        EventsOptions {
            backlog: 0,
            follow: true,
        }
    }
}

impl EventsOptions {
    /// The request path with its query string.
    pub fn path(&self) -> String {
        let mut path = EVENTS_PATH.to_string();
        let mut sep = '?';
        if self.backlog > 0 {
            path.push_str(&format!("{sep}backlog={}", self.backlog));
            sep = '&';
        }
        if !self.follow {
            path.push_str(&format!("{sep}follow=false"));
        }
        path
    }

    /// Parse the query string of an events request (unknown keys ignored).
    pub fn from_query(query: &str) -> Self {
        let mut opts = EventsOptions::default();
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("backlog", v)) => opts.backlog = v.parse().unwrap_or(0),
                Some(("follow", v)) => opts.follow = !matches!(v, "false" | "0"),
                _ => {}
            }
        }
        opts
    }
}

/// A unix socket path short enough for `sockaddr_un` (about 107 bytes).
///
/// A path that fits is used as it is. A longer one (a deep
/// `XDG_RUNTIME_DIR`, a sandboxed test directory) is reached on Linux
/// through `/proc/self/fd/<dir>/<name>`, with the directory held open for
/// as long as this value lives. Elsewhere the long path is passed through
/// and the bind or connect reports the error.
#[derive(Debug)]
pub struct SocketPath {
    path: PathBuf,
    _dir: Option<std::fs::File>,
}

impl SocketPath {
    /// Longest path used directly.
    const MAX_DIRECT: usize = 100;

    /// Resolve `path` for binding or connecting.
    pub fn new(path: &Path) -> io::Result<Self> {
        let direct = SocketPath {
            path: path.to_path_buf(),
            _dir: None,
        };
        if path.as_os_str().len() <= Self::MAX_DIRECT || !cfg!(target_os = "linux") {
            return Ok(direct);
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Ok(direct);
        };
        use std::os::fd::AsRawFd;
        let dir = std::fs::File::open(parent)?;
        Ok(SocketPath {
            path: PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())).join(name),
            _dir: Some(dir),
        })
    }

    /// The path to hand to `bind` or `connect`.
    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

/// Largest non-streaming response body the client accepts.
const MAX_BODY: usize = 64 * 1024 * 1024;
/// Largest single events line the client accepts.
const MAX_LINE: usize = 1024 * 1024;

/// Gateway status.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    /// Gateway (chainz) version.
    pub version: String,
    /// Gateway process ID.
    pub pid: u32,
    /// TCP port the gateway listens on.
    pub port: u16,
    /// Seconds since the gateway started.
    pub uptime_secs: u64,
    /// One entry per served chain.
    pub chains: Vec<ChainStatus>,
}

/// Status of one served chain.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChainStatus {
    /// Chain name.
    pub chain: String,
    /// Chain ID.
    pub chain_id: u64,
    /// Name of the active endpoint.
    pub active: String,
    /// Names of the failover endpoints, in order.
    pub failover: Vec<String>,
    /// The head high-water mark, if known.
    pub head: Option<u64>,
}

/// Metrics over every window at one instant.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetricsSnapshot {
    /// When the snapshot was taken, in unix milliseconds.
    pub at_ms: u64,
    /// One entry per window (e.g. 60 s and 900 s).
    pub windows: Vec<MetricsWindow>,
}

/// Metrics over one sliding window.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetricsWindow {
    /// Window length in seconds.
    pub window_secs: u64,
    /// One row per (chain, endpoint, method).
    pub rows: Vec<MetricsRow>,
}

/// Metrics for one (chain, endpoint, method) within a window.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetricsRow {
    /// Chain name.
    pub chain: String,
    /// Endpoint name.
    pub endpoint: String,
    /// JSON-RPC method.
    pub method: String,
    /// Requests sent.
    pub requests: u64,
    /// Errors by class.
    pub errors: BTreeMap<ErrorClass, u64>,
    /// Median latency in milliseconds.
    pub p50_ms: u32,
    /// 95th percentile latency in milliseconds.
    pub p95_ms: u32,
    /// HTTP 429 responses.
    pub rate_limited: u64,
    /// Requests that failed over to another endpoint.
    pub failovers: u64,
    /// Response bytes.
    pub bytes: u64,
}

/// One proxied request, as shown by the inspector.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestRecord {
    /// Monotonic record ID.
    pub id: u64,
    /// When the request arrived, in unix milliseconds.
    pub at_ms: u64,
    /// Chain name.
    pub chain: String,
    /// JSON-RPC method (or a batch summary).
    pub method: String,
    /// Short human-readable parameter summary.
    pub params_summary: String,
    /// Name of the endpoint that served (or last failed) the request.
    pub upstream: String,
    /// Attempts made across endpoints.
    pub attempts: u32,
    /// Total latency in milliseconds.
    pub latency_ms: u32,
    /// Outcome.
    pub status: RecordStatus,
    /// Response size in bytes.
    pub response_bytes: u64,
}

/// The outcome of a [`RequestRecord`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordStatus {
    /// Answered successfully.
    Ok,
    /// Failed.
    Error {
        /// The error class.
        class: ErrorClass,
        /// The JSON-RPC error code, for RPC errors.
        code: Option<i64>,
    },
}

/// A client for the control socket.
#[derive(Debug, Clone)]
pub struct ControlClient {
    socket: PathBuf,
}

impl ControlClient {
    /// A client for the socket at `socket` (no connection is made yet).
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        ControlClient {
            socket: socket.into(),
        }
    }

    /// `GET /_chainz/status`.
    pub async fn status(&self) -> io::Result<Status> {
        self.get_json(STATUS_PATH).await
    }

    /// `GET /_chainz/metrics`.
    pub async fn metrics(&self) -> io::Result<MetricsSnapshot> {
        self.get_json(METRICS_PATH).await
    }

    /// `GET /_chainz/events`. Returns once the server has accepted the stream
    /// (every record pushed after that is delivered). The receiver closes when
    /// the server closes the stream; dropping it closes the connection.
    /// Malformed lines are skipped.
    pub async fn events(&self) -> io::Result<mpsc::Receiver<RequestRecord>> {
        self.events_with(EventsOptions::default()).await
    }

    /// `GET /_chainz/events` with a backlog and/or without following; see
    /// [`EventsOptions`]. With `follow: false` the receiver closes after the
    /// backlog.
    pub async fn events_with(
        &self,
        options: EventsOptions,
    ) -> io::Result<mpsc::Receiver<RequestRecord>> {
        let mut body = self.send("GET", &options.path()).await?;
        let (tx, rx) = mpsc::channel(1024);
        tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::new();
            loop {
                let chunk = tokio::select! {
                    _ = tx.closed() => return,
                    c = body.next_chunk() => c,
                };
                let Ok(Some(chunk)) = chunk else { return };
                buf.extend_from_slice(&chunk);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line = line.trim_ascii();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(record) = serde_json::from_slice::<RequestRecord>(line)
                        && tx.send(record).await.is_err()
                    {
                        return;
                    }
                }
                if buf.len() > MAX_LINE {
                    return;
                }
            }
        });
        Ok(rx)
    }

    /// `POST /_chainz/reload`.
    pub async fn reload(&self) -> io::Result<()> {
        let mut body = self.send("POST", RELOAD_PATH).await?;
        body.read_all().await?;
        Ok(())
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> io::Result<T> {
        let mut body = self.send("GET", path).await?;
        let bytes = body.read_all().await?;
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Send a request and read the response head; errors on non-2xx.
    async fn send(&self, method: &str, path: &str) -> io::Result<Body<UnixStream>> {
        let socket = SocketPath::new(&self.socket)?;
        let mut stream = UnixStream::connect(socket.as_path()).await?;
        drop(socket);
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await?;
        let mut reader = BufReader::new(stream);
        let head = read_head(&mut reader).await?;
        let status = head
            .first_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| invalid("malformed status line"))?;
        if !(200..300).contains(&status) {
            return Err(io::Error::other(format!(
                "control API {method} {path} returned HTTP {status}"
            )));
        }
        let mode = if status == 204 {
            BodyMode::Length(0)
        } else if head
            .header("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
        {
            BodyMode::Chunked
        } else if let Some(len) = head.header("content-length") {
            BodyMode::Length(
                len.trim()
                    .parse()
                    .map_err(|_| invalid("malformed content-length"))?,
            )
        } else {
            BodyMode::Eof
        };
        Ok(Body { reader, mode })
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// A parsed HTTP/1.1 message head.
struct Head {
    first_line: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn read_head<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> io::Result<Head> {
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).await? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed before response",
        ));
    }
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated HTTP head",
            ));
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if headers.len() > 100 {
            return Err(invalid("too many headers"));
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Head {
        first_line: first_line.trim_end().to_string(),
        headers,
    })
}

enum BodyMode {
    Length(usize),
    Chunked,
    ChunkedDone,
    Eof,
}

/// An HTTP/1.1 response body reader.
struct Body<R> {
    reader: BufReader<R>,
    mode: BodyMode,
}

impl<R: AsyncRead + Unpin> Body<R> {
    /// The next piece of the body; `None` at the end.
    async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        match &mut self.mode {
            BodyMode::Length(0) | BodyMode::ChunkedDone => Ok(None),
            BodyMode::Length(remaining) => {
                let mut buf = vec![0; (*remaining).min(64 * 1024)];
                let n = self.reader.read(&mut buf).await?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated body",
                    ));
                }
                *remaining -= n;
                buf.truncate(n);
                Ok(Some(buf))
            }
            BodyMode::Eof => {
                let mut buf = vec![0; 64 * 1024];
                let n = self.reader.read(&mut buf).await?;
                if n == 0 {
                    return Ok(None);
                }
                buf.truncate(n);
                Ok(Some(buf))
            }
            BodyMode::Chunked => {
                let mut line = String::new();
                if self.reader.read_line(&mut line).await? == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated chunk",
                    ));
                }
                let size_str = line.trim().split(';').next().unwrap_or("");
                let size = usize::from_str_radix(size_str.trim(), 16)
                    .map_err(|_| invalid("malformed chunk size"))?;
                if size == 0 {
                    // Discard trailers up to the final empty line.
                    loop {
                        let mut t = String::new();
                        if self.reader.read_line(&mut t).await? == 0 || t.trim().is_empty() {
                            break;
                        }
                    }
                    self.mode = BodyMode::ChunkedDone;
                    return Ok(None);
                }
                if size > MAX_BODY {
                    return Err(invalid("chunk too large"));
                }
                let mut buf = vec![0; size];
                self.reader.read_exact(&mut buf).await?;
                let mut crlf = [0u8; 2];
                self.reader.read_exact(&mut crlf).await?;
                Ok(Some(buf))
            }
        }
    }

    async fn read_all(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            out.extend_from_slice(&chunk);
            if out.len() > MAX_BODY {
                return Err(invalid("response body too large"));
            }
        }
        Ok(out)
    }
}

struct MockState {
    status: Mutex<Status>,
    metrics: Mutex<MetricsSnapshot>,
    reloads: AtomicU64,
    events: broadcast::Sender<RequestRecord>,
    history: Mutex<std::collections::VecDeque<RequestRecord>>,
}

/// An in-process mock of the control socket, for tests of control clients
/// (the dashboard builds against this). Events are streamed with chunked
/// transfer encoding, as a hyper-based server would.
///
/// Dropping it stops the server, closes every connection and removes the
/// socket file.
pub struct MockControlServer {
    path: PathBuf,
    state: Arc<MockState>,
    accept: JoinHandle<()>,
}

impl std::fmt::Debug for MockControlServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockControlServer")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl MockControlServer {
    /// Bind `socket` (mode 0600; a stale file there is replaced) and serve.
    pub async fn start(
        socket: &Path,
        status: Status,
        metrics: MetricsSnapshot,
    ) -> io::Result<Self> {
        match std::fs::remove_file(socket) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(SocketPath::new(socket)?.as_path())?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
        }
        let (events, _) = broadcast::channel(1024);
        let state = Arc::new(MockState {
            status: Mutex::new(status),
            metrics: Mutex::new(metrics),
            reloads: AtomicU64::new(0),
            events,
            history: Mutex::new(std::collections::VecDeque::new()),
        });
        let accept_state = state.clone();
        let accept = tokio::spawn(async move {
            // Owned by this task: aborting it aborts every connection.
            let mut conns = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        conns.spawn(serve_conn(stream, accept_state.clone()));
                    }
                    Some(_) = conns.join_next(), if !conns.is_empty() => {}
                }
            }
        });
        Ok(MockControlServer {
            path: socket.to_path_buf(),
            state,
            accept,
        })
    }

    /// Broadcast a record to every connected events client, and keep it for
    /// `backlog` requests (the last [`EVENTS_BUFFER`] are kept).
    pub fn push_event(&self, record: RequestRecord) {
        // Under the history lock, so a connecting stream sees each record
        // exactly once (in its backlog or live).
        let mut history = self.state.history.lock().unwrap_or_else(|e| e.into_inner());
        if history.len() == EVENTS_BUFFER {
            history.pop_front();
        }
        history.push_back(record.clone());
        let _ = self.state.events.send(record);
    }

    /// Replace the snapshot served by `/_chainz/metrics`.
    pub fn set_metrics(&self, metrics: MetricsSnapshot) {
        *self.state.metrics.lock().unwrap_or_else(|e| e.into_inner()) = metrics;
    }

    /// Replace the status served by `/_chainz/status`.
    pub fn set_status(&self, status: Status) {
        *self.state.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
    }

    /// How many `POST /_chainz/reload` requests were served.
    pub fn reload_count(&self) -> u64 {
        self.state.reloads.load(Ordering::SeqCst)
    }

    /// How many events streams are currently connected.
    pub fn event_subscribers(&self) -> usize {
        self.state.events.receiver_count()
    }

    /// The socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for MockControlServer {
    fn drop(&mut self) {
        self.accept.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn serve_conn(stream: UnixStream, state: Arc<MockState>) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let Ok(head) = read_head(&mut reader).await else {
        return;
    };
    if let Some(len) = head
        .header("content-length")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        let mut discard = (&mut reader).take(len);
        if tokio::io::copy(&mut discard, &mut tokio::io::sink())
            .await
            .is_err()
        {
            return;
        }
    }
    let mut parts = head.first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let json_response = |body: Vec<u8>| {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(&body);
        r
    };
    let simple = |status: &str| {
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
    };

    let known = [STATUS_PATH, METRICS_PATH, EVENTS_PATH, RELOAD_PATH].contains(&path);
    let response = match (method, path) {
        ("GET", STATUS_PATH) => {
            let status = state
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            json_response(serde_json::to_vec(&status).unwrap_or_default())
        }
        ("GET", METRICS_PATH) => {
            let metrics = state
                .metrics
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            json_response(serde_json::to_vec(&metrics).unwrap_or_default())
        }
        ("POST", RELOAD_PATH) => {
            state.reloads.fetch_add(1, Ordering::SeqCst);
            simple("204 No Content")
        }
        ("GET", EVENTS_PATH) => {
            let options = EventsOptions::from_query(query);
            let (backlog, rx) = {
                let history = state.history.lock().unwrap_or_else(|e| e.into_inner());
                let skip = history.len().saturating_sub(options.backlog);
                let backlog: Vec<RequestRecord> = history.iter().skip(skip).cloned().collect();
                (backlog, options.follow.then(|| state.events.subscribe()))
            };
            stream_events(reader, write, backlog, rx).await;
            return;
        }
        _ if known => simple("405 Method Not Allowed"),
        _ => simple("404 Not Found"),
    };
    let _ = write.write_all(&response).await;
    let _ = write.shutdown().await;
}

async fn stream_events(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut write: tokio::net::unix::OwnedWriteHalf,
    backlog: Vec<RequestRecord>,
    rx: Option<broadcast::Receiver<RequestRecord>>,
) {
    fn chunk(record: &RequestRecord) -> Vec<u8> {
        let mut line = serde_json::to_vec(record).unwrap_or_default();
        line.push(b'\n');
        let mut chunk = format!("{:x}\r\n", line.len()).into_bytes();
        chunk.extend_from_slice(&line);
        chunk.extend_from_slice(b"\r\n");
        chunk
    }

    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    if write.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for record in &backlog {
        if write.write_all(&chunk(record)).await.is_err() {
            return;
        }
    }
    let Some(mut rx) = rx else {
        let _ = write.write_all(b"0\r\n\r\n").await;
        return;
    };
    let mut scratch = [0u8; 256];
    loop {
        tokio::select! {
            record = rx.recv() => match record {
                Ok(record) => {
                    if write.write_all(&chunk(&record)).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    let _ = write.write_all(b"0\r\n\r\n").await;
                    return;
                }
            },
            n = reader.read(&mut scratch) => {
                if !matches!(n, Ok(n) if n > 0) {
                    return; // client closed
                }
            }
        }
    }
}
