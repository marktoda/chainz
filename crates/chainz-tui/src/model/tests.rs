//! Classification and folding, without a network or a terminal.

use super::status::{SLOW_FLOOR_MS, lag_blocks};
use super::*;
use crate::data::testing::{chain, plan};
use crate::plan::Source;
use chainz_rpc::{ErrorClass, HealthSample, HealthStore};

const NOW: u64 = 1_700_000_000_000;

fn head(number: u64, age_secs: u64) -> Head {
    Head {
        number,
        timestamp: Some(NOW / 1_000 - age_secs),
        base_fee: None,
        latency_ms: 50,
    }
}

#[test]
fn lag_is_measured_against_the_best_known_head() {
    let h = head(1_000, 2);
    assert_eq!(lag_blocks(&h, None, Some(2.0), NOW), 0);
    assert_eq!(lag_blocks(&h, Some(1_000), Some(2.0), NOW), 0);
    assert_eq!(lag_blocks(&h, Some(1_003), Some(2.0), NOW), 3);
    // A peer that is itself behind does not make this one "ahead".
    assert_eq!(lag_blocks(&h, Some(990), Some(2.0), NOW), 0);
}

#[test]
fn lag_is_also_measured_against_the_clock() {
    // A 2 s chain whose newest block is 10 minutes old has stalled,
    // even with no peer to compare against: about 300 blocks behind.
    assert_eq!(lag_blocks(&head(1_000, 600), None, Some(2.0), NOW), 300);
    // Ordinary jitter, a missed slot and some clock skew are not lag.
    assert_eq!(lag_blocks(&head(1_000, 29), None, Some(2.0), NOW), 0);
    assert_eq!(lag_blocks(&head(1_000, 75), None, Some(12.0), NOW), 0);
    assert_eq!(lag_blocks(&head(1_000, 90), None, Some(12.0), NOW), 7);
    // A block "from the future" (skewed clock) is not lag either.
    let mut future = head(1_000, 0);
    future.timestamp = Some(NOW / 1_000 + 30);
    assert_eq!(lag_blocks(&future, None, Some(2.0), NOW), 0);
    // Without a block time or a timestamp, only peers can tell.
    assert_eq!(lag_blocks(&head(1_000, 600), None, None, NOW), 0);
    future.timestamp = None;
    assert_eq!(lag_blocks(&future, Some(1_010), Some(2.0), NOW), 10);
    // The larger of the two wins.
    assert_eq!(
        lag_blocks(&head(1_000, 600), Some(1_500), Some(2.0), NOW),
        500
    );
}

fn state() -> State {
    let plan = plan(vec![
        chain(
            "eth",
            1,
            &[
                ("a", "http://127.0.0.1:1"),
                ("b", "http://127.0.0.1:2"),
                ("c", "http://127.0.0.1:3"),
                ("d", "ws://127.0.0.1:4"),
            ],
        ),
        chain("base", 8453, &[("a", "http://127.0.0.1:5")]),
        chain("arb", 42161, &[("a", "http://127.0.0.1:6")]),
    ]);
    let store = HealthStore::empty();
    State::new(&plan, &store)
}

fn polled(chain: &str, number: u64, latency_ms: u32) -> Update {
    Update::Head {
        chain: chain.into(),
        source: Source::named(chain),
        at_ms: NOW,
        via_gateway: false,
        result: Ok(Head {
            latency_ms,
            ..head(number, 1)
        }),
        block_time: Some(2.0),
    }
}

fn failed(chain: &str, class: ErrorClass) -> Update {
    Update::Head {
        chain: chain.into(),
        source: Source::named(chain),
        at_ms: NOW,
        via_gateway: false,
        result: Err(Fault::new(class, "failed")),
        block_time: None,
    }
}

fn probes(results: &[(&str, Option<u64>, Option<ErrorClass>)]) -> Update {
    probes_at(NOW, results)
}

