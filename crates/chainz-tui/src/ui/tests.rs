//! Render tests: every tab, in its normal, empty, error and narrow states,
//! drawn to ratatui's `TestBackend` from a fixed state and clock.

use chainz_rpc::{ErrorClass, HealthSample, HealthStore};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use super::*;
use crate::app::{App, Tab};
use crate::data::testing::{chain, invalidate, plan};
use crate::gateway::testing as gw;
use crate::model::{Fault, GatewayInfo, Head, Holding, Sheet, State, Update, Via};
use crate::plan::{Plan, Source};

/// The frame clock: unix milliseconds.
pub(crate) const NOW: u64 = 1_700_000_000_000;

fn fixture_plan() -> std::sync::Arc<Plan> {
    let eth = chain(
        "eth",
        1,
        &[
            ("infura", "https://mainnet.infura.io/v3/0123456789abcdef"),
            ("llama", "https://eth.llamarpc.com"),
            ("publicnode", "https://ethereum-rpc.publicnode.com"),
            (
                "quiknode",
                "https://quiet-dawn.quiknode.pro/0123456789abcdef0123/",
            ),
            (
                "alchemy-ws",
                "wss://eth-mainnet.g.alchemy.com/v2/s3cr3t0123456789ab",
            ),
            ("broken", "https://rpc.example.org"),
        ],
    );
    let mut eth = eth;
    eth.info.aliases = vec!["ethereum".into(), "mainnet".into()];
    invalidate(&mut eth.endpoints[5], "variable ${EXAMPLE_KEY} is not set");
    let mut zora = chain("zora", 7777777, &[("uniswap", "https://zora.example.org")]);
    zora.info.tokens.remove(0); // no usdc on zora
    let mut polygon = chain("polygon", 137, &[("infura", "https://polygon.example.org")]);
    polygon.info.native_symbol = "POL";
    plan(vec![
        eth,
        chain("base", 8453, &[("infura", "https://base.example.org")]),
        chain("uni", 130, &[("infura", "https://uni.example.org")]),
        zora,
        chain("arb", 42161, &[("uniswap", "https://arb.example.org")]),
        polygon,
    ])
}

fn head(chain: &str, number: u64, age_secs: u64, block_time: f64, fee: u128, ms: u32) -> Update {
    Update::Head {
        chain: chain.into(),
        source: Source::named(chain),
        at_ms: NOW,
        via_gateway: false,
        result: Ok(Head {
            number,
            timestamp: Some(NOW / 1_000 - age_secs),
            base_fee: Some(fee),
            latency_ms: ms,
        }),
        block_time: Some(block_time),
    }
}

/// One endpoint's result in a probe round at `at_ms`.
fn probe(
    endpoint: &str,
    at_ms: u64,
    ms: u32,
    head: Option<u64>,
    error: Option<(ErrorClass, &str)>,
) -> (String, HealthSample, Option<String>) {
    let sample = HealthSample {
        at_ms,
        latency_ms: ms,
        head,
        error: error.map(|(class, _)| class),
    };
    (
        endpoint.into(),
        sample,
        error.map(|(_, message)| message.into()),
    )
}

fn sheet(via: Via, default: (u128, u128, u128), funder: (u128, u128, u128)) -> Sheet {
    let holding = |signer: &str, (native, usdc, weth): (u128, u128, u128)| Holding {
        signer: signer.into(),
        native: Some(native),
        tokens: vec![Some(usdc), Some(weth)],
    };
    Sheet {
        via,
        holders: vec![holding("default", default), holding("funder", funder)],
    }
}

