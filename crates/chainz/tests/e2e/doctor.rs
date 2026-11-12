//! `doctor`: config and credential checks, endpoint health, `--fix`,
//! `--discover`, and the maintainer check `--tokens`.

use crate::common::*;
use chainz_core::labels::{BuiltinToken, builtin_token, builtin_tokens};
use chainz_testkit::abi;
use std::collections::HashMap;

/// A user label that shadows a built-in token in `--tokens` checks.
const ROUTER: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";

#[test]
fn doctor_reports_dead_rpc_and_plaintext_signer() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd().arg("doctor").assert().code(1).stdout(
        predicate::str::contains("plaintext")
            .and(predicate::str::contains("eth: active endpoint 'local'"))
            .and(predicate::str::contains("`chainz doctor eth --discover`")),
    );
}

#[test]
fn doctor_groups_one_cause_across_chains() {
    let sb = Sandbox::new();
    let chains = [("eth", 1), ("base", 8453), ("op", 10), ("arb", 42161)];
    let mut toml = String::from("version = 2\n");
    for (name, id) in chains {
        toml.push_str(&format!(
            "\n[chains.{name}]\nid = {id}\nrpc = \"local\"\n[chains.{name}.rpcs]\nlocal = \"http://127.0.0.1:1\"\nqn = \"http://127.0.0.1:1/abcDEF0123456789abcdef0123456789xyz/\"\n"
        ));
    }
    sb.write_config(&toml);
    set_mode(&sb.dir(), 0o700);
    let out = sb.run(&["doctor"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(
        text.contains("'qn' on 4 chains: literal credential in the URL (path segment)"),
        "{text}"
    );
    assert!(
        text.contains("active endpoint 'local' is failing (connection failed) on 4 chains"),
        "{text}"
    );
    assert!(!text.contains("eth: active endpoint"), "{text}");
    // No endpoint can replace it: the next step is --discover.
    assert!(
        text.contains(
            "check your network, or find public endpoints with `chainz doctor CHAIN --discover`"
        ),
        "{text}"
    );
    // Every instance counts; nothing here is for --fix.
    assert!(
        text.contains("8 problems; `--fix` can't repair these"),
        "{text}"
    );
    // -v lists each finding under its group.
    let verbose = stdout(&sb.run(&["doctor", "-v"]));
    assert!(
        verbose.contains("chains.arb.rpcs.qn: literal credential"),
        "{verbose}"
    );
    assert!(
        verbose.contains("op: active endpoint 'local' is failing"),
        "{verbose}"
    );
}

#[test]
fn doctor_says_what_fix_would_do() {
    let sb = Sandbox::new();
    sb.write_config("version = 2\n[chains.eth]\nid = 1\nrpc = \"local\"\n[chains.eth.rpcs]\nlocal = \"http://127.0.0.1:1\"\ninfura = \"https://mainnet.infura.invalid/v3/0123456789abcdef0123456789abcdef\"\n");
    sb.write_secrets("");
    set_mode(&sb.dir(), 0o700);
    set_mode(&sb.secrets_path(), 0o600);
    set_mode(&sb.config_path(), 0o600);
    let text = stdout(&sb.run(&["doctor"]));
    assert!(
        text.contains("`chainz doctor --fix` would move 1 credential"),
        "{text}"
    );
}

#[test]
fn doctor_fix_moves_literal_credentials_into_variables() {
    let sb = Sandbox::new();
    let rpc = Mock::chain_id(1);
    let url = rpc.url();
    sb.write_config(&format!(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"local\"\n[chains.eth.rpcs]\nlocal = \"{url}\"\ninfura = \"https://mainnet.infura.invalid/v3/0123456789abcdef0123456789abcdef\"\n"
    ));
    sb.cmd()
        .args(["doctor", "--fix"])
        .assert()
        .stdout(predicate::str::contains("${INFURA_KEY}"));
    assert!(sb.config().contains("/v3/${INFURA_KEY}"));
    assert!(
        sb.secrets()
            .contains("INFURA_KEY = \"0123456789abcdef0123456789abcdef\"")
    );
}

#[test]
fn doctor_reports_semantically_invalid_config() {
    let sb = Sandbox::new();
    sb.write_config("version = 2\n[chains.eth]\nid = 1\nrpc = \"missing\"\n[chains.eth.rpcs]\na = \"http://x.invalid\"\n");
    sb.cmd().arg("ls").assert().failure();
    sb.cmd()
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("missing"));
}

#[test]
fn doctor_says_where_secrets_do_not_parse_without_quoting_them() {
    let sb = Sandbox::new();
    sb.write_config("version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = \"http://127.0.0.1:1\"\n");
    sb.write_secrets("[vars]\nK = \"s3cret-value\" x\n");
    sb.cmd().arg("doctor").assert().code(1).stdout(
        predicate::str::contains("secrets.toml: ")
            .and(predicate::str::contains("(at byte"))
            .and(predicate::str::contains("s3cret").not()),
    );
}

#[test]
fn doctor_lists_the_locations() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let out = sb.run(&["doctor"]);
    let text = stdout(&out);
    for (what, path) in [
        ("secrets", sb.secrets_path()),
        ("state", sb.path("state/chainz")),
        ("cache", sb.path("cache/chainz")),
        ("runtime", sb.runtime_file("")),
    ] {
        let path = path.display().to_string();
        assert!(
            text.contains(&format!("{what:<8} {}", path.trim_end_matches('/'))),
            "{what}: {text}"
        );
    }
    assert!(
        text.contains("serve.log") && text.contains("forks/NAME.log"),
        "{text}"
    );
}