fn probes_at(at_ms: u64, results: &[(&str, Option<u64>, Option<ErrorClass>)]) -> Update {
    Update::Probes {
        chain: "eth".into(),
        source: Source::named("eth"),
        at_ms,
        results: results
            .iter()
            .map(|(name, head, error)| {
                let sample = HealthSample {
                    at_ms,
                    latency_ms: 40,
                    head: *head,
                    error: *error,
                };
                (name.to_string(), sample, error.map(|_| "failed".into()))
            })
            .collect(),
    }
}

#[test]
fn chain_status_is_pending_ok_lag_slow_or_down() {
    let mut state = state();
    let status = |state: &State, i: usize| state.chains[i].status(state.slow_threshold());
    assert_eq!(status(&state, 0), Status::Pending);

    state.apply(polled("eth", 1_000, 50));
    assert_eq!(status(&state, 0), Status::Ok);

    // Two blocks behind a peer is noise; three is lag.
    state.apply(probes(&[("b", Some(1_002), None)]));
    state.apply(polled("eth", 1_000, 50));
    assert_eq!(status(&state, 0), Status::Ok);
    state.apply(probes(&[("b", Some(1_003), None)]));
    assert_eq!(
        status(&state, 0),
        Status::Ok,
        "judged when the head is polled"
    );
    state.apply(polled("eth", 1_000, 50));
    assert_eq!(status(&state, 0), Status::Lag(3));
    // Catching up clears it.
    state.apply(polled("eth", 1_003, 50));
    assert_eq!(status(&state, 0), Status::Ok);

    // A failed poll is down, whatever came before; the head is kept.
    state.apply(failed("eth", ErrorClass::Timeout));
    assert_eq!(
        status(&state, 0),
        Status::Down(FaultClass::Rpc(ErrorClass::Timeout))
    );
    assert_eq!(state.chains[0].head.as_ref().unwrap().number, 1_003);
    assert_eq!(state.chains[0].samples.back(), Some(&None));
    state.apply(polled("eth", 1_004, 50));
    assert_eq!(status(&state, 0), Status::Ok);

    // The gateway's head high-water mark counts as a peer.
    state.apply(Update::Gateway(Some(GatewayInfo { port: 18545 })));
    state.apply(Update::Traffic(Ok(TrafficSnapshot {
        chains: vec![GatewayChain {
            chain: "base".into(),
            active: "a".into(),
            head: Some(2_010),
        }],
        ..TrafficSnapshot::default()
    })));
    state.apply(polled("base", 2_000, 50));
    assert_eq!(status(&state, 1), Status::Lag(10));

    // Lag outranks slow; down outranks both.
    for _ in 0..8 {
        state.apply(polled("base", 2_000, 5_000));
    }
    assert_eq!(status(&state, 1), Status::Lag(10));
    state.apply(failed("base", ErrorClass::RateLimited));
    assert_eq!(
        status(&state, 1),
        Status::Down(FaultClass::Rpc(ErrorClass::RateLimited))
    );
    // An update for a chain that no longer exists is dropped.
    state.apply(polled("gone", 1, 1));
}

#[test]
fn slow_is_relative_to_the_other_chains_with_a_floor() {
    let mut state = state();
    let status = |state: &State, i: usize| state.chains[i].status(state.slow_threshold());
    assert_eq!(state.slow_threshold(), SLOW_FLOOR_MS, "no data: the floor");
    // Everyone around 50 ms: 199 ms is fine, 200 ms is slow.
    state.apply(polled("eth", 1, 50));
    state.apply(polled("base", 1, 55));
    state.apply(polled("arb", 1, 199));
    assert_eq!(state.slow_threshold(), 200);
    assert_eq!(status(&state, 2), Status::Ok);
    for _ in 0..3 {
        state.apply(polled("arb", 1, 240));
    }
    assert_eq!(status(&state, 2), Status::Slow);
    // One spike does not make a chain slow: it is the median.
    state.apply(polled("eth", 1, 3_000));
    assert_eq!(status(&state, 0), Status::Ok);
    // On a slow link everything is slow, so nothing is: 3× the median.
    let mut far = super::tests::state();
    far.apply(polled("eth", 1, 400));
    far.apply(polled("base", 1, 450));
    far.apply(polled("arb", 1, 500));
    assert_eq!(far.slow_threshold(), 1_350);
    assert!((0..3).all(|i| status(&far, i) == Status::Ok));
}

