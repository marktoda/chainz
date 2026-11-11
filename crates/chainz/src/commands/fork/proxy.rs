//! The fork's upstream proxy: a single-chain, loopback HTTP/1.1 server
//! that forwards JSON-RPC bodies to **one pinned endpoint** for the fork's
//! whole life.
//!
//! Anvil fetches fork state lazily, so every request must come from the
//! same node: the proxy retries transient errors on that endpoint
//! ([`RetryPolicy::same_endpoint_transient`]) and never fails over. Its URL
//! is `http://127.0.0.1:<port>/<token>`: a random per-start token and no
//! endpoint secret, so anvil's argv is safe to show in `ps`.
//!
//! The server is deliberately small: `POST /<token>` with a
//! `Content-Length` body, keep-alive, nothing else. No CORS, no other
//! methods, and a loopback `Host` is required.

use bytes::Bytes;
use chainz_rpc::{ErrorClass, RetryPolicy, RpcError, Target, Upstream};
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Largest request head accepted.
const MAX_HEAD: usize = 64 * 1024;
/// Largest request body accepted.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// A running proxy. Its tasks live on the runtime that started it.
pub struct Proxy {
    port: u16,
    token: String,
    task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    /// The URL handed to `anvil --fork-url`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, self.token)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct State {
    upstream: Arc<dyn Upstream>,
    target: Target,
    policy: RetryPolicy,
    token: String,
}

/// Bind a loopback port and serve until dropped.
pub async fn start(upstream: Arc<dyn Upstream>, target: Target) -> io::Result<Proxy> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let token = random_token();
    let state = Arc::new(State {
        upstream,
        target,
        policy: RetryPolicy::same_endpoint_transient(),
        token: token.clone(),
    });
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let _ = serve_connection(stream, &state).await;
            });
        }
    });
    Ok(Proxy { port, token, task })
}

/// 128 random bits as hex.
fn random_token() -> String {
    use rand::Rng;
    let bytes: [u8; 16] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The parts of a request head the proxy acts on.
#[derive(Debug, PartialEq, Eq)]
struct Head {
    method: String,
    path: String,
    host: Option<String>,
    content_length: Option<usize>,
    chunked: bool,
    close: bool,
}

/// A refusal: HTTP status and a message (never echoes request content).
type Refusal = (u16, &'static str);

/// Parse a request head (through the blank line).
fn parse_head(head: &[u8]) -> Result<Head, Refusal> {
    let text = std::str::from_utf8(head).map_err(|_| (400, "malformed request"))?;
    let mut lines = text.split("\r\n");
    let mut request = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(path), Some(version)) =
        (request.next(), request.next(), request.next())
    else {
        return Err((400, "malformed request line"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err((505, "HTTP/1.1 only"));
    }
    let mut parsed = Head {
        method: method.to_string(),
        path: path.to_string(),
        host: None,
        content_length: None,
        chunked: false,
        close: version == "HTTP/1.0",
    };
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line.split_once(':').ok_or((400, "malformed header"))?;
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "host" => parsed.host = Some(value.to_ascii_lowercase()),
            "content-length" => {
                parsed.content_length = Some(value.parse().map_err(|_| (400, "bad length"))?);
            }
            "transfer-encoding" => parsed.chunked = true,
            "connection" => {
                let value = value.to_ascii_lowercase();
                if value.contains("close") {
                    parsed.close = true;
                } else if value.contains("keep-alive") {
                    parsed.close = false;
                }
            }
            _ => {}
        }
    }
    Ok(parsed)
}

/// Whether the proxy forwards this request; otherwise the refusal.
fn authorize(head: &Head, token: &str) -> Result<usize, Refusal> {
    // The token gates everything: without it the proxy admits nothing.
    let path = head.path.split('?').next().unwrap_or_default();
    let given = path
        .strip_prefix('/')
        .unwrap_or_default()
        .trim_end_matches('/');
    if !constant_time_eq(given.as_bytes(), token.as_bytes()) {
        return Err((404, "not found"));
    }
    if !head.host.as_deref().is_some_and(is_loopback_host) {
        return Err((403, "loopback host required"));
    }
    if head.method != "POST" {
        return Err((405, "POST only"));
    }
    if head.chunked {
        return Err((411, "content-length required"));
    }
    match head.content_length {
        Some(n) if n > MAX_BODY => Err((413, "request too large")),
        Some(n) => Ok(n),
        None => Err((411, "content-length required")),
    }
}

/// `Host` must name loopback (blocks DNS rebinding from a browser).
fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

async fn serve_connection(mut stream: TcpStream, state: &State) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    loop {
        let head_end = loop {
            if let Some(end) = find_head_end(&buf) {
                break end;
            }
            if buf.len() > MAX_HEAD {
                return respond(
                    &mut stream,
                    431,
                    &error_body("request head too large"),
                    true,
                )
                .await;
            }
            if stream.read_buf(&mut buf).await? == 0 {
                return Ok(());
            }
        };
        let head = match parse_head(&buf[..head_end]) {
            Ok(head) => head,
            Err((status, message)) => {
                return respond(&mut stream, status, &error_body(message), true).await;
            }
        };
        let length = match authorize(&head, &state.token) {
            Ok(length) => length,
            // The body's extent is unknown or unwanted: answer and close.
            Err((status, message)) => {
                return respond(&mut stream, status, &error_body(message), true).await;
            }
        };
        let total = head_end + length;
        while buf.len() < total {
            if stream.read_buf(&mut buf).await? == 0 {
                return Ok(());
            }
        }
        let body = Bytes::copy_from_slice(&buf[head_end..total]);
        buf.drain(..total);
        let (status, response) = forward(state, body).await;
        respond(&mut stream, status, &response, head.close).await?;
        if head.close {
            return Ok(());
        }
    }
}

