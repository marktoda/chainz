//! The gateway's control protocol: its types, its routes, its NDJSON
//! framing, and a client. The server lives in `chainz-serve`.
//!
//! The protocol is stable: the gateway, `chainz serve status|log` and the
//! dashboard are all built on it.
//!
//! Protocol: HTTP/1.1 over the unix socket `serve.sock` (mode 0600), one
//! request per connection (`Connection: close`).
//!
//! | Request | Response |
//! | --- | --- |
//! | `GET /_chainz/status` | [`Status`] JSON |
//! | `GET /_chainz/metrics` | [`MetricsSnapshot`] JSON |
//! | `GET /_chainz/events[?backlog=N\|all][&after=ID][&follow=false]` | `application/x-ndjson` stream of [`RequestRecord`], one per line: first the last `N` buffered records (default 0, `all` for every one, oldest first) with an ID above `after`, then live records until either side closes. With `follow=false` the stream ends after the backlog. A malformed query is `400 Bad Request`. |
//! | `POST /_chainz/reload` | `204 No Content`; `500` with the cause as text when the new config is invalid (the old one stays in force) |
//! | anything else | `404 Not Found` (`405` for a known path with the wrong method) |
//!
//! Response bodies may be framed by `Content-Length`, chunked transfer
//! encoding, or connection close; the client accepts all three.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::error::ErrorClass;

/// How long [`ControlClient`] waits to connect and read a response head
/// (and, except for the events stream, the whole body).
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// The control API's endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRoute {
    /// `GET /_chainz/status`.
    Status,
    /// `GET /_chainz/metrics`.
    Metrics,
    /// `GET /_chainz/events`.
    Events,
    /// `POST /_chainz/reload`.
    Reload,
}

impl ControlRoute {
    /// Every route.
    pub const ALL: [ControlRoute; 4] = [
        ControlRoute::Status,
        ControlRoute::Metrics,
        ControlRoute::Events,
        ControlRoute::Reload,
    ];

    /// The request path.
    pub fn path(self) -> &'static str {
        match self {
            ControlRoute::Status => "/_chainz/status",
            ControlRoute::Metrics => "/_chainz/metrics",
            ControlRoute::Events => "/_chainz/events",
            ControlRoute::Reload => "/_chainz/reload",
        }
    }

    /// The HTTP method it takes.
    pub fn method(self) -> &'static str {
        match self {
            ControlRoute::Reload => "POST",
            _ => "GET",
        }
    }

    /// The route at `path` (without the query), if any.
    pub fn from_path(path: &str) -> Option<ControlRoute> {
        ControlRoute::ALL
            .into_iter()
            .find(|route| route.path() == path)
    }
}

/// How much of the buffer an events stream replays first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backlog {
    /// Up to this many records, the most recent ones.
    Last(usize),
    /// Everything the gateway still holds.
    All,
}

/// What an events stream delivers. The default is live records only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventsOptions {
    /// Replay these buffered records first, oldest first.
    pub backlog: Backlog,
    /// Only records with an ID above this one, in the backlog and live.
    pub after_id: Option<u64>,
    /// Keep streaming live records after the backlog.
    pub follow: bool,
}

impl Default for EventsOptions {
    fn default() -> Self {
        EventsOptions {
            backlog: Backlog::Last(0),
            after_id: None,
            follow: true,
        }
    }
}

impl EventsOptions {
    /// The request path with its query string.
    fn path(&self) -> String {
        let mut query = Vec::new();
        match self.backlog {
            Backlog::Last(0) => {}
            Backlog::Last(n) => query.push(format!("backlog={n}")),
            Backlog::All => query.push("backlog=all".to_string()),
        }
        if let Some(id) = self.after_id {
            query.push(format!("after={id}"));
        }
        if !self.follow {
            query.push("follow=false".to_string());
        }
        match query.is_empty() {
            true => ControlRoute::Events.path().to_string(),
            false => format!("{}?{}", ControlRoute::Events.path(), query.join("&")),
        }
    }

    /// Parse the query string of an events request. Unknown keys are
    /// ignored (for older clients and newer ones alike); a known key with
    /// a bad value is an error.
    pub fn from_query(query: &str) -> Result<Self, String> {
        let mut options = EventsOptions::default();
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let number = || {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("`{key}` must be a number, not `{value}`"))
            };
            match key {
                "backlog" if value == "all" => options.backlog = Backlog::All,
                "backlog" => {
                    options.backlog =
                        Backlog::Last(usize::try_from(number()?).unwrap_or(usize::MAX))
                }
                "after" => options.after_id = Some(number()?),
                "follow" => {
                    options.follow = match value {
                        "true" | "1" => true,
                        "false" | "0" => false,
                        _ => return Err(format!("`follow` must be true or false, not `{value}`")),
                    }
                }
                _ => {}
            }
        }
        Ok(options)
    }

    /// Whether a record with this ID is one the stream asked for.
    pub fn wants(&self, id: u64) -> bool {
        self.after_id.is_none_or(|after| id > after)
    }
}

