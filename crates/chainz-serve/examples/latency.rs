//! Gateway overhead benchmark: the same JSON-RPC call sent straight to a
//! local mock upstream and through the gateway, interleaved, over
//! keep-alive connections.
//!
//! ```text
//! cargo run --release -p chainz-serve --example latency [-- ROUNDS]
//! ```
//!
//! The budget (docs/architecture.md, Performance) is under 1 ms of added p50 latency.

use std::time::Duration;
use std::time::Instant;

use chainz_core::StorePaths;
use chainz_core::runtime::Runtime;
use chainz_rpc::health::percentile;
use chainz_rpc::{RpcClient, Target};
use chainz_serve::{Gateway, GatewayConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(5000);
    let upstream = mock_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let toml = format!(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = \"{upstream}\"\n"
    );
    let config_file = config_dir.join("config.toml");
    std::fs::write(&config_file, toml).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut config = GatewayConfig::new(
        StorePaths::in_dir(config_dir),
        Runtime::at(dir.path().join("runtime")),
        Some(dir.path().join("state")),
    );
    config.port = 0;
    let gateway = Gateway::start(config).await.unwrap();

    let client = RpcClient::new();
    let direct = Target::new(1, "direct", &upstream, &[]).unwrap();
    let through = Target::new(1, "gateway", &gateway.addr().url("eth", None), &[]).unwrap();
    let params = json!([{"to": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
        "data": "0x70a08231000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"}, "latest"]);

    let call = |target: &Target| {
        let client = client.clone();
        let target = target.clone();
        let params = params.clone();
        async move {
            let started = Instant::now();
            let _: Value = client.request(&target, "eth_call", params).await.unwrap();
            started.elapsed()
        }
    };
    for _ in 0..200 {
        call(&direct).await;
        call(&through).await;
    }
    let mut direct_times = Vec::with_capacity(rounds);
    let mut gateway_times = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        direct_times.push(call(&direct).await);
        gateway_times.push(call(&through).await);
    }
    gateway.shutdown().await;

    let us = |d: Duration| d.as_secs_f64() * 1e6;
    println!("{rounds} sequential eth_call round trips each, interleaved, loopback");
    for (name, times) in [
        ("direct", &mut direct_times),
        ("gateway", &mut gateway_times),
    ] {
        println!(
            "{name:>8}: p50 {:7.1} us   p95 {:7.1} us   p99 {:7.1} us",
            us(percentile(times, 50)),
            us(percentile(times, 95)),
            us(percentile(times, 99)),
        );
    }
    let added = us(percentile(&mut gateway_times, 50)) - us(percentile(&mut direct_times, 50));
    println!(
        "   added: p50 {added:7.1} us   p95 {:7.1} us   (budget: p50 < 1000 us)",
        us(percentile(&mut gateway_times, 95)) - us(percentile(&mut direct_times, 95)),
    );
    if added >= 1000.0 {
        eprintln!("over budget");
        std::process::exit(1);
    }
}

/// A keep-alive JSON-RPC upstream on 127.0.0.1 for chain 1 at block 100.
/// It answers every call, batches included, with a fixed result for the
/// method; its URL is returned.
async fn mock_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve(stream));
        }
    });
    url
}

async fn serve(mut stream: TcpStream) {
    let mut buf = Vec::new();
    while let Some(body) = read_request(&mut stream, &mut buf).await {
        let answer = |call: &Value| {
            let result = match call["method"].as_str() {
                Some("eth_chainId") => json!("0x1"),
                Some("eth_blockNumber") => json!("0x64"),
                _ => json!("0x"),
            };
            json!({"jsonrpc": "2.0", "id": call["id"], "result": result})
        };
        let reply = match serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null) {
            Value::Array(calls) => Value::Array(calls.iter().map(answer).collect()),
            call => answer(&call),
        }
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
            reply.len()
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// The body of the next request on a keep-alive connection; `None` at EOF.
async fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= end + 4 + length {
                let body = buf[end + 4..end + 4 + length].to_vec();
                buf.drain(..end + 4 + length);
                return Some(body);
            }
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}