#[test]
fn endpoint_statuses_follow_the_health_rule() {
    // Where the lag threshold sits is chainz-rpc's to test: the heads
    // here are well inside or well past it.
    let mut state = state();
    let statuses = |state: &State| -> Vec<EndpointStatus> {
        state.chains[0]
            .endpoint_statuses()
            .iter()
            .map(|s| s.0)
            .collect()
    };
    assert_eq!(
        statuses(&state),
        [
            EndpointStatus::Unknown,
            EndpointStatus::Unknown,
            EndpointStatus::Unknown,
            EndpointStatus::Websocket,
        ]
    );
    state.apply(probes(&[
        ("a", Some(1_000), None),
        ("b", Some(999), None),
        ("c", Some(900), None),
    ]));
    assert_eq!(
        statuses(&state)[..3],
        [
            EndpointStatus::Ok,
            EndpointStatus::Ok,
            EndpointStatus::Lagging(100)
        ]
    );
    // A failing endpoint's old head does not set the bar.
    state.apply(probes(&[
        ("a", None, Some(ErrorClass::Connect)),
        ("b", Some(990), None),
        ("c", Some(985), None),
    ]));
    assert_eq!(
        statuses(&state)[..3],
        [
            EndpointStatus::Failing(ErrorClass::Connect),
            EndpointStatus::Ok,
            EndpointStatus::Ok,
        ]
    );
    assert!(state.chains[0].endpoints[0].live);
    assert_eq!(state.chains[0].endpoints[1].p50(), Some(40));
    assert_eq!(state.chains[0].probed_at_ms, Some(NOW));
}

#[test]
fn old_probes_are_not_compared_with_new_ones() {
    let mut state = state();
    let statuses = |state: &State| state.chains[0].endpoint_statuses()[..3].to_vec();
    // Yesterday, `a` was probed far behind where the chain is today.
    let at = probes_at;
    state.apply(at(NOW - 86_400_000, &[("a", Some(1_000), None)]));
    state.apply(at(
        NOW - 86_400_000,
        &[("c", None, Some(ErrorClass::Connect))],
    ));
    state.apply(at(NOW, &[("b", Some(51_000), None)]));
    assert_eq!(
        statuses(&state),
        [
            (EndpointStatus::Unknown, None),
            (EndpointStatus::Ok, Some(0)),
            (EndpointStatus::Unknown, None),
        ]
    );
    // Within the window, they are one round.
    state.apply(at(NOW + 20_000, &[("a", Some(50_990), None)]));
    assert_eq!(
        statuses(&state)[..2],
        [
            (EndpointStatus::Ok, Some(10)),
            (EndpointStatus::Ok, Some(0)),
        ]
    );
}