#[test]
fn doctor_chain_fix_activates_the_best_and_yes_writes_failover() {
    let sb = Sandbox::new();
    let fast = chain_rpc(1, 1_000, Duration::ZERO);
    let slow = chain_rpc(1, 1_000, Duration::from_millis(100));
    let config = format!(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"dead\"\n[chains.eth.rpcs]\ndead = \"http://127.0.0.1:1\"\nfast = \"{}\"\nslow = \"{}\"\n",
        fast.url(),
        slow.url()
    );
    sb.write_config(&config);

    // Report only: the dead active endpoint is a problem.
    let out = sb.run(&["doctor", "eth"]);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}");
    assert!(report.contains("(2 of 3 answered)"), "{report}");
    assert!(
        report.contains("eth: active endpoint 'dead' is failing"),
        "{report}"
    );
    assert!(
        report.contains("best: fast (--fix activates it)"),
        "{report}"
    );
    assert!(
        report.contains("`chainz doctor eth --fix` would")
            && report.contains("switch 1 active endpoint"),
        "{report}"
    );
    assert!(sb.config().contains("rpc = \"dead\""));

    // --fix off a TTY activates the best but leaves failover unwritten.
    let fixed = sb.ok(&["doctor", "eth", "--fix"]);
    assert!(
        fixed.contains("eth: active → fast; proposed failover: slow"),
        "{fixed}"
    );
    assert!(fixed.contains("failover list not written"), "{fixed}");
    assert!(fixed.contains("fixed: eth: active dead → fast"), "{fixed}");
    assert!(sb.config().contains("rpc = \"fast\""));
    assert!(!sb.config().contains("failover"));

    // --yes writes it.
    sb.write_config(&config);
    sb.ok(&["doctor", "eth", "--fix", "--yes"]);
    let written = sb.config();
    assert!(written.contains("rpc = \"fast\""), "{written}");
    assert!(written.contains("failover = [\"slow\"]"), "{written}");

    // --discover needs a chain.
    sb.cmd().args(["doctor", "--discover"]).assert().code(2);
}

/// Every endpoint down: no latencies, nothing ranked, `--fix` says so, and
/// the exit code tells scripts.
#[test]
fn doctor_chain_with_every_endpoint_down_fails() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.uni]\nid = 130\nrpc = \"a\"\n[chains.uni.rpcs]\na = \"http://127.0.0.1:1\"\nb = \"http://127.0.0.1:1/b\"\n",
    );
    for fix in [false, true] {
        let args: &[&str] = if fix {
            &["doctor", "uni", "--fix"]
        } else {
            &["doctor", "uni"]
        };
        let out = sb.run(args);
        let text = stdout(&out);
        assert_eq!(out.status.code(), Some(1), "{text}");
        assert!(text.contains("(0 of 2 answered)"), "{text}");
        assert!(text.contains("✗ connection failed"), "{text}");
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("✗ connection failed"))
            .collect();
        assert!(
            lines[0].contains(" a ") && lines[1].contains(" b "),
            "{text}"
        );
        assert!(
            text.contains(
                "no healthy endpoint on uni; check your network, or find public endpoints with `chainz doctor uni --discover`"
            ),
            "{text}"
        );
    }
    assert!(sb.config().contains("rpc = \"a\""));
}

