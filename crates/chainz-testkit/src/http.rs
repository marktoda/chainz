//! A raw HTTP/1.1 client: one request per connection (`Connection:
//! close`), headers exactly as given. For talking to a gateway, a fork's
//! proxy or anvil the way a non-Rust client would.

use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// A response.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    /// The status line and headers, lowercased.
    pub head: String,
    pub body: String,
}

impl Response {
    /// The body as JSON (`Null` when it is not JSON).
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// A header's value; the name is matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.head, name)
    }
}

/// `method path` to `authority` (`host:port`) with `headers` and `body`.
/// `Host` and `Content-Length` are added unless `headers` has them.
/// `None` if the connection fails or no complete response comes back
/// within 30 s.
pub fn send(
    authority: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Option<Response> {
    let mut stream = TcpStream::connect(authority).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    stream
        .write_all(request(authority, method, path, headers, body).as_bytes())
        .ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    parse(&raw)
}

/// [`send`] on a tokio socket, for tests whose server runs on the same
/// runtime. Panics if the connection fails.
pub async fn send_async(
    authority: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
    stream
        .write_all(request(authority, method, path, headers, body).as_bytes())
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    parse(&raw).unwrap_or_else(|| panic!("no HTTP response: {raw:?}"))
}

/// POST a JSON-RPC call to `url` (`http://host:port/path`) and return the
/// whole response object; `None` if nothing (or not JSON) came back.
pub fn try_rpc(url: &str, method: &str, params: Value) -> Option<Value> {
    let rest = url.strip_prefix("http://").expect("an http:// URL");
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let response = send(
        authority,
        "POST",
        &format!("/{path}"),
        &[("Content-Type", "application/json")],
        &body.to_string(),
    )?;
    serde_json::from_str(&response.body).ok()
}

/// [`try_rpc`], requiring a `result`; returns it.
#[track_caller]
pub fn rpc(url: &str, method: &str, params: Value) -> Value {
    let response = try_rpc(url, method, params.clone())
        .unwrap_or_else(|| panic!("{method} on {url} got no answer"));
    assert!(
        response.get("error").is_none(),
        "{method}{params}: {response}"
    );
    response["result"].clone()
}

fn request(
    authority: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> String {
    let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    let mut out = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !has("host") {
        out.push_str(&format!("Host: {authority}\r\n"));
    }
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has("content-length") {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("\r\n");
    out.push_str(body);
    out
}

fn parse(raw: &[u8]) -> Option<Response> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    Some(Response {
        status,
        body: String::from_utf8_lossy(&raw[end + 4..]).into_owned(),
        head,
    })
}

/// A header's value in a lowercased `head`.
pub(crate) fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then_some(value.trim())
    })
}

/// Read one HTTP message off a keep-alive stream: the head (request or
/// status line and headers, lowercased) and the body. `None` at EOF.
pub(crate) fn read_message(reader: &mut impl BufRead) -> Option<(String, Vec<u8>)> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.trim_end().is_empty() {
            if head.is_empty() {
                continue;
            }
            break;
        }
        head.push_str(&line.to_ascii_lowercase());
    }
    let length: usize = header(&head, "content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some((head.trim_end().to_string(), body))
}