#[test]
fn a_reloaded_plan_keeps_what_is_still_true() {
    let mut state = state();
    state.apply(polled("eth", 1_000, 50));
    state.apply(polled("base", 2_000, 60));
    state.apply(probes(&[("b", Some(1_000), None)]));
    state.apply(Update::Gateway(Some(GatewayInfo { port: 18545 })));

    // `eth` switches to endpoint b; `arb` is removed; `op` is new.
    let mut eth = chain(
        "eth",
        1,
        &[
            ("a", "http://127.0.0.1:1"),
            ("b", "http://127.0.0.1:2"),
            ("c", "http://127.0.0.1:3"),
            ("d", "ws://127.0.0.1:4"),
        ],
    );
    eth.info.active = "b".into();
    // A plan reading from another endpoint is a new source.
    eth.source = Source::fresh();
    let next = plan(vec![
        eth,
        chain("base", 8453, &[("a", "http://127.0.0.1:5")]),
        chain("op", 10, &[("a", "http://127.0.0.1:7")]),
    ]);
    let store = HealthStore::empty();
    let rebuilt = state.rebuilt(&next, &store);

    let names: Vec<&str> = rebuilt
        .chains
        .iter()
        .map(|c| c.info.name.as_str())
        .collect();
    assert_eq!(names, ["eth", "base", "op"]);
    // The head came from the old endpoint: it is not b's.
    assert!(rebuilt.chains[0].head.is_none());
    assert_eq!(rebuilt.chains[0].block_time, Some(2.0));
    assert!(rebuilt.chains[0].endpoints[1].active);
    assert!(
        rebuilt.chains[0].endpoints[1].live,
        "probe history is per endpoint"
    );
    // An untouched chain keeps everything.
    assert_eq!(rebuilt.chains[1].head.as_ref().unwrap().number, 2_000);
    assert!(rebuilt.chains[2].head.is_none());
    assert_eq!(rebuilt.gateway, Some(GatewayInfo { port: 18545 }));
}

#[test]
fn traffic_after_the_gateway_stopped_is_dropped() {
    let mut state = state();
    state.apply(Update::Traffic(Err("connection refused".into())));
    assert_eq!(state.traffic_error, None);
    state.apply(Update::Gateway(Some(GatewayInfo { port: 18545 })));
    state.apply(Update::Traffic(Err("connection refused".into())));
    assert_eq!(state.traffic_error.as_deref(), Some("connection refused"));
}

#[test]
fn feed_is_capped_and_its_rate_counts_the_recent_window() {
    let mut state = state();
    state.apply(Update::Gateway(Some(GatewayInfo { port: 18545 })));
    for i in 0..(FEED_CAP as u64 + 50) {
        state.apply(Update::Feed(FeedItem {
            id: i,
            at_ms: NOW - 20_000 + i * 40,
            chain: "eth".into(),
            method: "eth_call".into(),
            summary: String::new(),
            upstream: "a".into(),
            attempts: 1,
            latency_ms: 10,
            error: None,
        }));
    }
    assert_eq!(state.feed.len(), FEED_CAP);
    assert_eq!(state.feed.front().unwrap().id, 50);
    // A replayed record (the backlog after a reconnect) is not doubled.
    let again = state.feed.back().unwrap().clone();
    state.apply(Update::Feed(again));
    assert_eq!(state.feed.len(), FEED_CAP);
    assert_eq!(state.feed.front().unwrap().id, 50);
    // An older one, replayed after newer ones, does not land at the end,
    // even when it fell off the front long ago.
    for old in [state.feed[100].clone(), state.feed[0].clone()] {
        state.apply(Update::Feed(old));
    }
    let mut early = state.feed.front().unwrap().clone();
    early.id = 7;
    early.at_ms -= 10_000;
    state.apply(Update::Feed(early));
    assert_eq!(state.feed.len(), FEED_CAP);
    assert_eq!(state.feed.back().unwrap().id, FEED_CAP as u64 + 49);
    // Same millisecond, a later ID: newer.
    let mut next = state.feed.back().unwrap().clone();
    next.id += 1;
    state.apply(Update::Feed(next));
    assert_eq!(state.feed.back().unwrap().id, FEED_CAP as u64 + 50);
    state.feed.pop_back();
    // 25 requests a second, over the last 10 s.
    let newest = state.feed.back().unwrap().at_ms;
    assert_eq!(state.feed_rate(newest), 25.1);
    assert_eq!(state.feed_rate(newest + 60_000), 0.0);
}