/// Forward one body to the pinned endpoint. Upstream failures become an
/// HTTP error status so anvil's own retry logic sees them as transport
/// errors, never as fork state.
async fn forward(state: &State, body: Bytes) -> (u16, Bytes) {
    match state
        .upstream
        .send(&state.target, body, &state.policy)
        .await
    {
        Ok(response) => (200, response.body),
        Err(error) => (
            error_status(&error),
            error_body(&format!("chainz fork proxy: upstream {error}")),
        ),
    }
}

fn error_status(error: &RpcError) -> u16 {
    match error.class() {
        ErrorClass::RateLimited => 429,
        ErrorClass::Timeout => 504,
        _ => 502,
    }
}

fn error_body(message: &str) -> Bytes {
    Bytes::from(
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32000, "message": message},
        })
        .to_string(),
    )
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    }
}

async fn respond(stream: &mut TcpStream, status: u16, body: &[u8], close: bool) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        reason(status),
        body.len()
    );
    if close {
        head.push_str("connection: close\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chainz_rpc::upstream::BoxFuture;
    use chainz_rpc::{RpcClient, UpstreamResponse};
    use std::sync::Mutex;
    use std::time::Duration;

    fn head(text: &str) -> Result<Head, Refusal> {
        parse_head(text.as_bytes())
    }

    #[test]
    fn parses_a_request_head() {
        let h = head(
            "POST /tok HTTP/1.1\r\nHost: 127.0.0.1:9\r\nContent-Length: 12\r\nConnection: keep-alive\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            (h.method.as_str(), h.path.as_str(), h.content_length),
            ("POST", "/tok", Some(12))
        );
        assert!(!h.close && !h.chunked);
        assert!(head("POST /tok HTTP/1.0\r\n\r\n").unwrap().close);
        assert!(
            head("POST /tok HTTP/1.1\r\nConnection: close\r\n\r\n")
                .unwrap()
                .close
        );
        assert_eq!(head("garbage\r\n\r\n").unwrap_err().0, 400);
        assert_eq!(head("POST /tok HTTP/2\r\n\r\n").unwrap_err().0, 505);
        assert_eq!(
            head("POST /tok HTTP/1.1\r\nContent-Length: x\r\n\r\n")
                .unwrap_err()
                .0,
            400
        );
    }

    #[test]
    fn only_a_tokened_loopback_post_is_forwarded() {
        let ok = |text: &str| authorize(&head(text).unwrap(), "tok");
        let base = "Host: 127.0.0.1:9\r\nContent-Length: 2\r\n\r\n";
        assert_eq!(ok(&format!("POST /tok HTTP/1.1\r\n{base}")), Ok(2));
        assert_eq!(ok(&format!("POST /tok/ HTTP/1.1\r\n{base}")), Ok(2));
        assert_eq!(
            ok(&format!("POST /nope HTTP/1.1\r\n{base}")).unwrap_err().0,
            404
        );
        assert_eq!(
            ok(&format!("POST / HTTP/1.1\r\n{base}")).unwrap_err().0,
            404
        );
        assert_eq!(
            ok(&format!("POST /tok/extra HTTP/1.1\r\n{base}"))
                .unwrap_err()
                .0,
            404
        );
        assert_eq!(
            ok(&format!("GET /tok HTTP/1.1\r\n{base}")).unwrap_err().0,
            405
        );
        assert_eq!(
            ok(&format!("OPTIONS /tok HTTP/1.1\r\n{base}"))
                .unwrap_err()
                .0,
            405
        );
        assert_eq!(
            ok("POST /tok HTTP/1.1\r\nHost: evil.example\r\nContent-Length: 2\r\n\r\n")
                .unwrap_err()
                .0,
            403
        );
        assert_eq!(
            ok("POST /tok HTTP/1.1\r\nContent-Length: 2\r\n\r\n")
                .unwrap_err()
                .0,
            403
        );
        assert_eq!(
            ok("POST /tok HTTP/1.1\r\nHost: localhost:1\r\n\r\n")
                .unwrap_err()
                .0,
            411
        );
        assert_eq!(
            ok("POST /tok HTTP/1.1\r\nHost: [::1]:1\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap_err()
                .0,
            411
        );
    }

    #[test]
    fn tokens_are_random_and_url_safe() {
        let (a, b) = (random_token(), random_token());
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// Records every target it is asked to reach and fails on demand.
    struct Recorder {
        seen: Mutex<Vec<(String, RetryPolicy, Bytes)>>,
        fail: Option<RpcError>,
    }

    impl Upstream for Recorder {
        fn send<'a>(
            &'a self,
            target: &'a Target,
            body: Bytes,
            policy: &'a RetryPolicy,
        ) -> BoxFuture<'a, Result<UpstreamResponse, RpcError>> {
            self.seen.lock().unwrap().push((
                target.name().to_string(),
                policy.clone(),
                body.clone(),
            ));
            let result = match &self.fail {
                Some(error) => Err(error.clone()),
                None => Ok(UpstreamResponse {
                    status: 200,
                    body: Bytes::from(format!(
                        r#"{{"jsonrpc":"2.0","id":1,"result":"0x{:x}"}}"#,
                        body.len()
                    )),
                    attempts: 1,
                    latency: Duration::ZERO,
                    retry_after: None,
                }),
            };
            Box::pin(async move { result })
        }
    }

    fn pinned() -> Target {
        Target::new(1, "pinned", "http://127.0.0.1:1", &[]).unwrap()
    }

    #[tokio::test]
    async fn forwards_keep_alive_requests_to_the_pinned_endpoint() {
        let recorder = Arc::new(Recorder {
            seen: Mutex::new(Vec::new()),
            fail: None,
        });
        let proxy = start(recorder.clone(), pinned()).await.unwrap();
        // RpcClient reuses one connection: two requests exercise keep-alive.
        let client = RpcClient::new();
        let through = Target::new(1, "proxy", &proxy.url(), &[]).unwrap();
        // The stub upstream answers with the forwarded body's length.
        let first = client.chain_id(&through).await.unwrap();
        let second = client.block_number(&through).await.unwrap();
        let seen = recorder.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(first, seen[0].2.len() as u64);
        assert_eq!(second, seen[1].2.len() as u64);
        assert!(
            std::str::from_utf8(&seen[1].2)
                .unwrap()
                .contains("eth_blockNumber")
        );
        for (name, policy, body) in seen.iter() {
            assert_eq!(name, "pinned");
            assert_eq!(policy, &RetryPolicy::same_endpoint_transient());
            assert!(std::str::from_utf8(body).unwrap().contains("\"method\""));
        }
    }

    #[tokio::test]
    async fn refuses_a_wrong_token_and_maps_upstream_errors() {
        let recorder = Arc::new(Recorder {
            seen: Mutex::new(Vec::new()),
            fail: Some(RpcError::RateLimited { retry_after: None }),
        });
        let proxy = start(recorder.clone(), pinned()).await.unwrap();
        let client = RpcClient::new();
        let wrong = format!("http://127.0.0.1:{}/wrong", proxy.port);
        let wrong = Target::new(1, "proxy", &wrong, &[]).unwrap();
        assert_eq!(
            client.chain_id(&wrong).await.unwrap_err(),
            RpcError::Http(404)
        );
        assert!(recorder.seen.lock().unwrap().is_empty(), "never forwarded");
        let through = Target::new(1, "proxy", &proxy.url(), &[]).unwrap();
        assert!(matches!(
            client.chain_id(&through).await.unwrap_err(),
            RpcError::RateLimited { .. }
        ));
        assert_eq!(error_status(&RpcError::Timeout), 504);
        assert_eq!(error_status(&RpcError::Connect), 502);
    }
}
