//! Test support: a local mock JSON-RPC server and hand-built plans.

use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chainz_core::Address;
use chainz_core::route::Route;
use chainz_core::runtime::Runtime;
use chainz_rpc::HealthStore;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::{Event, Shared, Tuning};
use crate::model::Update;
use crate::plan::{ChainInfo, ChainPlan, EndpointPlan, Plan, SignerPlan, Source, TokenPlan};

/// What the mock answers to one HTTP request.
pub(crate) enum Reply {
    /// HTTP 200 with this JSON body.
    Json(Value),
    /// A bare HTTP status.
    Status(u16),
    /// HTTP 429 with `Retry-After` in seconds.
    RateLimited(u64),
}

/// One request the mock received.
#[derive(Debug, Clone)]
pub(crate) struct Hit {
    pub(crate) at: Instant,
    pub(crate) path: String,
    pub(crate) body: Value,
}

impl Hit {
    /// The JSON-RPC methods in the request (one, or a batch's).
    pub(crate) fn methods(&self) -> Vec<String> {
        let method = |v: &Value| v["method"].as_str().unwrap_or_default().to_string();
        match &self.body {
            Value::Array(calls) => calls.iter().map(method).collect(),
            call => vec![method(call)],
        }
    }
}

/// A JSON-RPC server on a loopback port.
pub(crate) struct MockRpc {
    pub(crate) url: String,
    hits: Arc<Mutex<Vec<Hit>>>,
    task: JoinHandle<()>,
}

impl Drop for MockRpc {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockRpc {
    /// Serve with `handler`, which sees each request's whole JSON body.
    pub(crate) async fn start(handler: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits: Arc<Mutex<Vec<Hit>>> = Arc::default();
        let handler = Arc::new(handler);
        let recorded = hits.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    continue;
                };
                let (handler, recorded) = (handler.clone(), recorded.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let (head_len, body_len) = loop {
                        let mut chunk = [0u8; 4096];
                        let Ok(n) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (end + 4, len);
                        }
                    };
                    while buf.len() < head_len + body_len {
                        let mut chunk = [0u8; 4096];
                        match stream.read(&mut chunk).await {
                            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                            _ => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..head_len]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let body: Value = serde_json::from_slice(&buf[head_len..head_len + body_len])
                        .unwrap_or(Value::Null);
                    let reply = handler(&body);
                    recorded.lock().unwrap().push(Hit {
                        at: Instant::now(),
                        path,
                        body,
                    });
                    let response = match reply {
                        Reply::Json(value) => {
                            let body = value.to_string();
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        }
                        Reply::Status(code) => format!(
                            "HTTP/1.1 {code} Mock\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        ),
                        Reply::RateLimited(secs) => format!(
                            "HTTP/1.1 429 Mock\r\nretry-after: {secs}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        ),
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        MockRpc { url, hits, task }
    }

    /// Serve by answering each call (alone or in a batch) with `call`:
    /// `Ok(result)` or `Err(json_rpc_error_code)`.
    pub(crate) async fn calls(
        call: impl Fn(&str, &Value) -> Result<Value, i64> + Send + Sync + 'static,
    ) -> Self {
        Self::start(move |body| Reply::Json(answer(body, &call))).await
    }

    /// A chain whose head never moves: block `head` at time `timestamp`,
    /// and every earlier block `block_time` seconds before the next.
    pub(crate) async fn chain(head: u64, timestamp: u64, block_time: f64) -> Self {
        Self::calls(move |method, params| match method {
            "eth_getBlockByNumber" => Ok(block(head, timestamp, block_time, &params[0])),
            "eth_chainId" => Ok(json!("0x1")),
            "eth_blockNumber" => Ok(json!(format!("{head:#x}"))),
            _ => Err(-32601),
        })
        .await
    }

    pub(crate) fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }

    /// Every method received, in order, batches flattened.
    pub(crate) fn methods(&self) -> Vec<String> {
        self.hits().iter().flat_map(Hit::methods).collect()
    }
}

/// A block object for `eth_getBlockByNumber(tag)` on a static chain.
pub(crate) fn block(head: u64, timestamp: u64, block_time: f64, tag: &Value) -> Value {
    let number = match tag.as_str() {
        Some("latest") | None => head,
        Some(hex) => u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap(),
    };
    let behind = (head - number) as f64 * block_time;
    json!({
        "number": format!("{number:#x}"),
        "timestamp": format!("{:#x}", timestamp - behind as u64),
        "baseFeePerGas": "0x186a0",
    })
}

/// Answer a request body (single or batch) call by call.
pub(crate) fn answer(body: &Value, call: &dyn Fn(&str, &Value) -> Result<Value, i64>) -> Value {
    let one = |request: &Value| {
        let id = request["id"].clone();
        match call(
            request["method"].as_str().unwrap_or_default(),
            &request["params"],
        ) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(code) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": "mock error"},
            }),
        }
    };
    match body {
        Value::Array(requests) => Value::Array(requests.iter().map(one).collect()),
        request => one(request),
    }
}

/// ABI-encode `(bool success, bytes returnData)[]` where each successful
/// entry returns one `uint256`, as `aggregate3` does.
pub(crate) fn aggregate3_result(values: &[Option<u128>]) -> String {
    let word = |n: u128| format!("{n:064x}");
    let mut out = word(32) + &word(values.len() as u128);
    let mut offset = values.len() as u128 * 32;
    let mut items = String::new();
    for value in values {
        out += &word(offset);
        match value {
            Some(v) => {
                items += &(word(1) + &word(64) + &word(32) + &word(*v));
                offset += 4 * 32;
            }
            None => {
                items += &(word(0) + &word(64) + &word(0));
                offset += 3 * 32;
            }
        }
    }
    format!("0x{out}{items}")
}

