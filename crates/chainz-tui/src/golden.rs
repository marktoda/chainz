//! Golden tests for `--once`: the text tables and the JSON document, from
//! fixed states and a fixed clock. Regenerate with `CHAINZ_BLESS=1`.

use std::path::PathBuf;

use chainz_core::StorePaths;
use chainz_core::{Chainz, LoadMode};
use chainz_rpc::{ErrorClass, HealthStore};

use crate::data::testing::{chains_only, plan, short_tempdir};
use crate::model::{Fault, Head, Holding, Sheet, State, Update, Via};
use crate::plan::Plan;
use crate::ui::tests::{NOW, fixture, with_gateway};

fn golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("CHAINZ_BLESS").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run with CHAINZ_BLESS=1 to create it",
            path.display()
        )
    });
    assert!(
        expected == actual,
        "{name} differs (CHAINZ_BLESS=1 regenerates it)\n── actual ──\n{actual}── expected ──\n{expected}"
    );
}

/// A plan built by [`Plan::build`] from a real config: registry tokens,
/// native symbols and URL redaction as the dashboard really derives them.
fn registry_plan() -> Plan {
    let dir = short_tempdir();
    let paths = StorePaths::in_dir(dir.path().join("config"));
    std::fs::create_dir_all(paths.dir()).unwrap();
    std::fs::write(
        paths.config(),
        r#"version = 2
default = "base"

[chains.eth]
id = 1
aliases = ["mainnet"]
rpc = "infura"
failover = ["llama"]

[chains.eth.rpcs]
infura = "https://mainnet.infura.io/v3/${INFURA_KEY}"
llama = "https://eth.llamarpc.com"
alchemy = "https://eth-mainnet.g.alchemy.com/v2/0123456789abcdef0123456789abcdef"
stream = "wss://eth.example.org/ws/hunter2"

[chains.base]
id = 8453
rpc = "public"

[chains.base.rpcs]
public = "https://mainnet.base.org"

[chains.gnosis]
id = 100
rpc = "public"

[chains.gnosis.rpcs]
public = "https://rpc.gnosischain.com"

[chains.polygon]
id = 137
rpc = "public"

[chains.polygon.rpcs]
public = "https://polygon-rpc.com"

[chains.celo]
id = 42220
rpc = "forno"

[chains.celo.rpcs]
forno = "https://forno.celo.org"

[chains.devnet]
id = 31337
rpc = "local"

[chains.devnet.rpcs]
local = "http://127.0.0.1:8545"

[signers.deployer]
backend = "keyring"
address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

[signers.cold]
backend = "keyring"
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(paths.config(), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(paths.secrets(), "[vars]\nINFURA_KEY = \"s3cr3t\"\n").unwrap();
    std::fs::set_permissions(paths.secrets(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let chainz = Chainz::open_at(paths, LoadMode::Strict).unwrap();
    Plan::build(&chainz, false)
}

/// The registry plan with heads and balances on most chains.
fn registry_state() -> State {
    let plan = registry_plan();
    let mut state = State::new(&plan, &HealthStore::empty());
    let ether = 1_000_000_000_000_000_000u128;
    let source = |chain: &str| plan.chain(chain).unwrap().source;
    let head = |chain: &str, number: u64, ms: u32| Update::Head {
        chain: chain.into(),
        source: source(chain),
        at_ms: NOW,
        via_gateway: false,
        result: Ok(Head {
            number,
            timestamp: Some(NOW / 1_000 - 3),
            base_fee: Some(1_500_000_000),
            latency_ms: ms,
        }),
        block_time: Some(5.0),
    };
    let sheet = |chain: &str, native: u128, tokens: usize| Update::Balances {
        chain: chain.into(),
        source: source(chain),
        at_ms: NOW - 5_000,
        result: Ok(Sheet {
            via: Via::Multicall3,
            holders: vec![Holding {
                signer: "deployer".into(),
                native: Some(native),
                tokens: (0..tokens)
                    .map(|i| Some((i as u128 + 1) * 1_000_000))
                    .collect(),
            }],
        }),
    };
    for (chain, number) in [
        ("eth", 23_000_000),
        ("base", 38_000_000),
        ("gnosis", 41_000_000),
    ] {
        state.apply(head(chain, number, 70));
    }
    state.apply(Update::Head {
        chain: "devnet".into(),
        source: source("devnet"),
        at_ms: NOW,
        via_gateway: false,
        result: Err(Fault::new(
            ErrorClass::Connect,
            "could not connect to endpoint",
        )),
        block_time: None,
    });
    let tokens = |chain: &str| plan.chain(chain).unwrap().info.tokens.len();
    for (chain, native) in [
        ("eth", ether / 4),
        ("base", ether * 3),
        ("gnosis", ether * 12),
        ("polygon", crate::view::SATURATED),
        ("celo", 0),
    ] {
        state.apply(sheet(chain, native, tokens(chain)));
    }
    state.apply(Update::Balances {
        chain: "devnet".into(),
        source: source("devnet"),
        at_ms: NOW,
        result: Err(Fault::new(
            ErrorClass::Connect,
            "could not connect to endpoint",
        )),
    });
    state
}

/// Every state the golden files cover, by name.
fn states() -> Vec<(&'static str, State)> {
    vec![
        (
            "empty",
            State::new(&chains_only(vec![]), &HealthStore::empty()),
        ),
        (
            "empty-with-signers",
            State::new(&plan(vec![]), &HealthStore::empty()),
        ),
        ("fixture", fixture()),
        ("gateway", with_gateway(fixture())),
        ("registry", registry_state()),
    ]
}

#[test]
fn once_text_matches_the_golden_files() {
    for (name, state) in states() {
        golden(&format!("once/{name}.txt"), &crate::once::text(&state, NOW));
    }
}

#[test]
fn once_json_matches_the_golden_files() {
    for (name, state) in states() {
        let snapshot = crate::snapshot::Snapshot::new(&state, NOW);
        let json = serde_json::to_string_pretty(&snapshot).unwrap() + "\n";
        golden(&format!("once/{name}.json"), &json);
    }
}

/// What `--once` printed before it had its own writer: the live frame,
/// drawn by ratatui, with the tab bar, the key hints and the selection
/// marker taken off. The text writer must print exactly this.
fn screen_text(state: &State, now_ms: u64) -> String {
    use crate::app::{App, Tab};
    use crate::ui::theme::Theme;
    const WIDTH: u16 = 120;
    let rows = u16::try_from(state.chains.len()).unwrap_or(u16::MAX - 8);
    let height = rows.max(6) + 4;
    let mut app = App::default();
    let mut out = String::new();
    let mut frame = |app: &App| {
        let ctx = crate::ui::Ctx {
            app,
            state,
            theme: Theme::new(false),
            now_ms,
        };
        let text = crate::ui::render_text(ctx, WIDTH, height);
        let lines: Vec<&str> = text.lines().collect();
        let mut body = lines[1..lines.len() - 1].to_vec();
        let heading = state.signers.get(app.signer).and_then(|s| {
            let address = s.address?.to_checksum();
            (app.tab == Tab::Balances).then(|| format!(" signer {} {address}", s.name))
        });
        if let Some(heading) = &heading {
            body[0] = heading;
        }
        out.push_str(body.join("\n").replace('▸', " ").trim_end());
        out.push_str("\n\n");
    };
    frame(&app);
    app.tab = Tab::Balances;
    for (i, signer) in state.signers.iter().enumerate() {
        if signer.address.is_some() {
            app.signer = i;
            frame(&app);
        }
    }
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out.push('\n');
    out
}

#[test]
fn once_text_is_the_screen_without_its_chrome() {
    for (name, state) in states() {
        assert_eq!(
            crate::once::text(&state, NOW),
            screen_text(&state, NOW),
            "{name}"
        );
    }
}
