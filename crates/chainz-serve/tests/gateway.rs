//! Hermetic gateway tests: an in-process gateway against scripted mock
//! upstreams on loopback. Nothing here touches the network or the real
//! config.
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chainz_core::runtime::GatewayState;
use chainz_rpc::control::{Backlog, EventsOptions, RecordStatus, Window};
use chainz_rpc::{Capabilities, ErrorClass, HealthStore};
use common::{Mock, R, Rig, eth_config, eventually, http, probed_twice_since};
use serde_json::{Value, json};

const RAW_TX: &str = "0x02f86c0180843b9aca00847735940082520894f39fd6e51aad88f6f4ce6ab8827279cfffb92266865af3107a400080c001a0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// `eth` with active `a` and failover `[b]`.
async fn pair(a: &Mock, b: &Mock) -> Rig {
    let rig = Rig::start(&eth_config("a", &["b"], &[("a", a.url()), ("b", b.url())])).await;
    rig.wait_for_head("eth", 100, &[a, b]).await;
    rig
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_rejects_wrong_token_bad_host_non_post_and_non_json() {
    let a = Mock::named("a").await;
    let rig = Rig::start(&eth_config("a", &[], &[("a", a.url())])).await;
    let path = rig.path("eth", None);
    let host = rig.host();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#;
    let json_type = ("Content-Type", "application/json");

    // The well-formed request works, by name, alias and ID, with either
    // loopback host name.
    for (chain, host) in [
        ("eth", host.as_str()),
        ("mainnet", "localhost"),
        ("1", &format!("LOCALHOST:{}", rig.port)),
    ] {
        let ok = http(
            rig.port,
            "POST",
            &rig.path(chain, None),
            &[("Host", host), json_type],
            body,
        )
        .await;
        assert_eq!(ok.status, 200, "{chain} via {host}: {ok:?}");
        assert_eq!(ok.json()["result"], "a");
    }
    let served = a.calls().len();
    assert_eq!(served, 3);

    let good_host = ("Host", host.as_str());
    let wrong_token = format!("/{}/eth", "0".repeat(64));
    let other_port = format!("127.0.0.1:{}", rig.port.wrapping_add(1));
    let evil_port = format!("evil.example:{}", rig.port);
    let unknown_chain = rig.path("nope", None);
    type Case<'a> = (&'a str, &'a str, &'a str, Vec<(&'a str, &'a str)>, u16);
    let cases: Vec<Case> = vec![
        (
            "wrong token",
            "POST",
            &wrong_token,
            vec![good_host, json_type],
            403,
        ),
        ("no token", "POST", "/eth", vec![good_host, json_type], 403),
        ("empty path", "POST", "/", vec![good_host, json_type], 403),
        (
            "extra segment",
            "POST",
            "/t/eth/a/b",
            vec![good_host, json_type],
            403,
        ),
        (
            "rebinding host",
            "POST",
            &path,
            vec![("Host", "evil.example"), json_type],
            403,
        ),
        (
            "host with our port",
            "POST",
            &path,
            vec![("Host", &evil_port), json_type],
            403,
        ),
        (
            "loopback, other port",
            "POST",
            &path,
            vec![("Host", &other_port), json_type],
            403,
        ),
        ("no host", "POST", &path, vec![json_type], 403),
        ("GET", "GET", &path, vec![good_host, json_type], 405),
        (
            "OPTIONS preflight",
            "OPTIONS",
            &path,
            vec![
                good_host,
                ("Origin", "http://evil.example"),
                ("Access-Control-Request-Method", "POST"),
            ],
            405,
        ),
        ("PUT", "PUT", &path, vec![good_host, json_type], 405),
        (
            "form post",
            "POST",
            &path,
            vec![good_host, ("Content-Type", "text/plain")],
            415,
        ),
        ("no content type", "POST", &path, vec![good_host], 415),
        (
            "json-ish type",
            "POST",
            &path,
            vec![good_host, ("Content-Type", "application/jsonp")],
            415,
        ),
        (
            "unknown chain",
            "POST",
            &unknown_chain,
            vec![good_host, json_type],
            404,
        ),
    ];
    for (what, method, path, headers, expected) in &cases {
        let reply = http(rig.port, method, path, headers, body).await;
        assert_eq!(reply.status, *expected, "{what}: {reply:?}");
        assert!(
            !reply.head.contains("access-control"),
            "{what}: CORS header sent: {}",
            reply.head
        );
    }
    assert_eq!(
        a.calls().len(),
        served,
        "a rejected request reached upstream"
    );

    // A JSON content type with a charset is fine; no CORS headers on success.
    let ok = http(
        rig.port,
        "POST",
        &path,
        &[
            good_host,
            ("Content-Type", "application/json; charset=utf-8"),
            ("Origin", "http://evil.example"),
        ],
        body,
    )
    .await;
    assert_eq!(ok.status, 200);
    assert!(!ok.head.contains("access-control"), "{}", ok.head);

    // Malformed JSON-RPC is answered here, not forwarded.
    let bad = rig.post(&path, &json!("just a string")).await;
    assert_eq!(
        (bad.status, bad.json()["error"]["code"].as_i64()),
        (400, Some(-32600))
    );
    assert_eq!(a.calls().len(), served + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_the_active_upstream_mid_stream_fails_zero_reads() {
    // Slow enough that requests are in flight when the upstream dies.
    let mut a = Mock::start("a", |_| {
        R::Delay(Duration::from_millis(5), Box::new(R::Result(json!("a"))))
    })
    .await;
    let b = Mock::named("b").await;
    let rig = Arc::new(pair(&a, &b).await);

    let done = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for worker in 0..8 {
        let rig = rig.clone();
        let done = done.clone();
        workers.push(tokio::spawn(async move {
            let mut answers = Vec::new();
            for i in 0..50 {
                let method = [
                    "eth_call",
                    "eth_getBalance",
                    "eth_blockNumber",
                    "eth_getLogs",
                ][(worker + i) % 4];
                let reply = rig.call("eth", method, json!([])).await;
                done.fetch_add(1, Ordering::SeqCst);
                answers.push((reply.status, reply.json()["result"].clone()));
            }
            answers
        }));
    }
    eventually(|| done.load(Ordering::SeqCst) >= 100).await;
    a.kill().await;
    let mut by_a = 0;
    let mut by_b = 0;
    for worker in workers {
        for (status, result) in worker.await.unwrap() {
            assert_eq!(status, 200, "a read failed: {result}");
            match result.as_str() {
                Some("a") => by_a += 1,
                Some("b") => by_b += 1,
                other => panic!("a read failed: {other:?}"),
            }
        }
    }
    assert_eq!(by_a + by_b, 400);
    assert!(by_a >= 100 && by_b >= 100, "a={by_a} b={by_b}");

    // The failovers are visible in the metrics and the inspector.
    let metrics = rig.control().metrics().await.unwrap();
    let failovers: u64 = metrics.windows[0]
        .rows
        .iter()
        .filter(|r| r.endpoint == "a")
        .map(|r| r.failovers)
        .sum();
    assert!(failovers > 0);
    let mut records = rig
        .control()
        .events(EventsOptions {
            backlog: Backlog::All,
            after_id: None,
            follow: false,
        })
        .await
        .unwrap();
    let mut failed_over = 0;
    while let Some(record) = records.recv().await {
        assert_eq!(record.status, RecordStatus::Ok, "{record:?}");
        if record.upstream == "b" {
            assert!(record.attempts >= 2, "{record:?}");
            failed_over += 1;
        }
    }
    assert_eq!(failed_over, by_b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_never_leaves_the_active_endpoint() {
    for failure in [
        R::Http(503, None),
        R::Http(429, Some("30")),
        R::Error(-32005),
    ] {
        let fail = failure.clone();
        let a = Mock::start("a", move |_| fail.clone()).await;
        let b = Mock::named("b").await;
        let rig = pair(&a, &b).await;

        // The read fails over...
        let read = rig.call("eth", "eth_call", json!([])).await;
        assert_eq!(read.json()["result"], "b", "{failure:?}");
        let reads_on_b = b.calls().len();

        // ...the sends, and anything not known to be a read, do not.
        for method in [
            "eth_sendRawTransaction",
            "eth_sendTransaction",
            "eth_sendRawTransactionSync",
            "eth_sendBundle",
            "eth_sign",
            "anvil_setBalance",
            "made_up_method",
        ] {
            let reply = rig.call("eth", method, json!([RAW_TX])).await;
            assert!(
                reply.json()["error"].is_object(),
                "{failure:?} {method}: {reply:?}"
            );
            assert!(a.methods().iter().any(|m| m == method));
        }
        // A batch with one send in it follows the send's rule.
        let batch = rig
            .post(
                &rig.path("eth", None),
                &json!([
                    {"jsonrpc":"2.0","id":1,"method":"eth_call","params":[]},
                    {"jsonrpc":"2.0","id":2,"method":"eth_sendRawTransaction","params":[RAW_TX]},
                ]),
            )
            .await;
        assert_ne!(batch.json()[0]["result"], "b", "{failure:?}: {batch:?}");
        assert_eq!(
            b.calls().len(),
            reads_on_b,
            "{failure:?}: a send reached the failover endpoint"
        );
        assert!(!b.methods().iter().any(|m| m.contains("send")));

        // A read-only batch may fail over as a whole.
        let batch = rig
            .post(
                &rig.path("eth", None),
                &json!([
                    {"jsonrpc":"2.0","id":1,"method":"eth_call","params":[]},
                    {"jsonrpc":"2.0","id":2,"method":"eth_blockNumber","params":[]},
                ]),
            )
            .await;
        if !matches!(failure, R::Error(_)) {
            // (-32005 inside a batch is not inspected.)
            assert_eq!(batch.json()[1]["result"], "b", "{failure:?}: {batch:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_active_send_fails_without_trying_anything_else() {
    let mut a = Mock::named("a").await;
    let b = Mock::named("b").await;
    let rig = pair(&a, &b).await;
    a.kill().await;
    let reply = rig
        .call("eth", "eth_sendRawTransaction", json!([RAW_TX]))
        .await;
    assert_eq!(reply.status, 502, "{reply:?}");
    let message = reply.json()["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("endpoint 'a'"), "{message}");
    assert!(!message.contains("127.0.0.1"), "URL leaked: {message}");
    assert!(b.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_endpoint_outside_the_failover_list_is_never_tried() {
    let a = Mock::start("a", |_| R::Http(503, None)).await;
    let b = Mock::start("b", |_| R::Http(503, None)).await;
    let private = Mock::named("private").await;
    let rig = Rig::start(&eth_config(
        "a",
        &["b"],
        &[("a", a.url()), ("b", b.url()), ("private", private.url())],
    ))
    .await;
    rig.wait_for_head("eth", 100, &[&a, &b]).await;
    for method in ["eth_call", "eth_getLogs", "eth_sendRawTransaction"] {
        let reply = rig.call("eth", method, json!([])).await;
        assert_eq!(reply.status, 503, "{method}: {reply:?}");
    }
    assert_eq!(a.calls().len(), 3);
    assert_eq!(
        b.calls().len(),
        2,
        "both reads were retried on the failover endpoint"
    );
    assert!(
        private.calls().is_empty(),
        "a non-failover endpoint was tried"
    );
    assert_eq!(private.probes(), 0, "a non-failover endpoint was probed");

    // With an empty failover list nothing but the active endpoint is
    // touched, not even by probes. A second chain with a failover pool
    // shows the probe loop at work meanwhile.
    let a = Mock::start("a", |_| R::Http(503, None)).await;
    let other = Mock::named("other").await;
    let (c, d) = (Mock::named("c").await, Mock::named("d").await);
    let mut toml = eth_config("a", &[], &[("a", a.url()), ("other", other.url())]);
    toml.push_str(&format!(
        "[chains.base]\nid = 8453\nrpc = \"c\"\nfailover = [\"d\"]\n[chains.base.rpcs]\nc = \"{}\"\nd = \"{}\"\n",
        c.url(),
        d.url()
    ));
    let rig = Rig::start(&toml).await;
    let reply = rig.call("eth", "eth_call", json!([])).await;
    assert_eq!(reply.status, 503);
    assert_eq!(a.calls().len(), 2, "one same-endpoint retry for a read");
    // Several probe rounds go by...
    probed_twice_since(&[&c, &d], &[c.probes(), d.probes()]).await;
    probed_twice_since(&[&c, &d], &[c.probes(), d.probes()]).await;
    // ...and none of them reached `eth`'s endpoints.
    assert!(other.calls().is_empty());
    assert_eq!((a.probes(), other.probes()), (0, 0));
    assert_eq!(a.calls().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_429_waits_on_the_same_endpoint_then_shifts() {
    // Short Retry-After: wait it out on the same endpoint.
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let a = Mock::start("a", move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            R::Http(429, Some("1"))
        } else {
            R::Result(json!("a"))
        }
    })
    .await;
    let b = Mock::named("b").await;
    let rig = pair(&a, &b).await;
    let started = Instant::now();
    let reply = rig.call("eth", "eth_getTransactionCount", json!([])).await;
    let took = started.elapsed();
    assert_eq!(reply.json()["result"], "a", "{reply:?}");
    assert!(took >= Duration::from_millis(950), "did not wait: {took:?}");
    assert_eq!(a.calls().len(), 2, "one 429, then one retry");
    assert!(b.calls().is_empty(), "shifted instead of waiting");

    // A send waits the same way and stays on the endpoint. (The limiter
    // adds a little spacing of its own after the 429.)
    hits.store(0, Ordering::SeqCst);
    let reply = rig
        .call("eth", "eth_sendRawTransaction", json!([RAW_TX]))
        .await;
    assert_eq!(reply.json()["result"], "a", "{reply:?}");
    assert!(b.calls().is_empty());

    // Long Retry-After: a read shifts at once; a send comes back as a 429.
    let a = Mock::start("a", |_| R::Http(429, Some("30"))).await;
    let b = Mock::named("b").await;
    let rig = pair(&a, &b).await;
    let started = Instant::now();
    let reply = rig.call("eth", "eth_call", json!([])).await;
    assert_eq!(reply.json()["result"], "b", "{reply:?}");
    assert_eq!(a.calls().len(), 1, "no second try on `a`");
    // Nowhere near the 30 s it was asked to wait.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    let reply = rig
        .call("eth", "eth_sendRawTransaction", json!([RAW_TX]))
        .await;
    assert_eq!(reply.status, 429, "{reply:?}");
    assert!(reply.head.contains("retry-after: 30"), "{}", reply.head);
    assert_eq!(reply.json()["error"]["code"], -32005);
    assert_eq!(b.calls().len(), 1);

    // No Retry-After at all: back off on the same endpoint (250 ms, 500 ms,
    // 1 s), each retry taking a bucket slot, then shift once the next wait
    // would pass 2 s. Bucket waits count toward those 2 s, so on a slow
    // machine the shift can come one 429 early, or the bucket may turn the
    // last retry away itself (unsent) instead of `a` answering it 429.
    let a = Mock::start("a", |_| R::Http(429, None)).await;
    let b = Mock::named("b").await;
    let rig = pair(&a, &b).await;
    let started = Instant::now();
    let reply = rig.call("eth", "eth_call", json!([])).await;
    let took = started.elapsed();
    assert_eq!(reply.json()["result"], "b", "{reply:?}");
    let sent = a.calls().len() as u64;
    assert!((3..=4).contains(&sent), "{sent} tries on `a`");
    // At least the 250 ms and 500 ms backoffs were slept.
    assert!(took >= Duration::from_millis(750), "{took:?}");
    let metrics = rig.control().metrics().await.unwrap();
    let row = metrics.windows[0]
        .rows
        .iter()
        .find(|r| r.endpoint == "a")
        .unwrap();
    // One rate-limit signal per 429, plus one for a refusal, which is no
    // upstream attempt; exactly one failover.
    assert!(
        (sent..=sent + 1).contains(&row.rate_limited),
        "{} signals for {sent} tries",
        row.rate_limited
    );
    assert_eq!(row.failovers, 1);
    assert_eq!(row.requests, sent);
    assert_eq!(row.errors[&ErrorClass::RateLimited], sent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filters_are_sticky_to_the_endpoint_that_created_them() {
    let mut a = Mock::start("a", |call| match call["method"].as_str().unwrap() {
        "eth_newFilter" => R::Result(json!("0xF1")),
        "eth_newBlockFilter" => R::Result(json!("0xf2")),
        _ => R::Result(json!("a")),
    })
    .await;
    let b = Mock::named("b").await;
    let rig = pair(&a, &b).await;
    assert_eq!(
        rig.call("eth", "eth_newFilter", json!([{}])).await.json()["result"],
        "0xF1"
    );
    // Created inside a batch, too.
    let batch = rig
        .post(
            &rig.path("eth", None),
            &json!([
                {"jsonrpc":"2.0","id":"x","method":"eth_blockNumber","params":[]},
                {"jsonrpc":"2.0","id":"y","method":"eth_newBlockFilter","params":[]},
            ]),
        )
        .await;
    assert_eq!(batch.json()[1]["result"], "0xf2");

    // `chainz update eth --rpc b`: the file watch picks the change up.
    Rig::write_config(
        rig.dir.path(),
        &eth_config("b", &["a"], &[("a", a.url()), ("b", b.url())]),
    );
    eventually(|| rig.gateway().status().chains[0].active == "b").await;
    assert_eq!(
        rig.call("eth", "eth_blockNumber", json!([])).await.json()["result"],
        "b"
    );

    // The filters still live on `a`.
    for (method, filter) in [
        ("eth_getFilterChanges", "0xf1"),
        ("eth_getFilterLogs", "0xF1"),
        ("eth_getFilterChanges", "0xf2"),
        ("eth_uninstallFilter", "0xf2"),
    ] {
        let reply = rig.call("eth", method, json!([filter])).await;
        assert_eq!(reply.json()["result"], "a", "{method} {filter}");
    }
    // Uninstalled (and never-seen) filters go to the active endpoint.
    assert_eq!(
        rig.call("eth", "eth_getFilterChanges", json!(["0xf2"]))
            .await
            .json()["result"],
        "b"
    );
    assert_eq!(
        rig.call("eth", "eth_getFilterChanges", json!(["0x99"]))
            .await
            .json()["result"],
        "b"
    );

    // A batch can't span two endpoints.
    let mixed = rig
        .post(
            &rig.path("eth", None),
            &json!([
                {"jsonrpc":"2.0","id":1,"method":"eth_getFilterChanges","params":["0xf1"]},
                {"jsonrpc":"2.0","id":2,"method":"eth_sendRawTransaction","params":[RAW_TX]},
            ]),
        )
        .await;
    assert_eq!(mixed.status, 400, "{mixed:?}");
    assert!(
        !a.methods()
            .iter()
            .chain(b.methods().iter())
            .any(|m| m.contains("send"))
    );

    // Never failed over: with `a` gone the filter read fails, although `b`
    // (now active) is healthy.
    let before = b.calls().len();
    a.kill().await;
    let reply = rig
        .call("eth", "eth_getFilterChanges", json!(["0xf1"]))
        .await;
    assert_eq!(reply.status, 502, "{reply:?}");
    assert_eq!(b.calls().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lagging_endpoint_is_not_a_failover_target() {
    let a = Mock::start("a", |_| R::Http(503, None)).await;
    let b = Mock::named("b").await;
    b.set_head(97); // 3 behind the high-water mark of 100
    let rig = pair(&a, &b).await;
    let reply = rig.call("eth", "eth_blockNumber", json!([])).await;
    assert_eq!(reply.status, 503, "{reply:?}");
    assert!(b.calls().is_empty(), "`latest` would have gone backwards");

    b.set_head(98); // within 2
    probed_twice_since(&[&b], &[b.probes()]).await;
    let reply = rig.call("eth", "eth_blockNumber", json!([])).await;
    assert_eq!(reply.json()["result"], "b");

    // `a` moves ahead: `b` is behind the mark again.
    a.set_head(200);
    rig.wait_for_head("eth", 200, &[&a, &b]).await;
    assert!(rig.call("eth", "eth_blockNumber", json!([])).await.json()["error"].is_object());
    assert_eq!(
        rig.control().status().await.unwrap().chains[0].head,
        Some(200)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_tags_steer_debug_trace_and_old_block_reads() {
    let a = Mock::named("a").await;
    let b = Mock::named("b").await;
    let c = Mock::named("c").await;
    let dir = tempfile::tempdir().unwrap();
    let toml = eth_config(
        "a",
        &["b", "c"],
        &[("a", a.url()), ("b", b.url()), ("c", c.url())],
    );
    Rig::write_config(dir.path(), &toml);
    let state = dir.path().join("state");
    let tag = |endpoint: &str, caps: Capabilities| {
        let mut store = HealthStore::load(&state).unwrap();
        store.set_capabilities(1, endpoint, caps);
        store.save().unwrap();
    };
    // a: a pruned node without debug. b: archive + debug. c: untagged.
    tag(
        "a",
        Capabilities {
            archive: Some(false),
            debug_trace: Some(false),
            probed_at: Some(1),
            ..Default::default()
        },
    );
    tag(
        "b",
        Capabilities {
            archive: Some(true),
            debug_trace: Some(true),
            probed_at: Some(1),
            ..Default::default()
        },
    );
    let gateway = chainz_serve::Gateway::start(Rig::config(dir.path()))
        .await
        .unwrap();
    let port = gateway.port();
    let rig = Rig {
        dir,
        gateway: Some(gateway),
        port,
    };
    rig.wait_for_head("eth", 100, &[&a, &b, &c]).await;
    for m in [&a, &b, &c] {
        m.set_head(1000);
    }
    rig.wait_for_head("eth", 1000, &[&a, &b, &c]).await;

    let who = |reply: common::HttpReply| reply.json()["result"].as_str().unwrap().to_string();
    assert_eq!(
        who(rig.call("eth", "eth_call", json!([{}, "latest"])).await),
        "a"
    );
    assert_eq!(
        who(rig.call("eth", "eth_call", json!([{}, "0x3e0"])).await),
        "a",
        "recent block"
    );
    assert_eq!(
        who(rig.call("eth", "eth_call", json!([{}, "0x10"])).await),
        "b",
        "old block"
    );
    assert_eq!(
        who(rig
            .call("eth", "eth_getBalance", json!(["0x00", "earliest"]))
            .await),
        "b"
    );
    assert_eq!(
        who(rig
            .call("eth", "debug_traceTransaction", json!(["0x00"]))
            .await),
        "b"
    );
    // trace_*: nobody is tagged, so it goes to the active endpoint only.
    assert_eq!(
        who(rig.call("eth", "trace_block", json!(["latest"])).await),
        "a"
    );
    assert!(
        c.calls().is_empty(),
        "an untagged failover endpoint got a capability call"
    );
    // A send is never steered.
    assert_eq!(
        who(rig
            .call("eth", "eth_sendRawTransaction", json!([RAW_TX]))
            .await),
        "a"
    );

    // Tags are re-read when the health store changes (`chainz rpc bench`).
    tag(
        "c",
        Capabilities {
            trace_block: Some(true),
            probed_at: Some(2),
            ..Default::default()
        },
    );
    tag(
        "a",
        Capabilities {
            archive: Some(false),
            debug_trace: Some(false),
            trace_block: Some(false),
            probed_at: Some(2),
            ..Default::default()
        },
    );
    let mut answer = String::new();
    for _ in 0..100 {
        answer = who(rig.call("eth", "trace_block", json!(["latest"])).await);
        if answer == "c" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(answer, "c");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_url_uses_exactly_that_endpoint() {
    let a = Mock::named("a").await;
    let b = Mock::start("b", |_| R::Http(503, None)).await;
    let other = Mock::named("other").await;
    let mut toml = eth_config(
        "a",
        &["b"],
        &[("a", a.url()), ("b", b.url()), ("other", other.url())],
    );
    toml.push_str("sock = \"wss://127.0.0.1:1\"\n");
    let rig = Rig::start(&toml).await;
    let call = json!({"jsonrpc":"2.0","id":1,"method":"eth_call","params":[]});

    let reply = rig.post(&rig.path("eth", Some("other")), &call).await;
    assert_eq!(reply.json()["result"], "other");
    // Pinned to a failing endpoint: no failover, even for a read.
    let reply = rig.post(&rig.path("eth", Some("b")), &call).await;
    assert_eq!(reply.status, 503);
    assert!(a.calls().is_empty() && other.calls().len() == 1);
    // Websocket and unknown endpoints are refused.
    let reply = rig.post(&rig.path("eth", Some("sock")), &call).await;
    assert_eq!(reply.status, 400);
    assert!(
        reply.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("websocket")
    );
    let reply = rig.post(&rig.path("eth", Some("nope")), &call).await;
    assert_eq!(reply.status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_enumerated_errors_fail_over() {
    for (failure, shifts) in [
        (R::Http(502, None), true),
        (R::Http(503, None), true),
        (R::Http(504, None), true),
        (R::Error(-32005), true),
        (
            R::Delay(Duration::from_secs(5), Box::new(R::Result(json!("late")))),
            true,
        ), // timeout
        (R::Http(500, None), false),
        (R::Http(401, None), false),
        (R::Http(404, None), false),
        (R::Error(-32000), false),
        (R::Error(3), false), // execution reverted
    ] {
        let fail = failure.clone();
        let a = Mock::start("a", move |_| fail.clone()).await;
        let b = Mock::named("b").await;
        let dir_toml = eth_config("a", &["b"], &[("a", a.url()), ("b", b.url())]);
        // Short enough to time out the 5 s delay, long enough that a busy
        // machine doesn't time out the others.
        let rig = Rig::start_with(&dir_toml, |c| c.timeout = Duration::from_secs(1)).await;
        rig.wait_for_head("eth", 100, &[&a, &b]).await;
        let reply = rig.call("eth", "eth_call", json!([])).await;
        assert_eq!(
            reply.json()["result"] == "b",
            shifts,
            "{failure:?}: {reply:?}"
        );
        assert_eq!(b.calls().len(), usize::from(shifts), "{failure:?}");
    }
}

/// `eth_sendRawTransactionSync` answers with the receipt: the inspector
/// shows the hash the node reports, as for `eth_sendRawTransaction`.
#[tokio::test]
async fn a_sync_send_is_recorded_with_the_nodes_hash() {
    let a = Mock::start("a", |call| match call["method"].as_str().unwrap() {
        "eth_sendRawTransactionSync" => R::Result(json!({
            "transactionHash": format!("0x{}", "CD".repeat(32)),
            "status": "0x1",
        })),
        _ => R::Result(json!("0x01")),
    })
    .await;
    let b = Mock::named("b").await;
    let rig = pair(&a, &b).await;
    let reply = rig
        .call("eth", "eth_sendRawTransactionSync", json!([RAW_TX]))
        .await;
    assert_eq!(reply.status, 200, "{reply:?}");
    let mut events = rig
        .control()
        .events(EventsOptions {
            backlog: Backlog::All,
            after_id: None,
            follow: false,
        })
        .await
        .unwrap();
    let record = events.recv().await.unwrap();
    assert_eq!(record.params_summary, format!("tx 0x{}", "cd".repeat(32)));
    assert!(
        b.calls().is_empty(),
        "a send never leaves the active endpoint"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspector_control_api_and_runtime_files() {
    use std::os::unix::fs::PermissionsExt;
    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    let a = Mock::start("a", |call| match call["method"].as_str().unwrap() {
        "eth_sendRawTransaction" => R::Result(json!(format!("0x{}", "ab".repeat(32)))),
        "eth_estimateGas" => R::Error(3),
        _ => R::Result(json!("0x01")),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("requests.jsonl");
    let mut toml = eth_config("a", &[], &[("a", format!("{}/v3/urlsekrit", a.url()))]);
    toml.push_str("[addresses.myvault]\neth = \"0x00000000000000000000000000000000000000aa\"\n");
    Rig::write_config(dir.path(), &toml);
    let mut config = Rig::config(dir.path());
    config.log = Some(log.clone());
    let gateway = chainz_serve::Gateway::start(config).await.unwrap();
    let port = gateway.port();
    let mut rig = Rig {
        dir,
        gateway: Some(gateway),
        port,
    };
    let runtime = rig.runtime();

    // Runtime files: lock held, serve.json and serve.sock private.
    let GatewayState::Up(info) = runtime.gateway().unwrap() else {
        panic!("the gateway is not up");
    };
    assert_eq!((info.port, info.pid), (port, std::process::id()));
    assert_eq!(info.token.len(), 64);
    for file in [runtime.serve_json_path(), runtime.serve_sock_path()] {
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", file.display());
    }
    // The exec seam hands out exactly the URLs the gateway serves.
    assert_eq!(info.addr(), rig.gateway().addr());
    // A second gateway is refused while this one runs.
    let second = chainz_serve::Gateway::start(Rig::config(rig.dir.path())).await;
    let error = format!("{:#}", second.unwrap_err());
    assert!(error.contains("already running"), "{error}");
    assert!(
        matches!(runtime.gateway().unwrap(), GatewayState::Up(_)),
        "the loser must not clean up"
    );

    // Live events, then some traffic.
    let control = rig.control();
    let mut live = control.events(EventsOptions::default()).await.unwrap();
    let balance_of = format!("0x70a08231{}", "00".repeat(32));
    rig.call(
        "eth",
        "eth_call",
        json!([{"to": USDC, "data": balance_of}, "latest"]),
    )
    .await;
    rig.call(
        "eth",
        "eth_getBalance",
        json!(["0x00000000000000000000000000000000000000aa", "latest"]),
    )
    .await;
    rig.call("eth", "eth_sendRawTransaction", json!([RAW_TX]))
        .await;
    rig.call("eth", "eth_estimateGas", json!([{"to": USDC}]))
        .await;
    rig.post(
        &rig.path("eth", None),
        &json!([{"id":1,"method":"eth_chainId"},{"id":2,"method":"eth_chainId"}]),
    )
    .await;

    let mut records = Vec::new();
    for _ in 0..5 {
        let record = tokio::time::timeout(Duration::from_secs(5), live.recv())
            .await
            .unwrap()
            .unwrap();
        records.push(record);
    }
    let summary: Vec<(&str, &str)> = records
        .iter()
        .map(|r| (r.method.as_str(), r.params_summary.as_str()))
        .collect();
    let tx_hash = format!("tx 0x{}", "ab".repeat(32));
    assert_eq!(
        summary,
        [
            ("eth_call", "@usdc.balanceOf latest"),
            ("eth_getBalance", "@myvault latest"),
            ("eth_sendRawTransaction", tx_hash.as_str()),
            ("eth_estimateGas", "@usdc"),
            ("batch(2)", "eth_chainId×2"),
        ]
    );
    assert_eq!(
        records.iter().map(|r| r.id).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    assert!(
        records
            .iter()
            .all(|r| r.chain == "eth" && r.upstream == "a" && r.attempts == 1)
    );
    assert_eq!(records[0].status, RecordStatus::Ok);
    assert_eq!(
        records[3].status,
        RecordStatus::Error {
            class: ErrorClass::Rpc,
            code: Some(3)
        }
    );
    assert!(records[0].response_bytes > 0);

    // Backlog replay, with and without following.
    let mut replay = control
        .events(EventsOptions {
            backlog: Backlog::Last(2),
            after_id: None,
            follow: false,
        })
        .await
        .unwrap();
    assert_eq!(replay.recv().await.unwrap().id, 4);
    assert_eq!(replay.recv().await.unwrap().id, 5);
    assert!(
        replay.recv().await.is_none(),
        "follow=false ends after the backlog"
    );
    let mut tail = control
        .events(EventsOptions {
            backlog: Backlog::Last(1),
            after_id: None,
            follow: true,
        })
        .await
        .unwrap();
    assert_eq!(tail.recv().await.unwrap().id, 5);
    rig.call("eth", "eth_blockNumber", json!([])).await;
    assert_eq!(tail.recv().await.unwrap().id, 6);

    // Status and metrics.
    let status = control.status().await.unwrap();
    assert_eq!((status.port, status.pid), (port, std::process::id()));
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(status.chains.len(), 1);
    assert_eq!(
        (status.chains[0].chain.as_str(), status.chains[0].chain_id),
        ("eth", 1)
    );
    assert_eq!(status.chains[0].active, "a");
    let metrics = control.metrics().await.unwrap();
    assert_eq!(
        metrics
            .windows
            .iter()
            .map(|w| w.window_secs)
            .collect::<Vec<_>>(),
        [60, 900]
    );
    for window in &metrics.windows {
        let total: u64 = window.rows.iter().map(|r| r.requests).sum();
        assert_eq!(total, 6);
        let call = window.rows.iter().find(|r| r.method == "eth_call").unwrap();
        assert_eq!(
            (call.chain.as_str(), call.endpoint.as_str(), call.requests),
            ("eth", "a", 1)
        );
        assert!(call.bytes > 0 && call.errors.is_empty());
        let totals = window.totals.unwrap();
        assert_eq!((totals.requests, totals.errors), (6, 0));
        assert_eq!(window.chains["eth"], totals);
    }
    assert_eq!(metrics.window(Window::Short).unwrap().window_secs, 60);

    // Reload through the control API; a broken config keeps the old one,
    // and the client hears why.
    control.reload().await.unwrap();
    std::fs::write(Rig::paths(rig.dir.path()).config(), "this is not toml [").unwrap();
    let error = control.reload().await.unwrap_err().to_string();
    assert!(error.contains("HTTP 500: "), "{error}");
    assert!(error.contains("config.toml"), "{error}");
    assert_eq!(
        rig.call("eth", "eth_blockNumber", json!([])).await.status,
        200
    );

    // Shutdown: files withdrawn, lock released, log flushed.
    rig.shutdown().await;
    assert_eq!(runtime.gateway().unwrap(), GatewayState::Down);
    assert!(!runtime.serve_json_path().exists());
    assert!(!runtime.serve_sock_path().exists());
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(text.lines().count(), 7);
    let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(first["params_summary"], "@usdc.balanceOf latest");
    // Nothing secret reached the inspector: no raw transaction, no URL, no token.
    let everything = format!("{text}{records:?}{status:?}{metrics:?}");
    for secret in [&RAW_TX[2..40], "urlsekrit", info.token.as_str()] {
        assert!(!everything.contains(secret), "leaked {secret}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_in_flight_requests() {
    let a = Mock::start("a", |_| {
        R::Delay(
            Duration::from_millis(400),
            Box::new(R::Result(json!("slow"))),
        )
    })
    .await;
    let mut rig = Rig::start(&eth_config("a", &[], &[("a", a.url())])).await;
    let path = rig.path("eth", None);
    let host = rig.host();
    let port = rig.port;
    let in_flight = tokio::spawn(async move {
        http(
            port,
            "POST",
            &path,
            &[("Host", &host), ("Content-Type", "application/json")],
            r#"{"jsonrpc":"2.0","id":1,"method":"eth_call","params":[]}"#,
        )
        .await
    });
    eventually(|| a.calls().len() == 1).await;
    let started = Instant::now();
    rig.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let reply = in_flight.await.unwrap();
    assert_eq!(
        (reply.status, reply.json()["result"].as_str()),
        (200, Some("slow"))
    );
    // And the port is closed afterwards.
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_already_reloaded_is_not_reloaded_again_by_the_watch() {
    let a = Mock::named("a").await;
    let watch = Duration::from_millis(500);
    let rig = Rig::start_with(&eth_config("a", &[], &[("a", a.url())]), |config| {
        config.watch_interval = watch;
    })
    .await;
    let mut notices = rig.gateway().notices();
    // Changed, then reloaded (as SIGHUP does) before the watch looks.
    Rig::write_config(
        rig.dir.path(),
        &eth_config("a", &[], &[("a", a.url()), ("b", a.url())]),
    );
    rig.gateway().reload().await.unwrap();
    let mut reloads = || {
        let mut n = 0;
        while let Ok(notice) = notices.try_recv() {
            n += usize::from(notice.contains("reloaded"));
        }
        n
    };
    // A watch tick between the write and the reload may reload first; the
    // explicit reload then reloads again, as SIGHUP always does.
    assert!(reloads() >= 1);
    tokio::time::sleep(watch * 3).await;
    assert_eq!(reloads(), 0, "the watch reloaded an unchanged config");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_and_headers_are_applied_upstream_and_follow_reloads() {
    // The mock echoes nothing; inspect what it received.
    let a = Mock::named("a").await;
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = {{ url = \"{}/${{GW_TEST_PATH}}\", headers = {{ x-key = \"${{GW_TEST_HEADER}}\" }} }}\n",
        a.url()
    );
    Rig::write_config(dir.path(), &config);
    let secrets = dir.path().join("config/secrets.toml");
    let write_secrets = |path_value: &str| {
        use std::os::unix::fs::PermissionsExt;
        let tmp = secrets.with_extension("tmp");
        std::fs::write(
            &tmp,
            format!("[vars]\nGW_TEST_PATH = \"{path_value}\"\nGW_TEST_HEADER = \"hdr\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&tmp, &secrets).unwrap();
    };
    write_secrets("one");
    let gateway = chainz_serve::Gateway::start(Rig::config(dir.path()))
        .await
        .unwrap();
    let port = gateway.port();
    let rig = Rig {
        dir,
        gateway: Some(gateway),
        port,
    };
    assert_eq!(rig.call("eth", "eth_chainId", json!([])).await.status, 200);
    // A changed secret is picked up by the file watch, though the file
    // has the same length (and may well have the same mtime).
    let mut notices = rig.gateway().notices();
    write_secrets("two");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !notices.recv().await.unwrap().contains("reloaded") {}
    })
    .await
    .expect("no reload");
    assert_eq!(rig.call("eth", "eth_chainId", json!([])).await.status, 200);
    let heads = a.heads();
    assert_eq!(heads.len(), 2);
    assert!(
        heads[0].starts_with("post /one ") && heads[1].starts_with("post /two "),
        "{heads:?}"
    );
    assert!(heads.iter().all(|h| h.contains("x-key: hdr")), "{heads:?}");
    // The client's own headers (its Host, the gateway token) stay behind.
    let token = rig.path("eth", None);
    assert!(
        !heads
            .iter()
            .any(|h| h.contains(token.trim_start_matches('/').split('/').next().unwrap()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_size_config_change_with_the_same_mtime_is_seen() {
    let (a, b) = (Mock::named("a").await, Mock::named("b").await);
    let endpoints = [("a", a.url()), ("b", b.url())];
    let rig = Rig::start(&eth_config("a", &[], &endpoints)).await;
    let config = Rig::paths(rig.dir.path()).config().to_path_buf();
    let before = std::fs::metadata(&config).unwrap();
    // `chainz update eth --rpc b`: the same length, and here the same mtime too
    // (as two writes within one timestamp tick would have).
    let next = eth_config("b", &[], &endpoints);
    assert_eq!(next.len() as u64, before.len());
    let tmp = config.with_extension("tmp");
    std::fs::write(&tmp, &next).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let file = std::fs::File::options().write(true).open(&tmp).unwrap();
    file.set_modified(before.modified().unwrap()).unwrap();
    drop(file);
    std::fs::rename(&tmp, &config).unwrap();
    eventually(|| rig.gateway().status().chains[0].active == "b").await;
    assert_eq!(
        rig.call("eth", "eth_chainId", json!([])).await.json()["result"],
        "b"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_nowhere_else_to_go_requests_queue_in_the_bucket() {
    // One 429 teaches the bucket a rate (2 req/s); a burst after that is
    // spaced out by the bucket, none held back to fire all at once.
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let a = Mock::start("a", move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            R::Http(429, Some("1"))
        } else {
            R::Result(json!("a"))
        }
    })
    .await;
    let rig = Arc::new(Rig::start(&eth_config("a", &[], &[("a", a.url())])).await);
    assert_eq!(rig.call("eth", "eth_call", json!([])).await.status, 200);
    let burst: Vec<_> = (0..8)
        .map(|_| {
            let rig = rig.clone();
            tokio::spawn(async move { rig.call("eth", "eth_call", json!([])).await.status })
        })
        .collect();
    for request in burst {
        assert_eq!(request.await.unwrap(), 200);
    }
    let arrivals = a.arrivals();
    assert_eq!(arrivals.len(), 10);
    let mut burst = arrivals[2..].to_vec();
    burst.sort();
    // Old behaviour: the requests the bucket couldn't fit in 2 s slept 2 s
    // and then all went at once. Now they go about 1/rate apart. (Two may
    // land close together: a success that raises the rate mid-burst moves
    // a later reservation earlier.)
    let offsets: Vec<_> = arrivals.iter().map(|t| *t - arrivals[0]).collect();
    for three in burst.windows(3) {
        let span = three[2] - three[0];
        assert!(
            span >= Duration::from_millis(300),
            "three sends within {span:?}: {offsets:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_after_a_429_go_through_the_bucket() {
    // Eight concurrent reads all get "429, retry after 1 s". Their retries
    // take bucket slots rather than all firing together a second later.
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let a = Mock::start("a", move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) < 8 {
            // Slow enough that all eight are in before the first 429 lands.
            R::Delay(
                Duration::from_millis(200),
                Box::new(R::Http(429, Some("1"))),
            )
        } else {
            R::Result(json!("a"))
        }
    })
    .await;
    let rig = Arc::new(Rig::start(&eth_config("a", &[], &[("a", a.url())])).await);
    let herd: Vec<_> = (0..8)
        .map(|_| {
            let rig = rig.clone();
            tokio::spawn(async move { rig.call("eth", "eth_call", json!([])).await.status })
        })
        .collect();
    for request in herd {
        assert_eq!(request.await.unwrap(), 200);
    }
    let arrivals = a.arrivals();
    assert_eq!(arrivals.len(), 16);
    let mut retries = arrivals[8..].to_vec();
    retries.sort();
    let spread = *retries.last().unwrap() - retries[0];
    assert!(
        spread >= Duration::from_millis(500),
        "retries came in {spread:?}"
    );
    // One decrease for the whole burst of 429s, not eight.
    let metrics = rig.control().metrics().await.unwrap();
    let row = &metrics.windows[0].rows[0];
    assert_eq!((row.requests, row.rate_limited), (16, 8));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_serve_json_write_leaves_nothing_running() {
    let a = Mock::named("a").await;
    let dir = tempfile::tempdir().unwrap();
    Rig::write_config(dir.path(), &eth_config("a", &[], &[("a", a.url())]));
    // serve.json can't be written: a directory is in its place.
    let runtime = Rig::runtime_at(dir.path());
    std::fs::create_dir_all(runtime.serve_json_path().join("blocker")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(runtime.dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut config = Rig::config(dir.path());
    config.port = port;
    let error = chainz_serve::Gateway::start(config).await.unwrap_err();
    assert!(format!("{error:#}").contains("serve.json"), "{error:#}");
    // Nothing kept the port, the socket or the lock.
    std::net::TcpListener::bind(("127.0.0.1", port)).expect("the port is still held");
    assert!(!runtime.serve_sock_path().exists());
    assert_eq!(runtime.gateway().unwrap(), GatewayState::Down);
}
