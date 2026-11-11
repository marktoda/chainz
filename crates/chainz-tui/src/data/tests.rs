//! Data-layer tests against local mock JSON-RPC servers and a mock
//! gateway control socket.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chainz_core::runtime::ServeInfo;
use chainz_rpc::ErrorClass;
use serde_json::{Value, json};
use tokio::runtime::Handle;

use super::testing::*;
use super::*;
use crate::gateway::testing as gw;
use crate::model::{EndpointStatus, Sheet, State, Status, Via};

const NOW: u64 = 1_700_000_000;

fn tuning() -> Tuning {
    Tuning::default()
}

#[test]
fn pace_is_the_block_time_within_three_to_twelve_seconds() {
    let t = tuning();
    let secs = |s: f64| pace(Some(s), &t).as_secs_f64();
    assert_eq!(secs(0.25), 3.0); // arbitrum: floor
    assert_eq!(secs(2.0), 3.0); // base: floor
    assert_eq!(secs(5.0), 5.0);
    assert_eq!(secs(12.04), 12.0); // ethereum: ceiling
    assert_eq!(secs(600.0), 12.0);
    // Until the block time is known, and for nonsense, poll slowly.
    assert_eq!(pace(None, &t), Duration::from_secs(12));
    assert_eq!(pace(Some(f64::NAN), &t), Duration::from_secs(12));
    assert_eq!(pace(Some(0.0), &t), Duration::from_secs(12));
    assert_eq!(pace(Some(-3.0), &t), Duration::from_secs(12));
}