/// Six chains in every state the Chains tab can show.
pub(crate) fn fixture() -> State {
    let mut state = State::new(&fixture_plan(), &HealthStore::empty());
    let eth = 1_000_000_000_000_000_000u128;
    let updates = vec![
        // eth: its endpoints probed, one of them ahead of nobody.
        Update::Probes {
            chain: "eth".into(),
            source: Source::named("eth"),
            at_ms: NOW - 3_000,
            results: vec![
                probe("infura", NOW - 3_000, 48, Some(23_481_220), None),
                probe("llama", NOW - 3_000, 131, Some(23_481_186), None),
                probe(
                    "publicnode",
                    NOW - 3_000,
                    52,
                    None,
                    Some((ErrorClass::RateLimited, "rate limited (retry after 30s)")),
                ),
                probe("quiknode", NOW - 3_000, 1_240, Some(23_481_219), None),
            ],
        },
        head("eth", 23_481_220, 4, 12.0, 410_000_000, 48),
        head("eth", 23_481_220, 4, 12.0, 410_000_000, 52),
        head("eth", 23_481_220, 4, 12.0, 410_000_000, 47),
        head("eth", 23_481_220, 4, 12.0, 410_000_000, 96),
        head("eth", 23_481_220, 4, 12.0, 410_000_000, 48),
        head("base", 38_112_904, 1, 2.0, 2_000_000, 61),
        head("base", 38_112_904, 1, 2.0, 2_000_000, 60),
        // uni: three blocks behind what a peer reported.
        Update::Probes {
            chain: "uni".into(),
            source: Source::named("uni"),
            at_ms: NOW - 9_000,
            results: vec![probe("infura", NOW - 9_000, 55, Some(31_770_016), None)],
        },
        head("uni", 31_770_013, 0, 1.0, 400, 55),
        // zora: answering, slowly.
        head("zora", 33_004_118, 9, 2.0, 1_000_000, 900),
        head("zora", 33_004_118, 9, 2.0, 1_000_000, 240),
        head("zora", 33_004_118, 9, 2.0, 1_000_000, 260),
        // arb: was fine, now timing out.
        head("arb", 284_000_117, 62, 0.25, 10_000_000, 80),
        Update::Head {
            chain: "arb".into(),
            source: Source::named("arb"),
            at_ms: NOW,
            via_gateway: false,
            result: Err(Fault::new(ErrorClass::Timeout, "request timed out")),
            block_time: None,
        },
        // polygon: not polled yet.
        Update::Balances {
            chain: "eth".into(),
            source: Source::named("eth"),
            at_ms: NOW - 12_000,
            result: Ok(sheet(
                Via::Multicall3,
                (1_234_500_000_000_000_000, 12_345_678_900, 0),
                (eth / 20, 0, 0),
            )),
        },
        Update::Balances {
            chain: "base".into(),
            source: Source::named("base"),
            at_ms: NOW - 12_000,
            result: Ok(sheet(
                Via::Multicall3,
                (eth / 2, 250_000_000, eth / 8),
                (0, 0, 0),
            )),
        },
        Update::Balances {
            chain: "uni".into(),
            source: Source::named("uni"),
            at_ms: NOW - 11_000,
            result: Ok(sheet(Via::Batch, (0, 0, 0), (1, 5, 0))),
        },
        Update::Balances {
            chain: "zora".into(),
            source: Source::named("zora"),
            at_ms: NOW - 11_000,
            result: Ok(Sheet {
                via: Via::Batch,
                holders: vec![Holding {
                    signer: "default".into(),
                    native: Some(eth / 1_000),
                    tokens: vec![None],
                }],
            }),
        },
        Update::Balances {
            chain: "arb".into(),
            source: Source::named("arb"),
            at_ms: NOW - 2_000,
            result: Err(Fault::new(ErrorClass::Timeout, "request timed out")),
        },
    ];
    for update in updates {
        state.apply(update);
    }
    state
}

pub(crate) fn with_gateway(mut state: State) -> State {
    state.apply(Update::Gateway(Some(GatewayInfo { port: 18545 })));
    state.apply(Update::Traffic(Ok(gw::traffic())));
    for item in gw::feed() {
        state.apply(Update::Feed(item));
    }
    state
}

fn app(tab: Tab) -> App {
    let mut app = App::default();
    app.tab = tab;
    app
}

fn ctx<'a>(app: &'a App, state: &'a State) -> Ctx<'a> {
    Ctx {
        app,
        state,
        theme: Theme::new(false),
        now_ms: NOW,
    }
}

fn frame(app: &App, state: &State, width: u16, height: u16) -> String {
    render_text(ctx(app, state), width, height)
}

/// The frame's cells, drawn through a real `Terminal` with `theme`.
fn buffer(app: &App, state: &State, theme: Theme, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let ctx = Ctx {
        theme,
        ..ctx(app, state)
    };
    terminal.draw(|f| draw(f, ctx)).unwrap();
    terminal.backend().buffer().clone()
}