pub(crate) const HOLDER_A: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
pub(crate) const HOLDER_B: &str = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF";
pub(crate) const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
pub(crate) const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

pub(crate) fn address(s: &str) -> Address {
    s.parse().unwrap()
}

/// An endpoint plan for `url` (no templates, no headers).
pub(crate) fn endpoint(chain_id: u64, name: &str, url: &str) -> EndpointPlan {
    let endpoint = chainz_core::model::Endpoint::new(url);
    let route = Route {
        chain_id,
        chain: String::new(),
        endpoint: name.into(),
        url: url.into(),
        headers: Vec::new(),
        display: String::new(),
        via_gateway: false,
        websocket: endpoint.is_websocket(),
    };
    EndpointPlan::new(name, &endpoint, Ok(route), false)
}

/// Make `endpoint` one whose config does not resolve, for `why`.
pub(crate) fn invalidate(endpoint: &mut EndpointPlan, why: &str) {
    endpoint.target = None;
    endpoint.info.invalid = Some(why.into());
}

/// A chain plan whose first endpoint is active, with WETH and USDC.
pub(crate) fn chain(name: &str, id: u64, endpoints: &[(&str, &str)]) -> ChainPlan {
    let token = |name: &str, symbol: &str, decimals, at: &str| TokenPlan {
        name: name.into(),
        symbol: symbol.into(),
        decimals,
        address: address(at),
    };
    let info = ChainInfo {
        name: name.into(),
        id,
        aliases: Vec::new(),
        active: endpoints[0].0.into(),
        tokens: vec![
            token("usdc", "USDC", 6, USDC),
            token("weth", "WETH", 18, WETH),
        ],
        native_symbol: "ETH",
    };
    let endpoints = endpoints
        .iter()
        .map(|(ep, url)| endpoint(id, ep, url))
        .collect();
    ChainPlan {
        source: Source::named(name),
        ..ChainPlan::new(info, endpoints)
    }
}

/// A plan over `chains` with two signers that have addresses and one
/// that does not.
pub(crate) fn plan(chains: Vec<ChainPlan>) -> Arc<Plan> {
    let signer = |name: &str, at: Option<&str>| SignerPlan {
        name: name.into(),
        address: at.map(address),
    };
    Arc::new(Plan {
        chains,
        signers: vec![
            signer("default", Some(HOLDER_A)),
            signer("funder", Some(HOLDER_B)),
            signer("cold", None),
        ],
    })
}

/// A plan over `chains` with no signers: nothing reads balances.
pub(crate) fn chains_only(chains: Vec<ChainPlan>) -> Arc<Plan> {
    Arc::new(Plan {
        chains,
        signers: Vec::new(),
    })
}

/// Intervals short enough for a test, with no start-up stagger.
pub(crate) fn fast() -> Tuning {
    Tuning {
        min_pace: Duration::from_millis(100),
        max_pace: Duration::from_millis(600),
        error_pace: Duration::from_millis(300),
        request_timeout: Duration::from_secs(2),
        block_span: 200,
        probe_every: Duration::from_millis(400),
        balance_every: Duration::from_millis(400),
        traffic_every: Duration::from_millis(100),
        watch_every: Duration::from_millis(100),
        stagger: Duration::ZERO,
    }
}

/// Worker state rooted in `dir` (runtime dir and state dir), and the
/// receiving end of its events.
pub(crate) fn shared(dir: &Path, tuning: Tuning) -> (Arc<Shared>, Receiver<Event>) {
    let (sender, receiver) = channel();
    let shared = Shared::new(
        HealthStore::load(dir).unwrap(),
        Runtime::at(dir.join("runtime")),
        tuning,
        sender,
    );
    (shared, receiver)
}

/// The updates among what is waiting in `events`.
pub(crate) fn updates(events: &Receiver<Event>) -> impl Iterator<Item = Update> + '_ {
    events.try_iter().filter_map(|event| match event {
        Event::Update(update) => Some(update),
        _ => None,
    })
}

/// A temp dir directly under `/tmp`. A unix socket path must fit in
/// `sun_path` (108 bytes on Linux, 104 on macOS), and `$TMPDIR` can be
/// long enough on its own (nix shells, macOS) to push one past it.
pub(crate) fn short_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let longest = dir.path().join("runtime/serve.sock");
    let len = longest.as_os_str().len();
    assert!(
        len < 100,
        "socket path {} is {len} bytes, past the unix socket limit",
        longest.display()
    );
    dir
}

/// Wait (up to ten seconds) until `done` holds.
pub(crate) async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait (up to five seconds) for an update that `pick` accepts.
pub(crate) async fn wait_for<T>(
    events: &Receiver<Event>,
    mut pick: impl FnMut(Update) -> Option<T>,
) -> T {
    wait_for_event(events, |event| match event {
        Event::Update(update) => pick(update),
        _ => None,
    })
    .await
}

/// Wait (up to five seconds) for an event that `pick` accepts.
pub(crate) async fn wait_for_event<T>(
    events: &Receiver<Event>,
    mut pick: impl FnMut(Event) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        while let Ok(event) = events.try_recv() {
            if let Some(found) = pick(event) {
                return found;
            }
        }
        assert!(Instant::now() < deadline, "no matching update in 5 s");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
