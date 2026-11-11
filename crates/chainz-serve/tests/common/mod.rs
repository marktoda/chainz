//! Hermetic test rig: scripted mock upstreams, a sandboxed config, a raw
//! HTTP client and a gateway started in-process.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chainz_core::StorePaths;
use chainz_core::runtime::Runtime;
use chainz_rpc::control::ControlClient;
use chainz_serve::{Gateway, GatewayConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// What a mock answers to one call.
#[derive(Debug, Clone)]
pub enum R {
    /// `{"result": …}`.
    Result(Value),
    /// `{"error": {"code": …}}`.
    Error(i64),
    /// A bare HTTP status, with an optional `Retry-After`.
    Http(u16, Option<&'static str>),
    /// Wait, then answer.
    Delay(Duration, Box<R>),
}

type Handler = dyn Fn(&Value) -> R + Send + Sync;

/// A scripted JSON-RPC upstream on 127.0.0.1. Health probes
/// (`[eth_chainId, eth_blockNumber]`) are answered from `head` and counted
/// separately from requests.
pub struct Mock {
    pub name: &'static str,
    port: u16,
    calls: Arc<Mutex<Vec<Value>>>,
    heads: Arc<Mutex<Vec<String>>>,
    arrivals: Arc<Mutex<Vec<Instant>>>,
    probes: Arc<AtomicUsize>,
    head: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl Mock {
    /// A mock that answers every call with its own name.
    pub async fn named(name: &'static str) -> Mock {
        Mock::start(name, move |_| R::Result(json!(name))).await
    }

    pub async fn start(
        name: &'static str,
        handler: impl Fn(&Value) -> R + Send + Sync + 'static,
    ) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let probes = Arc::new(AtomicUsize::new(0));
        let head = Arc::new(AtomicU64::new(100));
        let handler: Arc<Handler> = Arc::new(handler);
        let heads = Arc::new(Mutex::new(Vec::new()));
        let arrivals = Arc::new(Mutex::new(Vec::new()));
        let seen = Seen {
            calls: calls.clone(),
            heads: heads.clone(),
            arrivals: arrivals.clone(),
            probes: probes.clone(),
            head: head.clone(),
        };
        let task = tokio::spawn(async move {
            // Owned here: aborting this task drops every connection.
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else {
                            // Don't spin on a persistent accept error.
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            continue;
                        };
                        connections.spawn(connection(stream, handler.clone(), seen.clone()));
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Mock {
            name,
            port,
            calls,
            heads,
            arrivals,
            probes,
            head,
            task,
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every non-probe call received, in order.
    pub fn calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().clone()
    }

    /// The HTTP head (request line and headers, lowercased) of every
    /// non-probe request received.
    pub fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }

    /// When each non-probe request arrived, in order.
    pub fn arrivals(&self) -> Vec<Instant> {
        self.arrivals.lock().unwrap().clone()
    }

    /// Methods of every non-probe call received.
    pub fn methods(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|c| c["method"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    pub fn probes(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }

    pub fn set_head(&self, head: u64) {
        self.head.store(head, Ordering::SeqCst);
    }

    /// Stop serving and wait until the listener and every connection are
    /// dropped. `abort` alone only requests cancellation: on a multi-thread
    /// runtime the task can still accept one more call before it stops.
    pub async fn kill(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What a mock records, shared with its connections.
#[derive(Clone)]
struct Seen {
    calls: Arc<Mutex<Vec<Value>>>,
    heads: Arc<Mutex<Vec<String>>>,
    arrivals: Arc<Mutex<Vec<Instant>>>,
    probes: Arc<AtomicUsize>,
    head: Arc<AtomicU64>,
}

async fn connection(mut stream: TcpStream, handler: Arc<Handler>, seen: Seen) {
    let Seen {
        calls,
        heads,
        arrivals,
        probes,
        head,
    } = seen;
    let mut buf = Vec::new();
    loop {
        let Some((request_head, body)) = read_request(&mut stream, &mut buf).await else {
            return;
        };
        let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let is_probe = request.as_array().is_some_and(|a| {
            a.len() == 2 && a[0]["method"] == "eth_chainId" && a[1]["method"] == "eth_blockNumber"
        });
        let response = if is_probe {
            probes.fetch_add(1, Ordering::SeqCst);
            let head = format!("{:#x}", head.load(Ordering::SeqCst));
            http_response(
                200,
                None,
                &json!([
                    {"jsonrpc":"2.0","id":0,"result":"0x1"},
                    {"jsonrpc":"2.0","id":1,"result":head},
                ])
                .to_string(),
            )
        } else {
            let items: Vec<Value> = match &request {
                Value::Array(items) => items.clone(),
                one => vec![one.clone()],
            };
            arrivals.lock().unwrap().push(Instant::now());
            calls.lock().unwrap().extend(items.iter().cloned());
            heads.lock().unwrap().push(request_head);
            let mut bodies = Vec::new();
            let mut http = None;
            for item in &items {
                let mut reply = handler(item);
                while let R::Delay(wait, inner) = reply {
                    tokio::time::sleep(wait).await;
                    reply = *inner;
                }
                match reply {
                    R::Result(result) => {
                        bodies.push(json!({"jsonrpc":"2.0","id":item["id"],"result":result}));
                    }
                    R::Error(code) => bodies.push(json!({
                        "jsonrpc":"2.0","id":item["id"],
                        "error":{"code":code,"message":"mock error"}
                    })),
                    R::Http(status, retry_after) => http = Some((status, retry_after)),
                    R::Delay(..) => unreachable!(),
                }
            }
            match http {
                Some((status, retry_after)) => http_response(status, retry_after, ""),
                None if request.is_array() => {
                    http_response(200, None, &Value::Array(bodies).to_string())
                }
                None => http_response(200, None, &bodies[0].to_string()),
            }
        };
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn http_response(status: u16, retry_after: Option<&str>, body: &str) -> String {
    let retry = retry_after
        .map(|v| format!("Retry-After: {v}\r\n"))
        .unwrap_or_default();
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{retry}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Read one request off a keep-alive connection; `None` at EOF.
async fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            let total = end + 4 + length;
            while buf.len() < total {
                let mut chunk = [0u8; 16 * 1024];
                let n = stream.read(&mut chunk).await.ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = buf[end + 4..total].to_vec();
            buf.drain(..total);
            return Some((head, body));
        }
        let mut chunk = [0u8; 16 * 1024];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// A raw HTTP response.
#[derive(Debug)]
pub struct HttpReply {
    pub status: u16,
    /// The header block, lowercased.
    pub head: String,
    pub body: String,
}

impl HttpReply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// One raw HTTP/1.1 request to 127.0.0.1:`port` (`Connection: close`).
pub async fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> HttpReply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    HttpReply {
        status: head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        head: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

/// A sandboxed config plus a gateway running against it.
pub struct Rig {
    pub dir: tempfile::TempDir,
    pub gateway: Option<Gateway>,
    pub port: u16,
}

impl Rig {
    pub fn paths(dir: &Path) -> StorePaths {
        StorePaths::in_dir(dir.join("config"))
    }

    pub fn runtime_at(dir: &Path) -> Runtime {
        Runtime::at(dir.join("runtime"))
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    pub fn runtime(&self) -> Runtime {
        Rig::runtime_at(self.dir.path())
    }

    /// Write `config.toml` (atomically, like chainz does).
    pub fn write_config(dir: &Path, toml: &str) {
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
        let tmp = config.join("config.toml.tmp");
        std::fs::write(&tmp, toml).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::rename(&tmp, config.join("config.toml")).unwrap();
    }

    /// The default test settings: fast probes and config checks.
    pub fn config(dir: &Path) -> GatewayConfig {
        let mut config = GatewayConfig::new(
            Rig::paths(dir),
            Rig::runtime_at(dir),
            Some(dir.join("state")),
        );
        config.port = 0;
        config.timeout = Duration::from_secs(2);
        config.probe_interval = Duration::from_millis(50);
        config.watch_interval = Duration::from_millis(50);
        config
    }

    pub async fn start(toml: &str) -> Rig {
        Rig::start_with(toml, |_| {}).await
    }

    pub async fn start_with(toml: &str, tweak: impl FnOnce(&mut GatewayConfig)) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        Rig::write_config(dir.path(), toml);
        let mut config = Rig::config(dir.path());
        tweak(&mut config);
        let gateway = Gateway::start(config).await.unwrap();
        let port = gateway.port();
        Rig {
            dir,
            gateway: Some(gateway),
            port,
        }
    }

    pub fn gateway(&self) -> &Gateway {
        self.gateway.as_ref().unwrap()
    }

    /// The path part of the gateway URL for `chain`.
    pub fn path(&self, chain: &str, endpoint: Option<&str>) -> String {
        let url = self.gateway().url(chain, endpoint);
        url.split_once(&format!(":{}", self.port))
            .unwrap()
            .1
            .to_string()
    }

    pub fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// POST a JSON body the way a well-behaved client does.
    pub async fn post(&self, path: &str, body: &Value) -> HttpReply {
        http(
            self.port,
            "POST",
            path,
            &[("Host", &self.host()), ("Content-Type", "application/json")],
            &body.to_string(),
        )
        .await
    }

    /// One JSON-RPC call to `chain`.
    pub async fn call(&self, chain: &str, method: &str, params: Value) -> HttpReply {
        self.post(
            &self.path(chain, None),
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )
        .await
    }

    pub fn control(&self) -> ControlClient {
        ControlClient::new(self.runtime().serve_sock_path())
    }

    /// Wait until `chain`'s head high-water mark is at least `head` and
    /// the gateway holds a current head for each of `pool` (the mocks it
    /// probes, all already serving the head waited for).
    pub async fn wait_for_head(&self, chain: &str, head: u64, pool: &[&Mock]) {
        let start: Vec<usize> = pool.iter().map(|m| m.probes()).collect();
        eventually(|| {
            self.gateway()
                .status()
                .chains
                .iter()
                .any(|c| c.chain == chain && c.head.is_some_and(|h| h >= head))
        })
        .await;
        // Two more probes of each: the first was received after the call
        // began, so it saw the new head, and a probe round only starts
        // once the previous round's results are stored.
        probed_twice_since(pool, &start).await;
    }

    pub async fn shutdown(&mut self) {
        if let Some(gateway) = self.gateway.take() {
            gateway.shutdown().await;
        }
    }
}

/// Wait until each of `mocks` has been probed at least twice more than
/// `start` (their probe counts at some earlier point, in order).
pub async fn probed_twice_since(mocks: &[&Mock], start: &[usize]) {
    eventually(|| mocks.iter().zip(start).all(|(m, s)| m.probes() >= s + 2)).await;
}

/// Poll `condition` for up to 5 s.
pub async fn eventually(mut condition: impl FnMut() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met within 5 s");
}

/// A one-chain config: `eth` (ID 1) with the given active endpoint,
/// failover list and endpoints.
pub fn eth_config(active: &str, failover: &[&str], endpoints: &[(&str, String)]) -> String {
    let failover = failover
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut toml = format!(
        "version = 2\n[chains.eth]\nid = 1\naliases = [\"mainnet\"]\nrpc = \"{active}\"\nfailover = [{failover}]\n[chains.eth.rpcs]\n"
    );
    for (name, url) in endpoints {
        toml.push_str(&format!("{name} = \"{url}\"\n"));
    }
    toml
}
