//! [`Mock`]: a JSON-RPC server on 127.0.0.1, one thread per connection,
//! keep-alive. It works the same from sync and async tests: nothing in it
//! runs on the test's runtime.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::ops::Index;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::http;

/// One HTTP request a [`Mock`] received.
#[derive(Debug, Clone)]
pub struct Request {
    /// Its place among the requests the mock recorded, from 0.
    pub index: usize,
    /// When it arrived.
    pub at: Instant,
    /// The request line and headers, lowercased.
    pub head: String,
    /// The request path.
    pub path: String,
    pub body: Vec<u8>,
}

impl Request {
    /// The body as JSON (`Null` when it is not JSON).
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The JSON-RPC calls in the body: one, or each of a batch.
    pub fn calls(&self) -> Vec<Value> {
        match self.json() {
            Value::Array(calls) => calls,
            call => vec![call],
        }
    }

    /// The method of every call in the body.
    pub fn methods(&self) -> Vec<String> {
        self.calls().iter().map(method_of).collect()
    }

    /// The method of a single (non-batch) call.
    pub fn method(&self) -> String {
        method_of(&self.json())
    }

    /// A header's value; the name is matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<String> {
        http::header(&self.head, name).map(str::to_string)
    }
}

/// One JSON-RPC call, for a [`Mock::start`] handler. Indexing reads the
/// call object itself: `call["params"][0]`.
#[derive(Debug, Clone)]
pub struct Call {
    pub method: String,
    pub params: Value,
    /// The HTTP request path it came on.
    pub path: String,
    /// The whole call object.
    pub raw: Value,
}

impl Index<&str> for Call {
    type Output = Value;
    fn index(&self, key: &str) -> &Value {
        &self.raw[key]
    }
}

/// How a [`Mock::start`] handler answers one call.
#[derive(Debug, Clone)]
pub enum Answer {
    /// `{"result": …}`.
    Result(Value),
    /// `{"error": {"code": …, "message": "mock error"}}`.
    Error(i64),
    /// `{"error": {"code": …, "message": …}}`.
    ErrorMessage(i64, String),
    /// The whole HTTP response is this status (no body), with an optional
    /// `Retry-After`.
    Http(u16, Option<&'static str>),
    /// Wait, then answer.
    Delay(Duration, Box<Answer>),
}

impl From<Value> for Answer {
    fn from(result: Value) -> Self {
        Answer::Result(result)
    }
}

/// `Err(message)` is a JSON-RPC error with code 3 (execution reverted).
impl From<Result<Value, String>> for Answer {
    fn from(reply: Result<Value, String>) -> Self {
        match reply {
            Ok(result) => Answer::Result(result),
            Err(message) => Answer::ErrorMessage(3, message),
        }
    }
}

/// `Err(code)` is a JSON-RPC error with that code.
impl From<Result<Value, i64>> for Answer {
    fn from(reply: Result<Value, i64>) -> Self {
        match reply {
            Ok(result) => Answer::Result(result),
            Err(code) => Answer::Error(code),
        }
    }
}

/// How a [`Mock::http`] handler answers one request.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// Wait this long before answering.
    pub delay: Duration,
    /// Promise more body than is sent, then close: a response cut off
    /// mid-body.
    pub cut: bool,
}

impl Reply {
    /// This status and no body.
    pub fn status(status: u16) -> Self {
        Reply {
            status,
            headers: Vec::new(),
            body: String::new(),
            delay: Duration::ZERO,
            cut: false,
        }
    }

    /// HTTP 200 with `body` as JSON.
    pub fn json(body: Value) -> Self {
        Reply::raw(200, &body.to_string())
    }

    /// This status and body, as given.
    pub fn raw(status: u16, body: &str) -> Self {
        Reply {
            body: body.to_string(),
            ..Reply::status(status)
        }
    }

    /// The JSON-RPC result of the single call in `request`.
    pub fn result(request: &Request, result: Value) -> Self {
        Reply::json(json!({"jsonrpc": "2.0", "id": request.json()["id"], "result": result}))
    }

    /// A JSON-RPC error for the single call in `request`.
    pub fn rpc_error(request: &Request, code: i64, message: &str) -> Self {
        Reply::json(json!({
            "jsonrpc": "2.0", "id": request.json()["id"],
            "error": {"code": code, "message": message}
        }))
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn cut(mut self) -> Self {
        self.cut = true;
        self
    }
}

type CallHandler = dyn Fn(&Call) -> Answer + Send + Sync;
type HttpHandler = dyn Fn(usize, &Request) -> Reply + Send + Sync;

enum Handler {
    Calls(Box<CallHandler>),
    Http(Box<HttpHandler>),
}

/// The connections a mock has open, so `kill` can close them.
#[derive(Default)]
struct Connections {
    stopped: bool,
    next: u64,
    open: HashMap<u64, TcpStream>,
}

struct Shared {
    port: u16,
    handler: Handler,
    /// Answer `[eth_chainId, eth_blockNumber]` batches (a gateway's health
    /// probe) from `head`, counted apart from requests.
    probes: bool,
    head: AtomicU64,
    probed: AtomicUsize,
    requests: Mutex<Vec<Request>>,
    connections: Mutex<Connections>,
}

impl Shared {
    fn connections(&self) -> MutexGuard<'_, Connections> {
        self.connections.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A JSON-RPC server on 127.0.0.1. Every request is recorded (see
/// [`Mock::requests`]) before it is answered. Dropping the mock stops it.
pub struct Mock {
    pub port: u16,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Mock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mock").field("port", &self.port).finish()
    }
}

impl Mock {
    /// Answer each call, alone or in a batch, with `handler`. A batch is
    /// answered as a batch, in order; an [`Answer::Http`] for any of its
    /// calls is the answer to the whole request.
    pub fn start(handler: impl Fn(&Call) -> Answer + Send + Sync + 'static) -> Mock {
        Mock::spawn(Handler::Calls(Box::new(handler)), false)
    }