/// An endpoint whose `${VAR}` is set nowhere can't be probed: a problem,
/// not "no HTTP endpoints to probe" and exit 0.
#[test]
fn doctor_reports_endpoints_it_cannot_probe() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = \"https://x.invalid/v3/${NOPE_KEY}\"\n",
    );
    for args in [&["doctor", "eth"][..], &["doctor"]] {
        let out = sb.run(args);
        let text = stdout(&out);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {text}");
        assert!(
            text.contains("✗ not probed: chain 'eth' endpoint 'a'")
                && text.contains("${NOPE_KEY} is not set"),
            "{args:?}: {text}"
        );
        assert!(!text.contains("websocket"), "{args:?}: {text}");
    }
    assert!(stdout(&sb.run(&["doctor", "eth"])).contains("(0 of 1 answered)"));
}

#[test]
fn doctor_discover_lists_off_a_tty_and_yes_adds_the_live_ones() {
    let sb = Sandbox::new();
    let live = chain_rpc(1, 1_000, Duration::ZERO);
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"known\"\n[chains.eth.rpcs]\nknown = \"http://127.0.0.1:9/\"\n",
    );
    seed_chainlist(
        &sb,
        "Ethereum",
        1,
        &[
            "http://bad host",
            "http://127.0.0.1:1",
            &live.url(),
            "http://127.0.0.1:9",
        ],
    );

    // Off a TTY without --yes: the ranked list and a hint, nothing added.
    let before = sb.config();
    let listed = sb.run(&["doctor", "eth", "--discover"]);
    let text = stdout(&listed);
    assert!(text.contains(&live.url()), "{text}");
    assert!(
        text.contains("chainz doctor eth --discover --yes"),
        "{text}"
    );
    // A malformed chainlist URL is left out, as `add` leaves it out.
    assert!(!text.contains("bad host"), "{text}");
    assert!(!stderr(&listed).contains("REDACTED"), "{}", stderr(&listed));
    assert_eq!(sb.config(), before);

    // --yes adds every endpoint that answered, and only those; the checks
    // then rank it with the others.
    let added = stdout(&sb.run(&["doctor", "eth", "--discover", "--yes"]));
    assert!(
        added.contains("Added 1 endpoint to 'eth': local\n"),
        "{added}"
    );
    assert!(
        added.contains("best: local (--fix activates it)"),
        "{added}"
    );
    let (_, list) = endpoints(&sb, "eth");
    assert_eq!(
        list,
        [
            ("known".to_string(), "http://127.0.0.1:9/".to_string()),
            ("local".to_string(), live.url())
        ]
    );
}

/// What the mock chain says about one address.
#[derive(Clone)]
struct MockToken {
    code: bool,
    symbol: String,
    decimals: u8,
}

/// A JSON-RPC server answering `eth_chainId`, `eth_getCode` and the
/// `symbol()` / `decimals()` calls for the given tokens (keyed by lowercase
/// address). Unknown addresses have no code and revert.
fn token_rpc(chain_id: u64, tokens: HashMap<String, MockToken>) -> Mock {
    mock(move |call| {
        let params = &call.params;
        let token = |address: &Value| tokens.get(&address.as_str()?.to_ascii_lowercase());
        match call.method.as_str() {
            "eth_chainId" => Ok(json!(format!("{chain_id:#x}"))),
            "eth_getCode" => Ok(json!(if token(&params[0]).is_some_and(|t| t.code) {
                "0x6080"
            } else {
                "0x"
            })),
            "eth_call" => {
                let data = params[0]["data"].as_str().unwrap_or_default();
                match token(&params[0]["to"]) {
                    Some(t) if data == abi::SYMBOL => Ok(json!(abi::string(&t.symbol))),
                    Some(t) if data == abi::DECIMALS => Ok(json!(abi::uint(t.decimals.into()))),
                    _ => Err("execution reverted".into()),
                }
            }
            _ => Err("method not found".into()),
        }
    })
}

/// Chains `(name, id, rpc url)` with a plaintext `default` signer.
fn seeded(chains: &[(&str, u64, &str)]) -> Sandbox {
    let sb = Sandbox::new();
    sb.seed_with(chains);
    sb
}

fn registry_on(chain_id: u64) -> Vec<&'static BuiltinToken> {
    builtin_tokens()
        .iter()
        .filter(|t| t.chain_id == chain_id)
        .collect()
}

fn mock_tokens(chain_id: u64) -> HashMap<String, MockToken> {
    registry_on(chain_id)
        .into_iter()
        .map(|t| {
            (
                t.address.to_lower_hex(),
                MockToken {
                    code: true,
                    symbol: t.symbol.clone(),
                    decimals: t.decimals,
                },
            )
        })
        .collect()
}