/// Compare a frame with a literal that starts on the line after `r"`.
#[track_caller]
fn assert_frame(actual: &str, expected: &str) {
    let expected = expected.strip_prefix('\n').unwrap_or(expected);
    assert!(
        actual == expected,
        "frame differs\n── actual ──\n{actual}── expected ──\n{expected}"
    );
}

#[test]
fn chains_tab_shows_every_state_aligned() {
    assert_frame(
        &frame(&app(Tab::Chains), &fixture(), 110, 10),
        r"
 chainz top ─ [1] Chains  [2] Endpoints  [3] Balances  [4] Traffic                                 serve: off
 6 chains · 2 ok · 1 lag · 1 slow · 1 down · 1 pending
 CHAIN    ID              HEAD  AGE  BLOCK     BASE FEE  RPC        P50  HEALTH    STATUS
▸eth      1         23,481,220   4s  12.0s   0.410 gwei  infura    48ms     ▁▁▁▆▁  ok
 base     8453      38,112,904   1s   2.0s   0.002 gwei  infura    60ms        ▁▁  ok
 uni      130       31,770,013   0s   1.0s  <0.001 gwei  infura    55ms         ▁  lag 3
 zora     7777777   33,004,118   9s   2.0s   0.001 gwei  uniswap  260ms       █▁▁  slow
 arb      42161    284,000,117  62s  0.25s   0.010 gwei  uniswap   80ms        ▁×  down timeout
 polygon  137                …    –      –            –  infura       –            …
 enter: endpoints  b: balances  r: refresh  s: set rpc  e: exec shell  /: filter  y: copy url  q: quit
",
    );
}

#[test]
fn chains_tab_narrows_by_dropping_columns_never_rows() {
    let state = fixture();
    assert_frame(
        &frame(&app(Tab::Chains), &state, 72, 10),
        r"
 [1] Chains  [2] Endpoints  [3] Balances  [4] Traffic        serve: off
 6 chains · 2 ok · 1 lag · 1 slow · 1 down · 1 pending
 CHAIN           HEAD  AGE     BASE FEE  RPC        P50  STATUS
▸eth       23,481,220   4s   0.410 gwei  infura    48ms  ok
 base      38,112,904   1s   0.002 gwei  infura    60ms  ok
 uni       31,770,013   0s  <0.001 gwei  infura    55ms  lag 3
 zora      33,004,118   9s   0.001 gwei  uniswap  260ms  slow
 arb      284,000,117  62s   0.010 gwei  uniswap   80ms  down timeout
 polygon            …    –            –  infura       –  …
 enter: endpoints  b: balances  r: refresh  s: set rpc  q: quit
",
    );
    assert_frame(
        &frame(&app(Tab::Chains), &state, 44, 10),
        r"
 [1] Chains [2] [3] [4]              gw off
 6 chains · 2 ok · 1 lag · 1 slow · 1 down
 CHAIN           HEAD  AGE  STATUS
▸eth       23,481,220   4s  ok
 base      38,112,904   1s  ok
 uni       31,770,013   0s  lag 3
 zora      33,004,118   9s  slow
 arb      284,000,117  62s  down timeout
 polygon            …    –  …
 enter: endpoints  b: balances  q: quit
",
    );
    // At the minimum size: name, head and status survive.
    let tiny = frame(&app(Tab::Chains), &state, 28, 5);
    assert!(tiny.contains("▸eth       23,481,220  ok"), "{tiny}");
    // Below it, say so instead of drawing garbage.
    assert!(frame(&app(Tab::Chains), &state, 27, 10).contains("terminal too small"));
    assert!(frame(&app(Tab::Chains), &state, 80, 4).contains("terminal too small"));
}

