//! Test support: a local mock JSON-RPC server and hand-built plans.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use chainz_core::Address;
use chainz_core::route::Route;
use chainz_core::runtime::Runtime;
use chainz_rpc::HealthStore;
use serde_json::{Value, json};

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

/// A JSON-RPC server on a loopback port: a [`chainz_testkit::Mock`]
/// answering whole request bodies.
pub(crate) struct MockRpc {
    pub(crate) url: String,
    mock: chainz_testkit::Mock,
}

impl MockRpc {
    /// Serve with `handler`, which sees each request's whole JSON body.
    pub(crate) async fn start(handler: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Self {
        let mock = chainz_testkit::Mock::http(move |_, request| match handler(&request.json()) {
            Reply::Json(value) => chainz_testkit::Reply::json(value),
            Reply::Status(code) => chainz_testkit::Reply::status(code),
            Reply::RateLimited(secs) => {
                chainz_testkit::Reply::status(429).header("Retry-After", &secs.to_string())
            }
        });
        MockRpc {
            url: mock.url(),
            mock,
        }
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
        self.mock
            .requests()
            .into_iter()
            .map(|r| Hit {
                at: r.at,
                body: r.json(),
                path: r.path,
            })
            .collect()
    }

    /// Every method received, in order, batches flattened.
    pub(crate) fn methods(&self) -> Vec<String> {
        self.mock.methods()
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

pub(crate) use chainz_testkit::abi::aggregate3_result;

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
        once_retry_wait: Duration::from_millis(50),
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

pub(crate) use chainz_testkit::short_tempdir;

/// Wait (up to ten seconds) until `done` holds.
pub(crate) async fn wait_until(what: &str, done: impl FnMut() -> bool) {
    chainz_testkit::wait_until_async(what, Duration::from_secs(10), done).await;
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