#[test]
fn doctor_tokens_passes_when_the_chain_agrees_with_the_registry() {
    // Arbitrum's USDT answers `USD₮0`: symbols are compared as UTF-8.
    let base = token_rpc(8453, mock_tokens(8453));
    let arb = token_rpc(42161, mock_tokens(42161));
    let sb = seeded(&[("base", 8453, &base.url()), ("arb", 42161, &arb.url())]);
    let out = sb.ok(&["doctor", "--tokens"]);
    for t in registry_on(8453) {
        let line = format!(
            "✓ base @{} {} {} ({} decimals)",
            t.name, t.address, t.symbol, t.decimals
        );
        assert!(out.contains(&line), "missing {line:?} in:\n{out}");
    }
    assert!(
        out.contains("✓ arb @usdt 0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9 USD₮0 (6 decimals)"),
        "{out}"
    );
    // Chains that are not configured are reported, not failed.
    assert!(out.contains("⚠ eth (1): not configured, "), "{out}");
    let checked = registry_on(8453).len() + registry_on(42161).len();
    let total = builtin_tokens().len();
    assert!(
        out.ends_with(&format!(
            "✓ {total} registry entries: {checked} ok, 0 failed, {} skipped\n",
            total - checked
        )),
        "{out}"
    );
}

#[test]
fn doctor_tokens_fails_on_any_mismatch() {
    let mut tokens = mock_tokens(8453);
    let lower = |name: &str| builtin_token(8453, name).unwrap().address.to_lower_hex();
    tokens.get_mut(&lower("usdc")).unwrap().symbol = "USDbC".into();
    tokens.get_mut(&lower("usdt")).unwrap().decimals = 18;
    tokens.get_mut(&lower("dai")).unwrap().code = false;
    tokens.remove(&lower("usdce"));
    let rpc = token_rpc(8453, tokens);
    // `eth` answers with the wrong chain ID; `op` fails every request
    // (with an error `--tokens` doesn't retry: a closed port would cost
    // seconds of backoff).
    let wrong = token_rpc(5, HashMap::new());
    let broken = Mock::start(|_| Answer::Http(500, None));
    let sb = seeded(&[
        ("base", 8453, &rpc.url()),
        ("eth", 1, &wrong.url()),
        ("op", 10, &broken.url()),
    ]);
    // A shadowing user label is a note.
    sb.ok(&["label", "add", "weth", ROUTER, "--chain", "base"]);
    let out = sb.cmd().args(["doctor", "--tokens"]).assert().code(1);
    let out = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let line = |name: &str| {
        let t = builtin_token(8453, name).unwrap();
        format!("base @{name} {}", t.address)
    };
    assert!(
        out.contains(&format!("✓ {} WETH (18 decimals)", line("weth"))),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "✗ {}: symbol() is \"USDbC\", the registry says \"USDC\"",
            line("usdc")
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "✗ {}: decimals() is 18, the registry says 6",
            line("usdt")
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!("✗ {}: no code at this address", line("dai"))),
        "{out}"
    );
    assert!(
        out.contains(&format!("✗ {}: no code at this address", line("usdce"))),
        "{out}"
    );
    assert!(
        out.contains("✗ eth (1): chain ID mismatch: expected 1, endpoint reports 5; "),
        "{out}"
    );
    assert!(out.contains("✗ op (10): HTTP status 500; "), "{out}");
    let failed = 4 + registry_on(1).len() + registry_on(10).len();
    assert!(out.contains(&format!(": 1 ok, {failed} failed, ")), "{out}");
    assert!(!out.contains("127.0.0.1"), "{out}");
    // Shadowing is a config matter: plain `doctor` notes it, `--tokens`
    // (which checks the registry against the chain) does not.
    let shadow = format!("⚠ base: your @weth ({ROUTER}) shadows the built-in token");
    assert!(!out.contains(&shadow), "{out}");
    let plain = sb.run(&["doctor"]);
    assert!(stdout(&plain).contains(&shadow), "{}", stdout(&plain));
}

/// `--tokens` is a maintainer check: it runs, but `doctor --help` doesn't
/// list it, and it doesn't combine with `--fix`.
#[test]
fn doctor_tokens_is_unlisted_and_conflicts_with_fix() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    assert!(!sb.ok(&["doctor", "--help"]).contains("--tokens"));
    sb.cmd()
        .args(["doctor", "--tokens", "--fix"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
}