/// One NDJSON line: `value` as JSON, then `\n`.
pub fn write_line<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    Ok(line)
}

/// Take the next complete NDJSON line out of `buf` and parse it, skipping
/// blank lines. `None` when no complete line is left; the rest of `buf` is
/// kept for the next read.
pub(crate) fn read_line<T: DeserializeOwned>(buf: &mut Vec<u8>) -> Option<serde_json::Result<T>> {
    while let Some(end) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=end).collect();
        let line = line.trim_ascii();
        if !line.is_empty() {
            return Some(serde_json::from_slice(line));
        }
    }
    None
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
/// Most of an error body the client quotes.
const MAX_ERROR_BODY: usize = 4096;

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

/// The gateway's two metrics windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// The last minute.
    Short,
    /// The last 15 minutes.
    Long,
}

impl Window {
    /// The window's length in seconds.
    pub const fn secs(self) -> u64 {
        match self {
            Window::Short => 60,
            Window::Long => 900,
        }
    }
}

/// Metrics over every window at one instant.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetricsSnapshot {
    /// When the snapshot was taken, in unix milliseconds.
    pub at_ms: u64,
    /// One entry per window (e.g. 60 s and 900 s).
    pub windows: Vec<MetricsWindow>,
}

impl MetricsSnapshot {
    /// The given window, if the gateway sent it.
    pub fn window(&self, window: Window) -> Option<&MetricsWindow> {
        self.windows.iter().find(|w| w.window_secs == window.secs())
    }
}

/// Metrics over one sliding window.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetricsWindow {
    /// Window length in seconds.
    pub window_secs: u64,
    /// One row per (chain, endpoint, method).
    pub rows: Vec<MetricsRow>,
    /// Every row together, with true percentiles.
    pub totals: MetricsTotals,
    /// Each chain's rows together, with true percentiles.
    pub chains: BTreeMap<String, MetricsTotals>,
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
    /// Upstream attempts sent.
    pub requests: u64,
    /// Errors by class.
    pub errors: BTreeMap<ErrorClass, u64>,
    /// Median latency in milliseconds.
    pub p50_ms: u32,
    /// 95th percentile latency in milliseconds.
    pub p95_ms: u32,
    /// HTTP 429 responses, plus requests the gateway's own rate limiter
    /// turned away without sending (those are not counted in `requests`).
    pub rate_limited: u64,
    /// Requests that failed over to another endpoint.
    pub failovers: u64,
    /// Response bytes.
    pub bytes: u64,
}

/// Many [`MetricsRow`]s together. The percentiles are over every sample,
/// not an average of the rows' percentiles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetricsTotals {
    /// Upstream attempts sent.
    pub requests: u64,
    /// Failed attempts.
    pub errors: u64,
    /// As [`MetricsRow::rate_limited`].
    pub rate_limited: u64,
    /// Requests that failed over to another endpoint.
    pub failovers: u64,
    /// Median latency in milliseconds.
    pub p50_ms: u32,
    /// 95th percentile latency in milliseconds.
    pub p95_ms: u32,
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

/// `ok`, `rpc(-32000)`, or the class alone (`timeout`).
impl std::fmt::Display for RecordStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordStatus::Ok => f.write_str("ok"),
            RecordStatus::Error {
                class,
                code: Some(code),
            } => write!(f, "{class}({code})"),
            RecordStatus::Error { class, code: None } => write!(f, "{class}"),
        }
    }
}

/// A client for the control socket.
///
/// Every call gives up after 5 s, so a wedged gateway can't hang the
/// caller; only the body of an events stream is open-ended.
#[derive(Debug, Clone)]
pub struct ControlClient {
    socket: PathBuf,
    timeout: Duration,
}