#[test]
fn chains_tab_empty_filtered_and_scrolled() {
    // No chains configured.
    let nothing = State::new(&plan(vec![]), &HealthStore::empty());
    let text = frame(&app(Tab::Chains), &nothing, 80, 12);
    assert!(text.contains("No chains configured yet."), "{text}");
    assert!(text.contains("chainz add base"), "{text}");

    // A filter narrows the list and says so; it matches names, aliases, IDs.
    let state = fixture();
    let mut filtered = app(Tab::Chains);
    filtered.filter = "mainnet".into();
    let text = frame(&filtered, &state, 100, 10);
    assert!(text.contains("filter /mainnet (1 shown)"), "{text}");
    assert!(text.contains("▸eth") && !text.contains("base"), "{text}");
    filtered.filter = "84".into();
    assert!(frame(&filtered, &state, 100, 10).contains("▸base"));
    filtered.filter = "nope".into();
    let text = frame(&filtered, &state, 100, 10);
    assert!(text.contains("No chain matches /nope"), "{text}");

    // While typing, the footer is the prompt.
    filtered.filter = "ba".into();
    filtered.filtering = true;
    let text = frame(&filtered, &state, 100, 10);
    assert!(
        text.lines()
            .last()
            .unwrap()
            .starts_with(" /ba▏  enter: keep  esc: clear"),
        "{text}"
    );

    // More rows than fit: the selection stays visible and the range shows.
    let mut low = app(Tab::Chains);
    low.chain = Some("arb".into());
    let text = frame(&low, &state, 100, 7);
    assert!(text.contains("3–5 of 6"), "{text}");
    assert!(text.contains("▸arb") && !text.contains("eth "), "{text}");
}

#[test]
fn endpoints_tab_shows_health_errors_and_tags() {
    let state = with_gateway(fixture());
    assert_frame(
        &frame(&app(Tab::Endpoints), &state, 130, 11),
        r"
 chainz top ─ [1] Chains  [2] Endpoints  [3] Balances  [4] Traffic                                               serve: on :18545
 eth (1) · 6 endpoints · active infura · probed 3s ago
 ENDPOINT      STATUS     LATENCY    P50        HEAD  LAG  ERROR                               TAGS
▸● infura      ok            48ms   48ms  23,481,220    0
   llama       lagging      131ms  131ms  23,481,186   34
   publicnode  failing          –      –           –    –  rate limited (retry after 30s)
   quiknode    ok            1.2s   1.2s  23,481,219    1
   alchemy-ws  websocket        –      –           –    –                                      ws
   broken      invalid          –      –           –    –  variable ${EXAMPLE_KEY} is not set

 s: set rpc  h/l: chain  r: probe now  y: copy url  e: exec shell  esc: back  q: quit
",
    );
    // Narrow: status, latency and the error survive.
    assert_frame(
        &frame(&app(Tab::Endpoints), &state, 70, 11),
        r"
 [1] Chains  [2] Endpoints  [3] Balances  [4] Traffic    gw on :18545
 eth (1) · 6 endpoints · active infura · probed 3s ago
 ENDPOINT      STATUS     LATENCY  ERROR
▸● infura      ok            48ms
   llama       lagging      131ms
   publicnode  failing          –  rate limited (retry after 30s)
   quiknode    ok            1.2s
   alchemy-ws  websocket        –
   broken      invalid          –  variable ${EXAMPLE_KEY} is not set

 s: set rpc  h/l: chain  r: probe now  y: copy url  q: quit
",
    );
}

#[test]
fn endpoints_tab_redacts_urls_and_follows_the_selection() {
    let state = fixture();
    // Wide enough for the URL column: hosts and paths readable, never the
    // credential.
    let text = frame(&app(Tab::Endpoints), &state, 220, 11);
    assert!(
        text.contains("https://mainnet.infura.io/v3/[REDACTED]"),
        "{text}"
    );
    assert!(
        text.contains("wss://eth-mainnet.g.alchemy.com/v2/[REDACTED]"),
        "{text}"
    );
    assert!(
        !text.contains("s3cr3t") && !text.contains("0123456789abcdef"),
        "{text}"
    );

    // Another chain, not probed yet: say so rather than show blanks as facts.
    let mut other = app(Tab::Endpoints);
    other.chain = Some("zora".into());
    let text = frame(&other, &state, 100, 8);
    assert!(
        text.contains("zora (7777777) · 1 endpoints · active uniswap · probing…"),
        "{text}"
    );
    assert!(text.contains("▸● uniswap  …"), "{text}");

    // The second endpoint selected.
    let mut second = app(Tab::Endpoints);
    second.endpoint = 1;
    assert!(frame(&second, &state, 100, 11).contains("▸  llama"));

    // No chains at all.
    let nothing = State::new(&plan(vec![]), &HealthStore::empty());
    assert!(frame(&app(Tab::Endpoints), &nothing, 80, 10).contains("No chain selected."));
}