    /// [`Mock::start`], also answering health probes: a batch of exactly
    /// `[eth_chainId, eth_blockNumber]` gets chain ID 1 and [`Mock::set_head`]'s
    /// head (100 to begin with). Probes are counted by [`Mock::probes`] and
    /// are not recorded as requests.
    pub fn start_probed(handler: impl Fn(&Call) -> Answer + Send + Sync + 'static) -> Mock {
        Mock::spawn(Handler::Calls(Box::new(handler)), true)
    }

    /// Answer each HTTP request with `handler(index, request)`, where
    /// `index` counts the requests before it.
    pub fn http(handler: impl Fn(usize, &Request) -> Reply + Send + Sync + 'static) -> Mock {
        Mock::spawn(Handler::Http(Box::new(handler)), false)
    }

    /// Answers `eth_chainId` with `chain_id`, and nothing else.
    pub fn chain_id(chain_id: u64) -> Mock {
        Mock::start(move |call| match call.method.as_str() {
            "eth_chainId" => Answer::Result(json!(format!("{chain_id:#x}"))),
            _ => Answer::ErrorMessage(-32601, "method not found".into()),
        })
    }

    fn spawn(handler: Handler, probes: bool) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Shared {
            port,
            handler,
            probes,
            head: AtomicU64::new(100),
            probed: AtomicUsize::new(0),
            requests: Mutex::default(),
            connections: Mutex::default(),
        });
        let accept = {
            let shared = shared.clone();
            std::thread::spawn(move || accept_loop(&listener, &shared))
        };
        Mock {
            port,
            shared,
            accept: Some(accept),
        }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every request so far, probes aside, in arrival order.
    pub fn requests(&self) -> Vec<Request> {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// How many requests so far, probes aside.
    pub fn hits(&self) -> usize {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Every call so far (batches flattened), probes aside.
    pub fn calls(&self) -> Vec<Value> {
        self.requests().iter().flat_map(Request::calls).collect()
    }

    /// The method of every call so far (batches flattened), probes aside.
    pub fn methods(&self) -> Vec<String> {
        self.calls().iter().map(method_of).collect()
    }

    /// The head of every request so far, probes aside.
    pub fn heads(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.head).collect()
    }

    /// When each request so far arrived, probes aside.
    pub fn arrivals(&self) -> Vec<Instant> {
        self.requests().into_iter().map(|r| r.at).collect()
    }

    /// How many health probes were answered (see [`Mock::start_probed`]).
    pub fn probes(&self) -> usize {
        self.shared.probed.load(Ordering::SeqCst)
    }

    /// The block number probes report from now on.
    pub fn set_head(&self, head: u64) {
        self.shared.head.store(head, Ordering::SeqCst);
    }

    /// Stop: once this returns, the port refuses connections and every
    /// connection is closed, so no answer can arrive after it.
    pub fn kill(&mut self) {
        let Some(accept) = self.accept.take() else {
            return;
        };
        {
            let mut connections = self.shared.connections();
            connections.stopped = true;
            for stream in connections.open.values() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        // Wake the accept loop so it sees `stopped` and drops the listener.
        let _ = TcpStream::connect(("127.0.0.1", self.shared.port));
        let _ = accept.join();
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.kill();
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let mut connections = shared.connections();
        if connections.stopped {
            return;
        }
        let Ok(clone) = stream.try_clone() else {
            continue;
        };
        let id = connections.next;
        connections.next += 1;
        connections.open.insert(id, clone);
        drop(connections);
        let shared = shared.clone();
        std::thread::spawn(move || {
            serve(stream, &shared);
            shared.connections().open.remove(&id);
        });
    }
}

fn serve(stream: TcpStream, shared: &Shared) {
    let mut reader = BufReader::new(stream);
    while let Some((head, body)) = http::read_message(&mut reader) {
        let close = http::header(&head, "connection").is_some_and(|v| v == "close");
        let reply = answer(shared, head, body);
        if shared.connections().stopped {
            return;
        }
        let mut message = format!(
            "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            reply.status,
            reply.body.len() + if reply.cut { 100 } else { 0 }
        );
        for (name, value) in &reply.headers {
            message.push_str(&format!("{name}: {value}\r\n"));
        }
        message.push_str("\r\n");
        message.push_str(&reply.body);
        let stream = reader.get_mut();
        if stream.write_all(message.as_bytes()).is_err() || reply.cut || close {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
    }
}

fn answer(shared: &Shared, head: String, body: Vec<u8>) -> Reply {
    let path = head
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if shared.probes && is_probe(&json) {
        shared.probed.fetch_add(1, Ordering::SeqCst);
        let head = shared.head.load(Ordering::SeqCst);
        return Reply::json(json!([
            {"jsonrpc": "2.0", "id": json[0]["id"], "result": "0x1"},
            {"jsonrpc": "2.0", "id": json[1]["id"], "result": format!("{head:#x}")},
        ]));
    }
    let request = {
        let mut requests = shared.requests.lock().unwrap_or_else(|e| e.into_inner());
        let request = Request {
            index: requests.len(),
            at: Instant::now(),
            head,
            path: path.clone(),
            body,
        };
        requests.push(request.clone());
        request
    };
    let handler = match &shared.handler {
        Handler::Http(handler) => {
            let reply = handler(request.index, &request);
            std::thread::sleep(reply.delay);
            return reply;
        }
        Handler::Calls(handler) => handler,
    };
    let calls = match &json {
        Value::Array(calls) => calls.clone(),
        call => vec![call.clone()],
    };
    let mut bodies = Vec::new();
    for raw in calls {
        let call = Call {
            method: method_of(&raw),
            params: raw["params"].clone(),
            path: path.clone(),
            raw,
        };
        let mut answer = handler(&call);
        while let Answer::Delay(wait, inner) = answer {
            std::thread::sleep(wait);
            answer = *inner;
        }
        let id = &call.raw["id"];
        bodies.push(match answer {
            Answer::Result(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Answer::Error(code) => json!({"jsonrpc": "2.0", "id": id,
                                          "error": {"code": code, "message": "mock error"}}),
            Answer::ErrorMessage(code, message) => json!({"jsonrpc": "2.0", "id": id,
                                          "error": {"code": code, "message": message}}),
            Answer::Http(status, retry_after) => {
                let reply = Reply::status(status);
                return match retry_after {
                    Some(seconds) => reply.header("Retry-After", seconds),
                    None => reply,
                };
            }
            Answer::Delay(..) => unreachable!(),
        });
    }
    if json.is_array() {
        Reply::json(Value::Array(bodies))
    } else {
        Reply::json(bodies.pop().unwrap_or(Value::Null))
    }
}

fn is_probe(json: &Value) -> bool {
    json.as_array().is_some_and(|calls| {
        calls.len() == 2
            && calls[0]["method"] == "eth_chainId"
            && calls[1]["method"] == "eth_blockNumber"
    })
}

fn method_of(call: &Value) -> String {
    call["method"].as_str().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http;

    #[test]
    fn batches_are_answered_call_by_call_and_recorded() {
        let mock = Mock::start(|call| match call.method.as_str() {
            "a" => Answer::Result(json!(call["params"][0])),
            _ => Answer::Error(-32601),
        });
        let body = json!([
            {"jsonrpc": "2.0", "id": 7, "method": "a", "params": [1]},
            {"jsonrpc": "2.0", "id": 8, "method": "b", "params": []},
        ]);
        let reply = http::send(
            &format!("127.0.0.1:{}", mock.port),
            "POST",
            "/x",
            &[],
            &body.to_string(),
        )
        .unwrap();
        let answers = reply.json();
        assert_eq!(answers[0]["id"], 7);
        assert_eq!(answers[0]["result"], 1);
        assert_eq!(answers[1]["error"]["code"], -32601);
        assert_eq!(mock.methods(), ["a", "b"]);
        assert_eq!(mock.requests()[0].path, "/x");
    }

    #[test]
    fn probes_are_answered_and_counted_apart() {
        let mock = Mock::start_probed(|_| Answer::Result(json!("x")));
        mock.set_head(0x2a);
        let probe = json!([
            {"jsonrpc": "2.0", "id": 0, "method": "eth_chainId"},
            {"jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber"},
        ]);
        let reply = http::send(
            &format!("127.0.0.1:{}", mock.port),
            "POST",
            "/",
            &[],
            &probe.to_string(),
        )
        .unwrap();
        assert_eq!(reply.json()[1]["result"], "0x2a");
        assert_eq!((mock.probes(), mock.hits()), (1, 0));
    }

    #[test]
    fn a_killed_mock_refuses_connections() {
        let mut mock = Mock::start(|_| Answer::Result(json!(1)));
        let url = mock.url();
        assert_eq!(http::rpc(&url, "m", json!([])), 1);
        mock.kill();
        assert!(TcpStream::connect(("127.0.0.1", mock.port)).is_err());
        assert!(http::try_rpc(&url, "m", json!([])).is_none());
    }
}