impl ControlClient {
    /// A client for the socket at `socket` (no connection is made yet).
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        ControlClient {
            socket: socket.into(),
            timeout: CONTROL_TIMEOUT,
        }
    }

    /// Replace the 5 s timeout.
    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Run `call` under the timeout.
    async fn timed<T>(&self, call: impl Future<Output = io::Result<T>>) -> io::Result<T> {
        tokio::time::timeout(self.timeout, call)
            .await
            .unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the gateway's control socket did not answer in time",
                ))
            })
    }

    /// `GET /_chainz/status`.
    pub async fn status(&self) -> io::Result<Status> {
        self.get_json(ControlRoute::Status).await
    }

    /// `GET /_chainz/metrics`.
    pub async fn metrics(&self) -> io::Result<MetricsSnapshot> {
        self.get_json(ControlRoute::Metrics).await
    }

    /// `GET /_chainz/events`; see [`EventsOptions`]. Returns once the server
    /// has accepted the stream (every record pushed after that is
    /// delivered). The receiver closes when the server closes the stream
    /// (after the backlog, with `follow: false`); dropping it closes the
    /// connection. Malformed lines are skipped.
    pub async fn events(
        &self,
        options: EventsOptions,
    ) -> io::Result<mpsc::Receiver<RequestRecord>> {
        // Only the head is timed: the stream itself runs until either side
        // closes it.
        let mut body = self
            .timed(self.send(ControlRoute::Events.method(), &options.path()))
            .await?;
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
                while let Some(record) = read_line::<RequestRecord>(&mut buf) {
                    if let Ok(record) = record
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

    /// `POST /_chainz/reload`. A refused reload is an error carrying the
    /// gateway's reason.
    pub async fn reload(&self) -> io::Result<()> {
        self.timed(async {
            let route = ControlRoute::Reload;
            let mut body = self.send(route.method(), route.path()).await?;
            body.read_all().await?;
            Ok(())
        })
        .await
    }

    async fn get_json<T: DeserializeOwned>(&self, route: ControlRoute) -> io::Result<T> {
        let bytes = self
            .timed(async {
                self.send(route.method(), route.path())
                    .await?
                    .read_all()
                    .await
            })
            .await?;
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Send a request and read the response head; errors on non-2xx, with
    /// the start of the response body.
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
        let mut body = Body { reader, mode };
        if !(200..300).contains(&status) {
            let text = body.read_all().await.unwrap_or_default();
            let text = String::from_utf8_lossy(&text[..text.len().min(MAX_ERROR_BODY)]);
            let reason = match text.trim() {
                "" => String::new(),
                text => format!(": {text}"),
            };
            return Err(io::Error::other(format!(
                "control API {method} {path} returned HTTP {status}{reason}"
            )));
        }
        Ok(body)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_round_trip() {
        for route in ControlRoute::ALL {
            assert_eq!(ControlRoute::from_path(route.path()), Some(route));
        }
        assert_eq!(ControlRoute::from_path("/_chainz/nope"), None);
        assert_eq!(ControlRoute::Reload.method(), "POST");
    }

    #[test]
    fn events_query_round_trips() {
        let cases = [
            EventsOptions::default(),
            EventsOptions {
                backlog: Backlog::Last(7),
                after_id: None,
                follow: false,
            },
            EventsOptions {
                backlog: Backlog::All,
                after_id: Some(41),
                follow: true,
            },
        ];
        for options in cases {
            let path = options.path();
            let query = path.split_once('?').map_or("", |(_, q)| q);
            assert_eq!(EventsOptions::from_query(query), Ok(options), "{path}");
        }
        assert_eq!(EventsOptions::default().path(), ControlRoute::Events.path());
        assert_eq!(cases[1].path(), "/_chainz/events?backlog=7&follow=false");
        assert_eq!(cases[2].path(), "/_chainz/events?backlog=all&after=41");
        // Unknown keys are ignored; bad values are errors.
        assert_eq!(
            EventsOptions::from_query("x=1&follow=0"),
            Ok(EventsOptions {
                follow: false,
                ..EventsOptions::default()
            })
        );
        for bad in ["backlog=-1", "backlog=lots", "after=", "follow=maybe"] {
            assert!(EventsOptions::from_query(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ndjson_lines() {
        let mut buf = Vec::new();
        buf.extend(write_line(&1u32).unwrap());
        buf.extend(b"\n  \nnot json\n[2");
        assert_eq!(read_line::<u32>(&mut buf).unwrap().unwrap(), 1);
        assert!(read_line::<u32>(&mut buf).unwrap().is_err());
        assert!(read_line::<u32>(&mut buf).is_none());
        assert_eq!(buf, b"[2", "a partial line is kept");
    }

    #[test]
    fn record_status_display() {
        let error = |code| RecordStatus::Error {
            class: ErrorClass::Rpc,
            code,
        };
        assert_eq!(RecordStatus::Ok.to_string(), "ok");
        assert_eq!(error(Some(3)).to_string(), "rpc(3)");
        assert_eq!(error(None).to_string(), "rpc");
    }

    #[tokio::test]
    async fn a_wedged_gateway_times_out() {
        // Accepts connections, reads the request, never answers.
        let dir = chainz_testkit::short_tempdir();
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
            client
                .events(EventsOptions::default())
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        hold.abort();
        let _ = hold.await;
    }
}
