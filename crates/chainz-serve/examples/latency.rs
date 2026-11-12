//! Gateway overhead benchmark: the same JSON-RPC call sent straight to a
//! local mock upstream and through the gateway, interleaved, over
//! keep-alive connections.
//!
//! ```text
//! cargo run --release -p chainz-serve --example latency [-- ROUNDS]
//! ```
//!
//! The budget (docs/v2/design.md) is under 1 ms of added p50 latency.

#[path = "../tests/common/mod.rs"]
mod common;

use std::time::{Duration, Instant};

use chainz_rpc::{RpcClient, Target};
use common::{Mock, Rig, eth_config};
use serde_json::{Value, json};

fn percentile(sorted: &[Duration], pct: usize) -> Duration {
    sorted[(sorted.len() * pct / 100).min(sorted.len() - 1)]
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(5000);
    let upstream = Mock::named("0x1").await;
    let rig = Rig::start(&eth_config("a", &[], &[("a", upstream.url())])).await;
    let client = RpcClient::new();
    let direct = Target::new(1, "direct", &upstream.url(), &[]).unwrap();
    let gateway = Target::new(1, "gateway", &rig.gateway().addr().url("eth", None), &[]).unwrap();
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
        call(&gateway).await;
    }
    let mut direct_times = Vec::with_capacity(rounds);
    let mut gateway_times = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        direct_times.push(call(&direct).await);
        gateway_times.push(call(&gateway).await);
    }
    direct_times.sort_unstable();
    gateway_times.sort_unstable();
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    println!("{rounds} sequential eth_call round trips each, interleaved, loopback");
    for (name, times) in [("direct", &direct_times), ("gateway", &gateway_times)] {
        println!(
            "{name:>8}: p50 {:7.1} us   p95 {:7.1} us   p99 {:7.1} us",
            us(percentile(times, 50)),
            us(percentile(times, 95)),
            us(percentile(times, 99)),
        );
    }
    let added = us(percentile(&gateway_times, 50)) - us(percentile(&direct_times, 50));
    println!(
        "   added: p50 {added:7.1} us   p95 {:7.1} us   (budget: p50 < 1000 us)",
        us(percentile(&gateway_times, 95)) - us(percentile(&direct_times, 95)),
    );
    if added >= 1000.0 {
        eprintln!("over budget");
        std::process::exit(1);
    }
}