#[test]
fn endpoints_tab_marks_cached_health_as_cached() {
    // Health from an earlier `doctor CHAIN`, nothing probed in this session.
    let dir = tempfile::tempdir().unwrap();
    let mut store = HealthStore::load(dir.path()).unwrap();
    let sample = |latency_ms, head| chainz_rpc::HealthSample {
        at_ms: NOW - 600_000,
        latency_ms,
        head: Some(head),
        error: None,
    };
    store.record(1, "infura", sample(40, 23_000_000));
    store.record(1, "infura", sample(60, 23_000_050));
    store.record(1, "llama", sample(120, 23_000_050));
    store.set_capabilities(
        1,
        "llama",
        chainz_rpc::Capabilities {
            archive: Some(true),
            debug_trace: Some(true),
            max_logs_range: Some(10_000),
            ..Default::default()
        },
    );
    let state = State::new(&fixture_plan(), &store);
    let text = frame(&app(Tab::Endpoints), &state, 130, 11);
    assert!(text.contains("cached health · probing…"), "{text}");
    assert!(
        text.contains("▸● infura      ok            60ms   40ms  23,000,050"),
        "{text}"
    );
    assert!(text.contains("archive debug logs:10000"), "{text}");
    // The Chains sparkline starts from the same history.
    let chains = frame(&app(Tab::Chains), &state, 110, 10);
    assert!(chains.contains("40ms        ▁▃  …"), "{chains}");
}

#[test]
fn balances_tab_formats_with_token_decimals() {
    let state = fixture();
    assert_frame(
        &frame(&app(Tab::Balances), &state, 100, 10),
        r"
 chainz top ─ [1] Chains  [2] Endpoints  [3] Balances  [4] Traffic                       serve: off
 signer ▸default 0x7E5F…5Bdf    funder 0x2B5A…D6cF    cold no address
 CHAIN        NATIVE   @weth        @usdc  VIA         UPDATED
▸eth      1.2345 ETH       0  12,345.6789  multicall3  12s ago
 base     0.5000 ETH  0.1250     250.0000  multicall3  12s ago
 uni           0 ETH       0            0  batch       11s ago
 zora     0.0010 ETH       ?               batch       11s ago
 arb               –       –            –              timeout
 polygon           …       …            …              …
 h/l: signer  r: refresh  y: copy address  /: filter  e: exec shell  q: quit
",
    );
    // The second signer: its own numbers, and no sheet where it has none.
    let mut funder = app(Tab::Balances);
    funder.signer = 1;
    let text = frame(&funder, &state, 100, 10);
    assert!(
        text.contains("default 0x7E5F…5Bdf   ▸funder 0x2B5A…D6cF"),
        "{text}"
    );
    assert!(
        text.contains("▸eth       0.0500 ETH      0        0  multicall"),
        "{text}"
    );
    assert!(
        text.contains(" uni      <0.0001 ETH      0  <0.0001  batch"),
        "{text}"
    );
    // Narrow: how and when go first, then tokens from the right.
    assert_frame(
        &frame(&app(Tab::Balances), &state, 50, 10),
        r"
 [1] [2] [3] Balances [4]                  gw off
 signer ▸default 0x7E5F…5Bdf    funder 0x2B5A…D6cF
 CHAIN        NATIVE   @weth        @usdc  UPDATED
▸eth      1.2345 ETH       0  12,345.6789  12s ago
 base     0.5000 ETH  0.1250     250.0000  12s ago
 uni           0 ETH       0            0  11s ago
 zora     0.0010 ETH       ?               11s ago
 arb               –       –            –  timeout
 polygon           …       …            …  …
 h/l: signer  r: refresh  q: quit
",
    );
    let text = frame(&app(Tab::Balances), &state, 34, 10);
    assert!(text.contains(" CHAIN        NATIVE   @weth"), "{text}");
    assert!(!text.contains("@usdc"), "{text}");
}

#[test]
fn balances_tab_empty_states() {
    // A signer without a cached address is never unlocked to get one.
    let mut cold = app(Tab::Balances);
    cold.signer = 2;
    let text = frame(&cold, &fixture(), 100, 10);
    assert!(
        text.contains("Signer 'cold' has no cached address."),
        "{text}"
    );

    let mut bare = (*fixture_plan()).clone();
    bare.signers.clear();
    let state = State::new(&bare, &HealthStore::empty());
    let text = frame(&app(Tab::Balances), &state, 100, 12);
    assert!(text.contains("No signer has a cached address."), "{text}");
    assert!(text.contains("chainz signer add NAME"), "{text}");
    assert!(text.contains("Balances never unlock a key."), "{text}");

    let mut filtered = app(Tab::Balances);
    filtered.filter = "nope".into();
    assert!(frame(&filtered, &fixture(), 100, 10).contains("No chain matches /nope"));
}

