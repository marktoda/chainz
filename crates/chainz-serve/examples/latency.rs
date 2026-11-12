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
use chainz_testkit::{Answer, Mock};
use serde_json::{Value, json};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(5000);
    // Chain 1 at block 100; every other call answers `0x`.
    let mock = Mock::start(|call| match call.method.as_str() {
        "eth_chainId" => Answer::Result(json!("0x1")),
        "eth_blockNumber" => Answer::Result(json!("0x64")),
        _ => Answer::Result(json!("0x")),
    });
    let upstream = mock.url();
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path().join("config"));
    let toml = format!(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = \"{upstream}\"\n"
    );
    chainz_os::write_private(paths.config(), toml.as_bytes()).unwrap();
    let mut config = GatewayConfig::new(
        paths,
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
