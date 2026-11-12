//! A tiny scripted HTTP/1.1 mock server on 127.0.0.1 for hermetic tests.
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chainz_rpc::Target;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A secret embedded in every mock URL path, to check redaction.
pub const URL_SECRET: &str = "urlsekrit9f8e7d";
/// A secret header value, to check redaction.
pub const HEADER_SECRET: &str = "hdrsekrit1a2b3c";

#[derive(Debug, Clone)]
pub struct Req {
    pub head: String,
    pub body: Vec<u8>,
}

impl Req {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    /// The method of a single (non-batch) request.
    pub fn method(&self) -> String {
        self.json()["method"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    }
}

#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub delay: Duration,
    /// Promise more body than is sent, then close: a response cut off
    /// mid-body.
    pub cut: bool,
}

impl Resp {
    pub fn status(status: u16) -> Self {
        Resp {
            status,
            headers: vec![],
            body: String::new(),
            delay: Duration::ZERO,
            cut: false,
        }
    }
    pub fn json(body: Value) -> Self {
        Resp {
            body: body.to_string(),
            ..Resp::status(200)
        }
    }
    pub fn raw(status: u16, body: &str) -> Self {
        Resp {
            body: body.to_string(),
            ..Resp::status(status)
        }
    }
    /// `{"jsonrpc":"2.0","id":<id of req>,"result":result}`.
    pub fn result(req: &Req, result: Value) -> Self {
        Resp::json(serde_json::json!({"jsonrpc":"2.0","id":req.json()["id"],"result":result}))
    }
    pub fn rpc_error(req: &Req, code: i64, message: &str) -> Self {
        Resp::json(serde_json::json!({
            "jsonrpc":"2.0","id":req.json()["id"],"error":{"code":code,"message":message}
        }))
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn delay(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }
    pub fn cut(mut self) -> Self {
        self.cut = true;
        self
    }
}

type Handler = dyn Fn(usize, &Req) -> Resp + Send + Sync;

pub struct MockHttp {
    pub port: u16,
    hits: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Req>>>,
}

impl MockHttp {
    /// Serve with `handler(request_index, request)`.
    pub async fn start(handler: impl Fn(usize, &Req) -> Resp + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let (h, r) = (hits.clone(), requests.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (handler, hits, requests) = (handler.clone(), h.clone(), r.clone());
                tokio::spawn(async move {
                    let Some(req) = read_request(&mut stream).await else {
                        return;
                    };
                    let idx = hits.fetch_add(1, Ordering::SeqCst);
                    requests.lock().unwrap().push(req.clone());
                    let resp = handler(idx, &req);
                    tokio::time::sleep(resp.delay).await;
                    let mut out = format!(
                        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                        resp.status,
                        resp.body.len() + if resp.cut { 100 } else { 0 }
                    );
                    for (k, v) in &resp.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    out.push_str(&resp.body);
                    let _ = stream.write_all(out.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        MockHttp {
            port,
            hits,
            requests,
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v3/{URL_SECRET}", self.port)
    }

    pub fn target(&self, chain_id: u64) -> Target {
        Target::new(
            chain_id,
            "mock",
            &self.url(),
            &[("X-Api-Key".into(), HEADER_SECRET.into())],
        )
        .unwrap()
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }
}

/// A target on a local port nothing listens on.
pub fn dead_target() -> Target {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    Target::new(
        1,
        "dead",
        &format!("http://127.0.0.1:{port}/v3/{URL_SECRET}"),
        &[],
    )
    .unwrap()
}

/// Assert that neither Display nor Debug of `e` leaks a secret.
pub fn assert_redacted(e: &impl std::fmt::Debug, display: &str) {
    let dbg = format!("{e:?}");
    for text in [&dbg, display] {
        assert!(!text.contains(URL_SECRET), "URL leaked: {text}");
        assert!(!text.contains("127.0.0.1"), "host leaked: {text}");
        assert!(!text.contains(HEADER_SECRET), "header leaked: {text}");
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let len: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(Req {
        head,
        body: buf[head_end..head_end + len].to_vec(),
    })
}