#[test]
fn traffic_tab_without_a_gateway_says_how_to_start_one() {
    assert_frame(
        &frame(&app(Tab::Traffic), &fixture(), 100, 12),
        r"
 chainz top ─ [1] Chains  [2] Endpoints  [3] Balances  [4] Traffic                       serve: off


                                       No gateway is running.

                                    Start one with  chainz serve

          exec, your tools and this dashboard then share one failover-aware URL per chain,
                                  and every request shows up here.


 r: refresh  1-4: tabs  q: quit
",
    );
}

#[test]
fn traffic_tab_shows_rate_feed_methods_and_endpoints() {
    let state = with_gateway(fixture());
    assert_frame(
        &frame(&app(Tab::Traffic), &state, 140, 16),
        r"
 chainz top ─ [1] Chains  [2] Endpoints  [3] Balances  [4] Traffic                                                         serve: on :18545
 0.3 req/s · 742 req/1m · 3 errors · 12 × 429 · 1 failover · up 2h03m · v2.0.0-test
 LIVE REQUESTS                                                                             TOP METHODS · 1m
 AGO  CHAIN  METHOD           DETAIL                       VIA        MS  STATUS           METHOD           REQS  ERR    P50    P95
  0s  base   eth_call         @usdc.balanceOf(@wallet)     infura     61  ok               eth_call          600    0   50ms  210ms
  0s  base   eth_blockNumber                               infura     61  ok               eth_getLogs       120    3  210ms  630ms
  0s  eth    eth_getLogs      @usdc Transfer 2,000 blocks  llama   4,000  rate_limited ×2  eth_blockNumber    22    0   31ms   93ms

                                                                                           ENDPOINTS · 1m
                                                                                           ENDPOINT     REQS  ERR  429  FAILOVER    P50
                                                                                           eth/infura    422    0    0         0   39ms
                                                                                           base/infura   200    0   12         1   70ms
                                                                                           eth/llama     120    3    0         0  210ms


 r: refresh  1-4: tabs  q: quit
",
    );
    // Narrower: the sections stack.
    assert_frame(
        &frame(&app(Tab::Traffic), &state, 90, 24),
        r"
 chainz top ─ [1] Chains  [2] Endpoints  [3] Balances  [4] Traffic       serve: on :18545
 0.3 req/s · 742 req/1m · 3 errors · 12 × 429 · 1 failover · up 2h03m · v2.0.0-test
 LIVE REQUESTS
 AGO  CHAIN  METHOD           DETAIL                       VIA        MS  STATUS
  0s  base   eth_call         @usdc.balanceOf(@wallet)     infura     61  ok
  0s  base   eth_blockNumber                               infura     61  ok
  0s  eth    eth_getLogs      @usdc Transfer 2,000 blocks  llama   4,000  rate_limited ×2





 TOP METHODS · 1m
 METHOD           REQS  ERR    P50    P95
 eth_call          600    0   50ms  210ms
 eth_getLogs       120    3  210ms  630ms

 ENDPOINTS · 1m
 ENDPOINT     REQS  ERR  429  FAILOVER    P50
 eth/infura    422    0    0         0   39ms
 base/infura   200    0   12         1   70ms
 eth/llama     120    3    0         0  210ms

 r: refresh  1-4: tabs  q: quit
",
    );
    // Narrow and short: the feed alone, without its detail column.
    assert_frame(
        &frame(&app(Tab::Traffic), &state, 60, 12),
        r"
 [1] [2] [3] [4] Traffic                       gw on :18545
 0.3 req/s · 742 req/1m · 3 errors · 12 × 429 · 1 failover
 LIVE REQUESTS
 AGO  CHAIN  METHOD           VIA        MS  STATUS
  0s  base   eth_call         infura     61  ok
  0s  base   eth_blockNumber  infura     61  ok
  0s  eth    eth_getLogs      llama   4,000  rate_limited ×2




 r: refresh  1-4: tabs  q: quit
",
    );
}