#[test]
fn hostile_timestamps_cannot_crash_or_skew_the_pacing() {
    let t = tuning();
    // Values `Duration::from_secs_f64` would panic on.
    for secs in [1e300, f64::MAX, u64::MAX as f64 * 4.0, f64::INFINITY] {
        assert_eq!(pace(Some(secs), &t), Duration::from_secs(12), "{secs}");
    }
    assert_eq!(pace(Some(f64::MIN_POSITIVE), &t), Duration::from_secs(3));
    assert_eq!(pace(Some(f64::NEG_INFINITY), &t), Duration::from_secs(12));

    // A huge timestamp delta: one block, "u64::MAX seconds" later.
    let mut clock = BlockClock::default();
    clock.observe(1_000, 0);
    clock.observe(1_001, u64::MAX);
    assert_eq!(clock.block_time(), None);
    assert_eq!(pace(clock.block_time(), &t), Duration::from_secs(12));
    // Just over an hour per block is not believed; an hour is.
    let mut clock = BlockClock::default();
    clock.observe(10, NOW);
    clock.observe(11, NOW + 3_601);
    assert_eq!(clock.block_time(), None);
    let mut clock = BlockClock::default();
    clock.observe(10, NOW);
    clock.observe(11, NOW + 3_600);
    assert_eq!(clock.block_time(), Some(3_600.0));
    // Time that stands still or runs backwards is not a block time.
    let mut clock = BlockClock::default();
    clock.observe(10, NOW);
    clock.observe(20, NOW);
    assert_eq!(clock.block_time(), None);
    clock.observe(30, NOW - 500);
    assert_eq!(clock.block_time(), None);
    // A hostile anchor (block 0 at time u64::MAX, or far in the past).
    let mut clock = BlockClock::default();
    clock.observe(u64::MAX, NOW);
    clock.anchor(0, u64::MAX);
    assert_eq!(clock.block_time(), None);
    let mut clock = BlockClock::default();
    clock.observe(2, u64::MAX);
    clock.anchor(1, 0);
    assert_eq!(clock.block_time(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hostile_endpoint_does_not_stop_the_poller() {
    let dir = tempfile::tempdir().unwrap();
    // Every block claims a timestamp near u64::MAX, except the look-back
    // block, which claims zero.
    let rpc = MockRpc::calls(|_, params| {
        let latest = params[0] == "latest";
        Ok(json!({
            "number": if latest { "0x3e8" } else { "0x320" },
            "timestamp": if latest { "0xfffffffffffffff0" } else { "0x0" },
            "baseFeePerGas": "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        }))
    })
    .await;
    let plan = chains_only(vec![chain("evil", 1, &[("a", &rpc.url)])]);
    let (shared, updates) = shared(dir.path(), fast());
    let _engine = Engine::start(&Handle::current(), &shared, &plan);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    // Still polling (at the ceiling: 600 ms in the test tuning), no panic.
    assert!(latest_polls(&rpc) >= 3, "{} polls", latest_polls(&rpc));
    let mut state = State::new(&plan, &shared.store());
    while let Ok(update) = updates.try_recv() {
        state.apply(update);
    }
    let chain = &state.chains[0];
    assert_eq!(chain.block_time, None);
    assert_eq!(chain.head.as_ref().unwrap().base_fee, Some(u128::MAX));
    // Rendering and the snapshot survive it too.
    let _ = chain.status(state.slow_threshold());
    let _ = chain.block_age(crate::model::now_ms());
    serde_json::to_string(&crate::snapshot::Snapshot::new(
        &state,
        crate::model::now_ms(),
    ))
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_dashboard_polls_at_the_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let rpc = MockRpc::chain(5_000, NOW, 0.01).await; // would poll every 100 ms
    let plan = chains_only(vec![chain("quick", 1, &[("a", &rpc.url)])]);
    let (shared, _updates) = shared(dir.path(), fast());
    shared.set_idle(true);
    let _engine = Engine::start(&Handle::current(), &shared, &plan);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    // Every 600 ms while idle: 0, 0.6, 1.2.
    let idle = latest_polls(&rpc);
    assert!((2..=4).contains(&idle), "{idle} polls while idle");
    // A key press: back to the chain's own pace from the next tick.
    shared.set_idle(false);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let active = latest_polls(&rpc) - idle;
    assert!(active >= 8, "{active} polls once active");
}

#[test]
fn block_clock_averages_over_what_it_has_seen() {
    let mut clock = BlockClock::default();
    assert_eq!(clock.block_time(), None);
    clock.observe(1_000, NOW);
    assert_eq!(clock.block_time(), None, "one block is not a rate");
    clock.anchor(800, NOW - 400);
    assert_eq!(clock.block_time(), Some(2.0));
    // A newer anchor than the oldest block, a repeat and a reorg are ignored.
    clock.anchor(900, NOW);
    clock.observe(1_000, NOW + 50);
    clock.observe(990, NOW + 50);
    assert_eq!(clock.block_time(), Some(2.0));
    clock.observe(1_100, NOW + 500);
    assert_eq!(clock.block_time(), Some(3.0));
    // The window slides: old observations fall out.
    for i in 0..200 {
        clock.observe(2_000 + i, NOW + 1_000 + i * 12);
    }
    assert_eq!(clock.block_time(), Some(12.0));
}

fn latest_polls(rpc: &MockRpc) -> usize {
    rpc.hits()
        .iter()
        .filter(|h| h.body["params"][0] == "latest")
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pollers_pace_each_chain_by_its_block_time() {
    let dir = tempfile::tempdir().unwrap();
    // With the test tuning the pace is clamped to 100–600 ms.
    let quick = MockRpc::chain(5_000, NOW, 0.01).await; // → 100 ms (floor)
    let medium = MockRpc::chain(5_000, NOW, 0.25).await; // → 250 ms
    let slow = MockRpc::chain(5_000, NOW, 30.0).await; // → 600 ms (ceiling)
    let plan = chains_only(vec![
        chain("quick", 1, &[("a", &quick.url)]),
        chain("medium", 2, &[("a", &medium.url)]),
        chain("slow", 3, &[("a", &slow.url)]),
    ]);
    let (shared, updates) = shared(dir.path(), fast());
    let engine = Engine::start(&Handle::current(), &shared, &plan);
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    drop(engine);

    // Every chain: one poll, a wait at the floor (100 ms) while the block
    // time is measured, then its own pace. About 21, 9 and 5 polls in 2.1 s.
    let (q, m, s) = (
        latest_polls(&quick),
        latest_polls(&medium),
        latest_polls(&slow),
    );
    assert!((15..=22).contains(&q), "quick polled {q} times");
    assert!((7..=10).contains(&m), "medium polled {m} times");
    assert!((4..=5).contains(&s), "slow polled {s} times");

    // One request per tick, plus one look-back block per chain, ever.
    for rpc in [&quick, &medium, &slow] {
        let hits = rpc.hits();
        assert!(hits.iter().all(|h| h.methods() == ["eth_getBlockByNumber"]));
        assert_eq!(hits.len() - latest_polls(rpc), 1, "one look-back request");
        assert_eq!(hits[1].body["params"][0], "latest", "not on the first tick");
    }
    // Polls of one chain are spaced by its pace, not bunched.
    let times: Vec<_> = medium.hits().iter().map(|h| h.at).collect();
    let last_gap = times[times.len() - 1] - times[times.len() - 2];
    assert!(
        (Duration::from_millis(200)..Duration::from_millis(450)).contains(&last_gap),
        "medium's last gap was {last_gap:?}"
    );

    let mut state = State::new(&plan, &shared.store());
    while let Ok(update) = updates.try_recv() {
        state.apply(update);
    }
    let block_times: Vec<Option<f64>> = state.chains.iter().map(|c| c.block_time).collect();
    assert_eq!(block_times, [Some(0.01), Some(0.25), Some(30.0)]);
    let head = state.chains[0].head.as_ref().unwrap();
    assert_eq!((head.number, head.timestamp), (5_000, Some(NOW)));
    assert_eq!(head.base_fee, Some(100_000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_chain_backs_off_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    // Rate limited for the first three requests, then healthy.
    let rpc = MockRpc::start(move |body| {
        if seen.fetch_add(1, Ordering::SeqCst) < 3 {
            Reply::Status(429)
        } else {
            Reply::Json(answer(body, &|_, params| {
                Ok(block(77, NOW, 2.0, &params[0]))
            }))
        }
    })
    .await;
    let plan = chains_only(vec![chain("eth", 1, &[("a", &rpc.url)])]);
    let (shared, updates) = shared(dir.path(), fast());
    let _engine = Engine::start(&Handle::current(), &shared, &plan);

    let first = wait_for(&updates, |u| match u {
        Update::Head { result, .. } => Some(result),
        _ => None,
    })
    .await;
    assert_eq!(first.unwrap_err().class, ErrorClass::RateLimited);
    // Failures wait 300, 600, then 1200 ms: by 800 ms only two requests
    // have gone out, where an unthrottled retry loop would have sent many.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let head = wait_for(&updates, |u| match u {
        Update::Head {
            result: Ok(head), ..
        } => Some(head),
        _ => None,
    })
    .await;
    assert_eq!(head.number, 77);
}

/// A mock chain that answers balance reads. `multicall` decides what an
/// `eth_call` to Multicall3 returns.
async fn balance_rpc(
    multicall: impl Fn() -> Result<Value, i64> + Send + Sync + 'static,
) -> MockRpc {
    MockRpc::calls(move |method, params| match method {
        "eth_call" => {
            let to = params[0]["to"].as_str().unwrap().to_lowercase();
            let data = params[0]["data"].as_str().unwrap();
            let holder_a = data.ends_with(&HOLDER_A[2..].to_lowercase());
            if to == abi::MULTICALL3.to_lowercase() {
                multicall()
            } else if to == USDC.to_lowercase() {
                // 12.5 USDC for A, 0 for B.
                Ok(json!(format!(
                    "0x{:064x}",
                    if holder_a { 12_500_000 } else { 0 }
                )))
            } else {
                // WETH's balanceOf reverts.
                Err(3)
            }
        }
        "eth_getBalance" => {
            let holder_a = params[0].as_str().unwrap().eq_ignore_ascii_case(HOLDER_A);
            Ok(json!(if holder_a { "0xde0b6b3a7640000" } else { "0x0" }))
        }
        _ => Err(-32601),
    })
    .await
}

async fn read_sheet(shared: &Arc<Shared>, plan: &Plan) -> Result<Sheet, Fault> {
    let holders = Arc::new(plan.holders());
    match balances::read_update(shared.clone(), plan.chains[0].clone(), holders).await {
        Update::Balances { chain, result, .. } => {
            assert_eq!(chain, "eth");
            result
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn balances_come_from_one_multicall_where_it_is_deployed() {
    let dir = tempfile::tempdir().unwrap();
    // Per holder: native, then usdc, then weth (the plan's token order).
    let rpc = balance_rpc(|| {
        Ok(json!(aggregate3_result(&[
            Some(1_000_000_000_000_000_000),
            Some(12_500_000),
            None, // weth.balanceOf(A) failed inside the multicall
            Some(0),
            Some(0),
            Some(5),
        ])))
    })
    .await;
    let plan = plan(vec![chain("eth", 1, &[("a", &rpc.url)])]);
    let (shared, _updates) = shared(dir.path(), fast());

    let sheet = read_sheet(&shared, &plan).await.unwrap();
    assert_eq!(sheet.via, Via::Multicall3);
    // Only signers with a cached address are read; `cold` has none.
    let names: Vec<&str> = sheet.holders.iter().map(|h| h.signer.as_str()).collect();
    assert_eq!(names, ["default", "funder"]);
    assert_eq!(sheet.holders[0].native, Some(1_000_000_000_000_000_000));
    assert_eq!(sheet.holders[0].tokens, [Some(12_500_000), None]);
    assert_eq!(sheet.holders[1].native, Some(0));
    assert_eq!(sheet.holders[1].tokens, [Some(0), Some(5)]);

    // Exactly one request: an eth_call of aggregate3 to Multicall3 that
    // reads getEthBalance and balanceOf for both holders.
    let hits = rpc.hits();
    assert_eq!(hits.len(), 1);
    let call = &hits[0].body["params"][0];
    assert_eq!(
        call["to"].as_str().unwrap().to_lowercase(),
        abi::MULTICALL3.to_lowercase()
    );
    let data = call["data"].as_str().unwrap();
    assert!(data.starts_with("0x82ad56cb"));
    for needle in [
        "4d2301cc",
        "70a08231",
        &HOLDER_B[2..].to_lowercase(),
        &USDC[2..].to_lowercase(),
    ] {
        assert!(data.contains(needle), "calldata lacks {needle}");
    }
}

#[tokio::test]
async fn balances_fall_back_to_a_batch_where_multicall3_has_no_code() {
    let dir = tempfile::tempdir().unwrap();
    // An eth_call to an address without code returns empty data.
    let rpc = balance_rpc(|| Ok(json!("0x"))).await;
    let plan = plan(vec![chain("eth", 1, &[("a", &rpc.url)])]);
    let (shared, _updates) = shared(dir.path(), fast());

    let sheet = read_sheet(&shared, &plan).await.unwrap();
    assert_eq!(sheet.via, Via::Batch);
    assert_eq!(sheet.holders[0].native, Some(1_000_000_000_000_000_000));
    // A reverting balanceOf is a blank cell, not a failed sheet.
    assert_eq!(sheet.holders[0].tokens, [Some(12_500_000), None]);
    assert_eq!(sheet.holders[1].native, Some(0));
    assert_eq!(sheet.holders[1].tokens, [Some(0), None]);

    // The multicall attempt, then one batch: per holder a getBalance and
    // a balanceOf per token.
    let hits = rpc.hits();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].methods(), ["eth_call"]);
    let per_holder = ["eth_getBalance", "eth_call", "eth_call"];
    assert_eq!(hits[1].methods(), [per_holder, per_holder].concat());

    // The absence is remembered: the next round is the batch alone.
    assert_eq!(read_sheet(&shared, &plan).await.unwrap().via, Via::Batch);
    let hits = rpc.hits();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[2].methods().len(), 6);
}

#[tokio::test]
async fn a_failed_multicall_falls_back_for_that_round_only() {
    let dir = tempfile::tempdir().unwrap();
    let attempts = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = attempts.clone();
    // The multicall reverts once, then returns garbage, then works.
    let rpc = balance_rpc(move || match seen.fetch_add(1, Ordering::SeqCst) {
        0 => Err(3),
        1 => Ok(json!("0xdeadbeef")),
        _ => Ok(json!(aggregate3_result(&[Some(1); 6]))),
    })
    .await;
    let plan = plan(vec![chain("eth", 1, &[("a", &rpc.url)])]);
    let (shared, _updates) = shared(dir.path(), fast());

    assert_eq!(read_sheet(&shared, &plan).await.unwrap().via, Via::Batch);
    assert_eq!(read_sheet(&shared, &plan).await.unwrap().via, Via::Batch);
    let third = read_sheet(&shared, &plan).await.unwrap();
    assert_eq!(third.via, Via::Multicall3);
    assert_eq!(third.holders[1].tokens, [Some(1), Some(1)]);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "multicall tried every round"
    );
    assert_eq!(rpc.hits().len(), 5);
}

#[tokio::test]
async fn balance_errors_are_reported_without_hammering() {
    let dir = tempfile::tempdir().unwrap();
    // Rate limited: the batch would be refused too, so it is not sent.
    let limited = MockRpc::start(|_| Reply::Status(429)).await;
    let plan_limited = plan(vec![chain("eth", 1, &[("a", &limited.url)])]);
    let (shared, _updates) = shared(dir.path(), fast());
    let fault = read_sheet(&shared, &plan_limited).await.unwrap_err();
    assert_eq!(fault.class, ErrorClass::RateLimited);
    assert_eq!(limited.hits().len(), 1);

    // Nothing works at all: multicall, then the batch, then an error.
    let broken = MockRpc::calls(|_, _| Err(-32000)).await;
    let plan_broken = plan(vec![chain("eth", 1, &[("a", &broken.url)])]);
    let fault = read_sheet(&shared, &plan_broken).await.unwrap_err();
    assert_eq!(fault.class, ErrorClass::Rpc);
    assert_eq!(broken.hits().len(), 2);

    // An endpoint whose variables do not resolve never sends anything.
    let mut invalid = chain("eth", 1, &[("a", "http://127.0.0.1:1")]);
    invalid.endpoints[0].target = Err("variable ${INFURA_KEY} is not set".into());
    let fault = read_sheet(&shared, &plan(vec![invalid])).await.unwrap_err();
    assert_eq!(fault.class, ErrorClass::Invalid);
    assert!(fault.message.contains("INFURA_KEY"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn balances_refresh_on_their_interval_and_on_demand() {
    let dir = tempfile::tempdir().unwrap();
    let rpc = balance_rpc(|| Ok(json!(aggregate3_result(&[Some(7); 6])))).await;
    let plan = plan(vec![chain("eth", 1, &[("a", &rpc.url)])]);
    let mut tuning = fast();
    tuning.balance_every = Duration::from_secs(30);
    let (shared, updates) = shared(dir.path(), tuning);
    let engine = Engine::start(&Handle::current(), &shared, &plan);
    let sheet = |u| match u {
        Update::Balances { result, .. } => Some(result.unwrap()),
        _ => None,
    };
    assert_eq!(wait_for(&updates, sheet).await.holders[0].native, Some(7));
    let calls = || rpc.methods().iter().filter(|m| *m == "eth_call").count();
    assert_eq!(calls(), 1);
    // Nothing more until asked: the interval is 30 s.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(calls(), 1);
    engine.refresh(Refresh::Balances);
    wait_for(&updates, sheet).await;
    assert_eq!(calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoints_are_probed_only_while_focused_and_lag_is_classified() {
    let dir = tempfile::tempdir().unwrap();
    // Fresh blocks: nothing here is stale against the wall clock.
    let now = crate::model::now_ms() / 1_000;
    let probe = |head: u64| {
        MockRpc::calls(move |method, params| match method {
            "eth_chainId" => Ok(json!("0x1")),
            "eth_blockNumber" => Ok(json!(format!("{head:#x}"))),
            "eth_getBlockByNumber" => Ok(block(head, now, 2.0, &params[0])),
            _ => Err(-32601),
        })
    };
    // The active endpoint is 25 blocks behind its peers.
    let behind = probe(975).await;
    let best = probe(1_000).await;
    let near = probe(990).await;
    let wrong = MockRpc::calls(|_, _| Ok(json!("0x5"))).await; // another chain
    let down = MockRpc::start(|_| Reply::Status(503)).await;
    let plan = chains_only(vec![
        chain(
            "eth",
            1,
            &[
                ("behind", &behind.url),
                ("best", &best.url),
                ("near", &near.url),
                ("wrong", &wrong.url),
                ("down", &down.url),
                ("socket", "wss://eth.example.org/ws"),
            ],
        ),
        chain("other", 2, &[("a", &best.url)]),
    ]);
    let (shared, updates) = shared(dir.path(), fast());
    let engine = Engine::start(&Handle::current(), &shared, &plan);
    let mut state = State::new(&plan, &shared.store());

    // Unfocused, nothing is probed: peers of the active endpoint are idle.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        best.hits()
            .iter()
            .all(|h| h.methods() != ["eth_chainId", "eth_blockNumber"])
    );
    assert!(near.hits().is_empty() && down.hits().is_empty());

    engine.focus(Some("eth".into()));
    let (chain_name, results) = wait_for(&updates, |u| match u {
        Update::Probes { chain, results, .. } => Some((chain, results)),
        other => {
            state.apply(other);
            None
        }
    })
    .await;
    assert_eq!(chain_name, "eth");
    assert_eq!(
        results.len(),
        5,
        "the websocket endpoint is listed, not probed"
    );
    state.apply(Update::Probes {
        chain: chain_name,
        at_ms: NOW * 1_000,
        results,
    });

    let eth = &state.chains[0];
    let statuses: Vec<EndpointStatus> = eth.endpoint_statuses().iter().map(|s| s.0).collect();
    assert_eq!(
        statuses,
        [
            EndpointStatus::Lagging(25),
            EndpointStatus::Ok,
            EndpointStatus::Ok, // 10 behind: within the threshold of 20
            EndpointStatus::Failing(ErrorClass::ChainMismatch),
            EndpointStatus::Failing(ErrorClass::Http),
            EndpointStatus::Websocket,
        ]
    );
    let lags: Vec<Option<u64>> = eth.endpoint_statuses().iter().map(|s| s.1).collect();
    assert_eq!(lags, [Some(25), Some(0), Some(10), None, None, None]);

    // The next head poll of the lagging active endpoint is measured
    // against the best head its peers reported.
    let slow_ms = state.slow_threshold();
    wait_for(&updates, |u| {
        let fresh = matches!(&u, Update::Head { chain, .. } if chain == "eth");
        state.apply(u);
        fresh.then_some(())
    })
    .await;
    assert_eq!(state.chains[0].status(slow_ms), Status::Lag(25));
    assert_eq!(state.chains[1].status(slow_ms), Status::Ok);

    // Probes repeat every 400 ms while focused, and stop when unfocused.
    let rounds = || near.hits().len();
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!((2..=4).contains(&rounds()), "{} probe rounds", rounds());
    engine.focus(None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let settled = rounds();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(rounds(), settled);

    // Probes are written to the health history, as `rpc bench` does.
    let stored = chainz_rpc::HealthStore::load(dir.path()).unwrap();
    assert_eq!(stored.latest(1, "best").unwrap().head, Some(1_000));
    assert_eq!(
        stored.latest(1, "down").unwrap().error,
        Some(ErrorClass::Http)
    );
}

fn serve_info(port: u16) -> ServeInfo {
    ServeInfo {
        pid: std::process::id(),
        port,
        token: "s3cr3t-token".into(),
        started_at: NOW,
        version: "2.0.0-test".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_go_through_the_gateway_while_one_runs() {
    let dir = tempfile::tempdir().unwrap();
    let direct = MockRpc::chain(100, NOW, 2.0).await;
    let gateway = MockRpc::chain(200, NOW, 2.0).await;
    let port: u16 = gateway.url.rsplit(':').next().unwrap().parse().unwrap();
    let plan = chains_only(vec![chain("eth", 1, &[("a", &direct.url)])]);
    let (shared, updates) = shared(dir.path(), fast());
    let _engine = Engine::start(&Handle::current(), &shared, &plan);
    let head = |u| match u {
        Update::Head {
            via_gateway,
            result,
            ..
        } => Some((via_gateway, result.unwrap().number)),
        _ => None,
    };
    assert_eq!(wait_for(&updates, head).await, (false, 100));

    // A gateway appears: it holds the lock and has written serve.json.
    let runtime = chainz_core::runtime::Runtime::at(dir.path().join("runtime"));
    let lock = runtime.acquire_serve_lock().unwrap().unwrap();
    runtime.write_serve_info(&serve_info(port)).unwrap();
    let info = wait_for(&updates, |u| match u {
        Update::Gateway(Some(info)) => Some(info),
        _ => None,
    })
    .await;
    assert_eq!(info.port, port);
    assert_eq!(
        wait_for(&updates, |u| head(u).filter(|h| h.0)).await,
        (true, 200)
    );
    // The gateway's per-chain URL, by token and chain name.
    assert_eq!(gateway.hits()[0].path, "/s3cr3t-token/eth");

    // It stops: back to the active endpoint.
    drop(lock);
    wait_for(&updates, |u| {
        matches!(u, Update::Gateway(None)).then_some(())
    })
    .await;
    assert_eq!(
        wait_for(&updates, |u| head(u).filter(|h| !h.0)).await,
        (false, 100)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traffic_follows_the_gateway_control_socket() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(vec![]);
    let (shared, updates) = shared(dir.path(), fast());
    let mut state = State::new(&plan, &shared.store());
    let engine = Engine::start(&Handle::current(), &shared, &plan);
    // No gateway: the workers say so, and never touch the socket.
    wait_for(&updates, |u| {
        matches!(u, Update::Gateway(None)).then_some(())
    })
    .await;

    let runtime = chainz_core::runtime::Runtime::at(dir.path().join("runtime"));
    let lock = runtime.acquire_serve_lock().unwrap().unwrap();
    runtime.write_serve_info(&serve_info(18545)).unwrap();
    let server =
        gw::MockControlServer::start(&runtime.serve_sock_path(), gw::status(), gw::metrics())
            .await
            .unwrap();

    // Status and metrics arrive, folded into the dashboard's shape.
    let snapshot = wait_for(&updates, |u| {
        state.apply(u.clone());
        match u {
            Update::Traffic(Ok(snapshot)) => Some(snapshot),
            _ => None,
        }
    })
    .await;
    assert_eq!(snapshot, gw::traffic());
    assert_eq!(state.gateway.as_ref().unwrap().port, 18545);

    // The request stream is subscribed to, and records flow into the feed.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while server.event_subscribers() == 0 {
        assert!(std::time::Instant::now() < deadline, "no events subscriber");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    server.push_event(gw::record(
        1,
        NOW * 1_000,
        "eth_call",
        "@usdc.balanceOf(@wallet)",
    ));
    server.push_event(gw::failed_record(2, NOW * 1_000 + 500));
    wait_for(&updates, |u| {
        state.apply(u);
        (state.feed.len() == 2).then_some(())
    })
    .await;
    assert_eq!(state.feed[0].summary, "@usdc.balanceOf(@wallet)");
    assert_eq!(state.feed[1].error, Some((ErrorClass::RateLimited, None)));
    assert_eq!(state.feed_rate(NOW * 1_000 + 600, 10), 0.2);

    // Metrics are polled: a change shows up, and `r` asks at once.
    let mut busier = gw::metrics();
    busier.windows[1].rows[0].requests = 4_000;
    server.set_metrics(busier);
    engine.refresh(Refresh::Traffic);
    wait_for(&updates, |u| match u {
        Update::Traffic(Ok(s)) => (s.windows[0].requests == 4_342).then_some(()),
        _ => None,
    })
    .await;

    // The gateway dies without cleaning up: the dashboard notices.
    drop(server);
    drop(lock);
    wait_for(&updates, |u| {
        state.apply(u);
        state.gateway.is_none().then_some(())
    })
    .await;
    assert!(state.traffic.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_change_on_disk_is_announced() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "version = 2\n").unwrap();
    let (shared, updates) = shared(dir.path(), fast());
    let _engine = Engine::start(&Handle::current(), &shared, &plan(vec![]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !updates
            .try_iter()
            .any(|u| matches!(u, Update::ConfigChanged))
    );
    // A new mtime, even on filesystems with coarse timestamps.
    let later = std::time::SystemTime::now() + Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&config)
        .unwrap()
        .set_modified(later)
        .unwrap();
    wait_for(&updates, |u| {
        matches!(u, Update::ConfigChanged).then_some(())
    })
    .await;
}

#[tokio::test]
async fn once_collects_every_chain_and_retries_a_rate_limit() {
    let dir = tempfile::tempdir().unwrap();
    let healthy = balance_rpc(|| Ok(json!(aggregate3_result(&[Some(3); 6])))).await;
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    // The first request is rate limited; the retry succeeds.
    let limited = MockRpc::start(move |body| {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            return Reply::Status(429);
        }
        Reply::Json(answer(body, &|method, params| match method {
            "eth_getBlockByNumber" => Ok(block(900, NOW, 12.0, &params[0])),
            _ => Ok(json!(aggregate3_result(&[Some(4); 6]))),
        }))
    })
    .await;
    let down = MockRpc::start(|_| Reply::Status(500)).await;
    let plan = plan(vec![
        chain("healthy", 1, &[("a", &healthy.url)]),
        chain("limited", 2, &[("a", &limited.url)]),
        chain("down", 3, &[("a", &down.url)]),
    ]);
    let (shared, _updates) = shared(dir.path(), fast());
    let mut state = State::new(&plan, &shared.store());
    for update in collect_once(&shared, &plan).await {
        state.apply(update);
    }
    // `healthy` serves no blocks: its head is an error, its balances fine.
    let [healthy_chain, limited_chain, down_chain] = &state.chains[..] else {
        panic!("three chains");
    };
    assert_eq!(healthy_chain.fault.as_ref().unwrap().class, ErrorClass::Rpc);
    assert_eq!(
        healthy_chain.balances.as_ref().unwrap().holders[0].native,
        Some(3)
    );
    assert_eq!(limited_chain.head.as_ref().unwrap().number, 900);
    assert_eq!(limited_chain.block_time, Some(12.0));
    assert_eq!(
        limited_chain.balances.as_ref().unwrap().holders[1].tokens,
        [Some(4), Some(4)]
    );
    assert_eq!(down_chain.fault.as_ref().unwrap().class, ErrorClass::Http);
    assert_eq!(
        down_chain.balance_fault.as_ref().unwrap().class,
        ErrorClass::Http
    );
    // An HTTP 500 is not retried: one head request, a multicall, a batch.
    assert_eq!(down.hits().len(), 3);
    // No endpoint was probed.
    assert!(!healthy.methods().contains(&"eth_chainId".to_string()));
}

#[test]
fn chains_on_one_provider_start_a_beat_apart() {
    let plan = plan(vec![
        chain("eth", 1, &[("a", "https://mainnet.infura.io/v3/key")]),
        chain("arb", 42161, &[("a", "https://arb.example.org")]),
        chain(
            "base",
            8453,
            &[("a", "https://base-mainnet.infura.io/v3/key")],
        ),
        chain(
            "op",
            10,
            &[("a", "https://optimism-mainnet.infura.io/v3/key")],
        ),
        chain("local", 31337, &[("a", "http://127.0.0.1:8545")]),
        chain("fork", 31338, &[("a", "http://127.0.0.1:8546")]),
        chain("zora", 7777777, &[("a", "https://rpc.example.org")]),
    ]);
    assert_eq!(provider_slots(&plan), [0, 0, 1, 2, 0, 0, 1]);
}