#[test]
fn traffic_tab_connecting_idle_and_error_states() {
    // The lock is held but the control socket has not answered yet.
    let mut state = fixture();
    state.apply(Update::Gateway(Some(GatewayInfo { port: 18545 })));
    let text = frame(&app(Tab::Traffic), &state, 100, 12);
    assert!(text.contains("Connecting to the gateway…"), "{text}");

    // It never answers.
    state.apply(Update::Traffic(Err("connection refused".into())));
    let text = frame(&app(Tab::Traffic), &state, 100, 12);
    assert!(
        text.contains("The gateway's control socket did not answer."),
        "{text}"
    );
    assert!(text.contains("connection refused"), "{text}");

    // Running, no requests yet.
    state.apply(Update::Traffic(Ok(gw::traffic())));
    let text = frame(&app(Tab::Traffic), &state, 140, 16);
    assert!(text.contains("0.0 req/s"), "{text}");
    assert!(text.contains("waiting for requests…"), "{text}");

    // A later poll fails: keep the last numbers, flag them stale.
    state.apply(Update::Traffic(Err("timed out".into())));
    let text = frame(&app(Tab::Traffic), &state, 140, 16);
    assert!(
        text.contains("stale: timed out · 0.0 req/s · 742 req/1m"),
        "{text}"
    );

    // The gateway stops: back to the hint, nothing left over.
    state.apply(Update::Gateway(None));
    assert!(frame(&app(Tab::Traffic), &state, 100, 12).contains("No gateway is running."));
}

#[test]
fn footer_shows_notices_and_sheds_hints_to_fit() {
    let state = fixture();
    let mut noted = app(Tab::Chains);
    noted.notify("copied eth.infura URL", false, NOW);
    let text = frame(&noted, &state, 110, 10);
    let footer = text.lines().last().unwrap();
    assert!(footer.ends_with("copied eth.infura URL"), "{footer}");
    assert!(
        footer.starts_with(" enter: endpoints  b: balances"),
        "{footer}"
    );
    assert!(
        footer.contains("q: quit"),
        "quit is the last hint to go: {footer}"
    );
    // An expired notice goes away.
    assert!(noted.expire(NOW + 60_000));
    assert!(!frame(&noted, &state, 110, 10).contains("copied"));
}

#[test]
fn colours_carry_meaning_and_no_color_removes_them() {
    let state = fixture();
    let buf = buffer(&app(Tab::Chains), &state, Theme::new(true), 110, 10);
    // Find a cell by the text that starts there, on row `y`.
    let at = |y: u16, needle: &str| {
        let line: String = (0..110).map(|x| buf[(x, y)].symbol()).collect();
        let byte = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} not on row {y}"));
        let x = u16::try_from(line[..byte].chars().count()).unwrap();
        buf[(x, y)].clone()
    };
    assert_eq!(at(3, "ok").fg, Color::Green);
    assert_eq!(at(5, "lag 3").fg, Color::Magenta);
    assert_eq!(at(6, "slow").fg, Color::Yellow);
    assert_eq!(at(7, "down timeout").fg, Color::Red);
    assert_eq!(at(7, "×").fg, Color::Red);
    // The selected row is highlighted across its whole width.
    assert_eq!(at(3, "▸").fg, Color::Cyan);
    assert!((0..110).all(|x| buf[(x, 3)].bg == Color::Indexed(237)));
    assert!((0..110).all(|x| buf[(x, 4)].bg == Color::Reset));
    // The active tab stands out.
    assert!(at(0, "[1] Chains").modifier.contains(Modifier::UNDERLINED));
    assert!(
        !at(0, "[2] Endpoints")
            .modifier
            .contains(Modifier::UNDERLINED)
    );

    // NO_COLOR: not one coloured cell on any tab; selection by reverse.
    let live = with_gateway(fixture());
    for tab in Tab::ALL {
        let buf = buffer(&app(tab), &live, Theme::new(false), 140, 16);
        for cell in buf.content() {
            assert_eq!((cell.fg, cell.bg), (Color::Reset, Color::Reset), "{tab:?}");
        }
    }
    let plain = buffer(&app(Tab::Chains), &state, Theme::new(false), 110, 10);
    assert!(plain[(5, 3)].modifier.contains(Modifier::REVERSED));
}
