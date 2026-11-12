//! End-to-end tests for the address book: `addr …`, `@label` expansion in
//! `exec`, `label`, and `doctor --tokens`.
//!
//! As in `cli.rs`, every invocation runs inside a sandbox (see `common`).

mod common;

use chainz_core::labels::{BuiltinToken, builtin_token, builtin_tokens};
use chainz_rpc::abi;
use common::{MockRpc, Sandbox, TEST_ADDRESS, stdout};
use predicates::prelude::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;

const ROUTER: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
const ROUTER_ETH: &str = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF";
const SAFE: &str = "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69";
const UNKNOWN: &str = "0x1efF47bc3a10a45D4B230B5d10E37751FE6AA718";
const USDC_BASE: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const USDT_ETH: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";

/// `eth` (1) and `base` (8453) on a dead local RPC.
fn eth_and_base() -> Sandbox {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    sb
}

/// Chains `(name, id, rpc url)` with a plaintext `default` signer.
fn seeded(chains: &[(&str, u64, &str)]) -> Sandbox {
    let sb = Sandbox::new();
    sb.seed_with(chains);
    sb
}

/// `chainz exec <chain> -- echo <args>`, returning what echo printed.
fn echo(sb: &Sandbox, chain: &str, args: &[&str]) -> String {
    let mut all = vec!["exec", chain, "--", "echo"];
    all.extend_from_slice(args);
    sb.ok(&all).trim_end().to_string()
}

// ---------------------------------------------------------------- addr

#[test]
fn addr_add_ls_find_rm_roundtrip() {
    let sb = eth_and_base();
    sb.cmd()
        .args(["addr", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("@default").and(predicate::str::contains("signer")));

    // Per chain, with any input case; stored checksummed.
    sb.cmd()
        .args([
            "addr",
            "add",
            "MyRouter",
            &ROUTER.to_ascii_lowercase(),
            "--chain",
            "base",
        ])
        .assert()
        .success()
        .stdout(format!("Added @myrouter on base: {ROUTER}\n"));
    sb.cmd()
        .args(["addr", "add", "myrouter", ROUTER_ETH, "--chain", "1"])
        .assert()
        .success()
        .stdout(format!("Added @myrouter on eth: {ROUTER_ETH}\n"));
    // Every chain, only when asked.
    sb.cmd()
        .args(["addr", "add", "@my-safe", SAFE, "--all-chains"])
        .assert()
        .success()
        .stdout(format!("Added @my-safe on every chain: {SAFE}\n"));
    let config = sb.config();
    assert!(
        config.contains(&format!(
            "[addresses.myrouter]\nbase = \"{ROUTER}\"\neth = \"{ROUTER_ETH}\"\n"
        )),
        "{config}"
    );
    assert!(
        config.contains(&format!("[addresses.my-safe]\n\"*\" = \"{SAFE}\"\n")),
        "{config}"
    );

    // ls: everything, then one chain's view.
    let all = sb.ok(&["addr", "ls"]);
    let lines: Vec<Vec<&str>> = all
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(
        lines,
        vec![
            vec!["@myrouter", "base", ROUTER, "user"],
            vec!["@myrouter", "eth", ROUTER_ETH, "user"],
            vec!["@my-safe", "*", SAFE, "user"],
            vec!["@default", "*", TEST_ADDRESS, "signer"],
        ],
        "{all}"
    );
    let base = sb.ok(&["addr", "ls", "--chain", "base"]);
    assert!(
        base.contains(ROUTER) && !base.contains(ROUTER_ETH),
        "{base}"
    );
    assert!(base.contains(SAFE) && base.contains("@default"), "{base}");
    assert!(!base.contains("@usdc"), "{base}");
    // --builtin adds the registry.
    let base = sb.ok(&["addr", "ls", "--chain", "base", "--builtin"]);
    assert!(
        base.lines()
            .any(|l| l.starts_with("@usdc ") && l.contains(USDC_BASE) && l.contains("built-in")),
        "{base}"
    );
    assert!(!base.contains(USDT_ETH), "{base}");
    let all = sb.ok(&["addr", "ls", "--builtin"]);
    assert!(all.contains(USDC_BASE) && all.contains(USDT_ETH), "{all}");
    assert!(all.lines().count() >= 4 + builtin_tokens().len(), "{all}");

    // find: every label for the address, with its source.
    sb.cmd()
        .args(["addr", "find", &ROUTER.to_ascii_lowercase()])
        .assert()
        .success()
        .stdout("@myrouter on base (user)\n");
    sb.cmd()
        .args(["addr", "find", SAFE])
        .assert()
        .success()
        .stdout("@my-safe on every chain (user)\n");
    sb.cmd()
        .args(["addr", "find", TEST_ADDRESS])
        .assert()
        .success()
        .stdout("@default on every chain (signer)\n");
    sb.cmd()
        .args(["addr", "find", USDC_BASE])
        .assert()
        .success()
        .stdout("@usdc on base (built-in)\n");
    sb.cmd()
        .args(["addr", "find", UNKNOWN])
        .assert()
        .code(1)
        .stdout("")
        .stderr(format!("no label for {UNKNOWN}\n"));

    // rm: one chain's entry, then the whole label.
    sb.cmd()
        .args(["addr", "rm", "myrouter", "--chain", "base"])
        .assert()
        .success()
        .stdout(format!("Removed @myrouter on base ({ROUTER})\n"));
    let config = sb.config();
    assert!(
        config.contains(ROUTER_ETH) && !config.contains(ROUTER),
        "{config}"
    );
    sb.cmd()
        .args(["addr", "rm", "myrouter"])
        .assert()
        .success()
        .stdout(format!("Removed @myrouter on eth ({ROUTER_ETH})\n"));
    sb.cmd()
        .args(["addr", "rm", "my-safe", "--chain", "*"])
        .assert()
        .success()
        .stdout(format!("Removed @my-safe on every chain ({SAFE})\n"));
    assert!(!sb.config().contains("addresses"), "{}", sb.config());
    sb.cmd().args(["addr", "find", SAFE]).assert().code(1);
}

#[test]
fn addr_add_defaults_to_the_current_chain_context() {
    let sb = eth_and_base();
    // No --chain, no $CHAINZ_CHAIN, no default, no TTY: a usage error,
    // not a guess.
    sb.cmd()
        .args(["addr", "add", "myrouter", ROUTER])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no chain given: pass --chain"))
        .stderr(predicate::str::contains("Usage: chainz addr add"));
    assert!(!sb.config().contains("addresses"));

    // $CHAINZ_CHAIN (set inside `chainz shell`).
    sb.cmd()
        .env("CHAINZ_CHAIN", "base")
        .args(["addr", "add", "myrouter", ROUTER])
        .assert()
        .success()
        .stdout(format!("Added @myrouter on base: {ROUTER}\n"))
        .stderr("");
    // The default chain, with its stderr notice.
    sb.cmd().args(["use", "eth"]).assert().success();
    sb.cmd()
        .args(["addr", "add", "myrouter", ROUTER_ETH])
        .assert()
        .success()
        .stdout(format!("Added @myrouter on eth: {ROUTER_ETH}\n"))
        .stderr("chainz: eth (default)\n");
    // An explicit chain wins over both.
    sb.cmd()
        .env("CHAINZ_CHAIN", "eth")
        .args(["addr", "add", "other", SAFE, "--chain", "base"])
        .assert()
        .success()
        .stdout(format!("Added @other on base: {SAFE}\n"));
    // --chain and --all-chains exclude each other.
    sb.cmd()
        .args(["addr", "add", "x", SAFE, "--chain", "base", "--all-chains"])
        .assert()
        .failure();
}

#[test]
fn addr_add_rejects_bad_names_addresses_and_overwrites() {
    let sb = eth_and_base();
    sb.ok(&["addr", "add", "myrouter", ROUTER, "--chain", "base"]);
    let before = sb.config();
    let fails = |args: &[&str], want: &str| {
        sb.cmd()
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains(want));
        assert_eq!(sb.config(), before, "{args:?} changed the config");
    };
    // Context tokens and signer names are reserved.
    for name in [
        "rpc",
        "chainid",
        "chain",
        "wallet",
        "explorer",
        "explorer_key",
        "Wallet",
        "key",
    ] {
        fails(
            &["addr", "add", name, ROUTER, "--chain", "base"],
            "context token",
        );
    }
    fails(
        &["addr", "add", "default", ROUTER, "--all-chains"],
        "is a signer",
    );
    fails(
        &["addr", "add", "my.router", ROUTER, "--chain", "base"],
        "[a-z0-9_-]+",
    );
    // Addresses: malformed, or mixed case with a wrong checksum.
    fails(
        &["addr", "add", "x", "0x1234", "--chain", "base"],
        "is not an address",
    );
    fails(
        &["addr", "add", "x", "vitalik.eth", "--chain", "base"],
        "is not an address",
    );
    let typo = ROUTER.replace("7E5F", "7e5F");
    fails(
        &["addr", "add", "x", &typo, "--chain", "base"],
        "is not an address",
    );
    fails(
        &["addr", "add", "x", ROUTER, "--chain", "ghost"],
        "not found",
    );
    // An existing entry is never overwritten silently.
    sb.cmd()
        .args(["addr", "add", "myrouter", ROUTER_ETH, "--chain", "base"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains(format!("@myrouter is already {ROUTER} on base")).and(
                predicate::str::contains("chainz addr rm myrouter --chain 'base'"),
            ),
        );
    assert_eq!(sb.config(), before);
    // The same address again is fine.
    sb.cmd()
        .args([
            "addr",
            "add",
            "myrouter",
            &ROUTER.to_ascii_lowercase(),
            "--chain",
            "base",
        ])
        .assert()
        .success()
        .stdout(format!("@myrouter is already {ROUTER} on base\n"));
    assert_eq!(sb.config(), before);

    // rm: only your own labels, and only an exact chain.
    fails(&["addr", "rm", "usdc"], "built-in token");
    fails(&["addr", "rm", "default"], "is a signer");
    fails(&["addr", "rm", "myroutr"], "did you mean myrouter?");
    fails(
        &["addr", "rm", "myrouter", "--chain", "bas"],
        "exact chain name",
    );
    fails(
        &["addr", "rm", "myrouter", "--chain", "eth"],
        "no address on eth",
    );
}

#[test]
fn shadowing_a_builtin_token_is_allowed_and_noted() {
    let sb = eth_and_base();
    sb.cmd()
        .args(["addr", "add", "usdc", ROUTER, "--chain", "base"])
        .assert()
        .success()
        .stderr("⚠ @usdc now shadows the built-in token on base\n");
    assert_eq!(echo(&sb, "base", &["@usdc"]), ROUTER);
    // Only on that chain.
    let usdc_eth = builtin_token(1, "usdc").unwrap().address.to_string();
    assert_eq!(echo(&sb, "eth", &["@usdc"]), usdc_eth);
    let found = sb.ok(&["addr", "find", USDC_BASE]);
    assert_eq!(found, "@usdc on base (built-in, shadowed by your @usdc)\n");
    let listed = sb.ok(&["addr", "ls", "--builtin"]);
    assert!(
        listed
            .lines()
            .any(|l| l.contains(USDC_BASE) && l.ends_with("built-in (shadowed)")),
        "{listed}"
    );
}

// ---------------------------------------------------------------- exec

#[test]
fn exec_expands_user_labels_per_chain_and_for_every_chain() {
    let sb = eth_and_base();
    sb.ok(&["addr", "add", "myrouter", ROUTER, "--chain", "base"]);
    sb.ok(&["addr", "add", "my-safe", SAFE, "--all-chains"]);

    // Per chain: the label resolves where it was added.
    assert_eq!(echo(&sb, "base", &["@myrouter"]), ROUTER);
    assert_eq!(
        echo(&sb, "base", &["to=@MyRouter,x"]),
        format!("to={ROUTER},x")
    );
    // "*": on every chain.
    assert_eq!(echo(&sb, "base", &["@my-safe"]), SAFE);
    assert_eq!(echo(&sb, "eth", &["@my-safe"]), SAFE);
    // A qualifier reaches another chain's entry.
    assert_eq!(echo(&sb, "eth", &["@myrouter:base"]), ROUTER);
    // Built-in tokens.
    assert_eq!(echo(&sb, "base", &["@usdc"]), USDC_BASE);
    assert_eq!(echo(&sb, "eth", &["@usdt"]), USDT_ETH);
    assert_eq!(echo(&sb, "base", &["@usdt:eth"]), USDT_ETH);

    // A per-chain entry beats "*" on its own chain only.
    sb.ok(&["addr", "add", "my-safe", ROUTER_ETH, "--chain", "eth"]);
    assert_eq!(echo(&sb, "eth", &["@my-safe"]), ROUTER_ETH);
    assert_eq!(echo(&sb, "base", &["@my-safe"]), SAFE);

    // Removing the label makes the token unknown again.
    sb.ok(&["addr", "rm", "myrouter"]);
    sb.cmd()
        .args(["exec", "base", "--", "echo", "@myrouter"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown token @myrouter"));
}

#[test]
fn exec_errors_when_a_label_has_no_address_on_the_chain() {
    let sb = eth_and_base();
    sb.ok(&["addr", "add", "myrouter", ROUTER, "--chain", "base"]);
    sb.cmd()
        .args(["exec", "eth", "--", "echo", "@myrouter"])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains(
            "@myrouter has no address on chain 'eth' (1)",
        ));
    // The same for a qualifier, and for a built-in token absent from a chain.
    sb.cmd()
        .args(["exec", "base", "--", "echo", "@myrouter:eth"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "@myrouter has no address on chain 'eth' (1)",
        ));
    assert!(builtin_token(8453, "wbtc").is_none());
    sb.cmd()
        .args(["exec", "base", "--", "echo", "@wbtc"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "@wbtc has no address on chain 'base' (8453)",
        ));
}

// ---------------------------------------------------------------- label

#[test]
fn label_rewrites_addresses_for_the_chain() {
    let sb = eth_and_base();
    sb.ok(&["addr", "add", "myrouter", ROUTER, "--chain", "base"]);
    sb.ok(&["addr", "add", "pool", ROUTER, "--chain", "eth"]);
    sb.ok(&["addr", "add", "my-safe", SAFE, "--all-chains"]);
    let input = format!(
        "to {ROUTER} from {} token {USDC_BASE} safe {SAFE}\nword 0x000000000000000000000000{} other {UNKNOWN}\n",
        TEST_ADDRESS.to_ascii_lowercase(),
        &ROUTER[2..]
    );
    let rest = format!(
        "\nword 0x000000000000000000000000{} other {UNKNOWN}\n",
        &ROUTER[2..]
    );
    let run = |args: &[&str], env: Option<&str>| {
        let mut cmd = sb.cmd();
        if let Some(chain) = env {
            cmd.env("CHAINZ_CHAIN", chain);
        }
        let out = cmd.args(args).write_stdin(input.clone()).assert().success();
        let out = out.get_output();
        (
            String::from_utf8(out.stdout.clone()).unwrap(),
            String::from_utf8(out.stderr.clone()).unwrap(),
        )
    };

    let on_base = format!(
        "to myrouter({ROUTER}) from default({TEST_ADDRESS}) token usdc({USDC_BASE}) safe my-safe({SAFE}){rest}"
    );
    let on_eth = format!(
        "to pool({ROUTER}) from default({TEST_ADDRESS}) token {USDC_BASE} safe my-safe({SAFE}){rest}"
    );
    // No chain: "*" labels, signers, and addresses with one name everywhere.
    let anywhere = format!(
        "to {ROUTER} from default({TEST_ADDRESS}) token usdc({USDC_BASE}) safe my-safe({SAFE}){rest}"
    );
    assert_eq!(
        run(&["label", "--chain", "base"], None),
        (on_base.clone(), String::new())
    );
    assert_eq!(run(&["label", "--chain", "1"], None).0, on_eth);
    assert_eq!(
        run(&["label"], Some("base")),
        (on_base.clone(), String::new())
    );
    assert_eq!(run(&["label", "--chain", "eth"], Some("base")).0, on_eth);
    assert_eq!(run(&["label"], None), (anywhere, String::new()));
    // The default chain applies, and says so.
    sb.ok(&["use", "eth"]);
    assert_eq!(
        run(&["label"], None),
        (on_eth, "chainz: eth (default)\n".to_string())
    );
    sb.cmd()
        .args(["label", "--chain", "ghost"])
        .write_stdin("x")
        .assert()
        .failure()
        .stderr(predicate::str::contains("chain 'ghost' not found"));
}

#[test]
fn label_passes_non_utf8_input_through() {
    let sb = eth_and_base();
    let mut input = vec![0xff, 0x00, b' '];
    input.extend_from_slice(USDC_BASE.as_bytes());
    input.extend_from_slice(&[b' ', 0xfe, b'\n', 0x80]);
    let mut want = vec![0xff, 0x00, b' '];
    want.extend_from_slice(format!("usdc({USDC_BASE})").as_bytes());
    want.extend_from_slice(&[b' ', 0xfe, b'\n', 0x80]);
    let out = sb
        .cmd()
        .args(["label", "--chain", "base"])
        .write_stdin(input)
        .assert()
        .success();
    assert_eq!(out.get_output().stdout, want);
}

#[test]
fn label_streams_line_by_line() {
    let sb = eth_and_base();
    let mut child = sb
        .process()
        .args(["label", "--chain", "base"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    // Each line comes back while stdin is still open.
    for (line, want) in [
        (format!("a {USDC_BASE}\n"), format!("a usdc({USDC_BASE})\n")),
        ("plain\n".to_string(), "plain\n".to_string()),
        (
            format!("{TEST_ADDRESS}\n"),
            format!("default({TEST_ADDRESS})\n"),
        ),
    ] {
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
        let mut got = String::new();
        stdout.read_line(&mut got).unwrap();
        assert_eq!(got, want);
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn label_exits_quietly_when_the_reader_goes_away() {
    let sb = eth_and_base();
    let mut child = sb
        .process()
        .args(["label", "--chain", "base"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let mut stdin = child.stdin.take().unwrap();
    // The write may fail once chainz has gone; either way it must exit 0.
    let _ = stdin.write_all(format!("{USDC_BASE}\n").repeat(64).as_bytes());
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------- doctor --tokens

/// What the mock chain says about one address.
#[derive(Clone)]
struct MockToken {
    code: bool,
    symbol: String,
    decimals: u8,
}

/// A call's data for a bare `selector`.
fn selector(selector: [u8; 4]) -> String {
    let hex: String = selector.iter().map(|b| format!("{b:02x}")).collect();
    format!("0x{hex}")
}

fn abi_string(s: &str) -> String {
    let mut data = vec![0u8; 64];
    data[31] = 32;
    data[63] = s.len() as u8;
    data.extend_from_slice(s.as_bytes());
    data.resize(64 + s.len().div_ceil(32) * 32, 0);
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// A JSON-RPC server answering `eth_chainId`, `eth_getCode` and the
/// `symbol()` / `decimals()` calls for the given tokens (keyed by lowercase
/// address). Unknown addresses have no code and revert.
fn token_rpc(chain_id: u64, tokens: HashMap<String, MockToken>) -> String {
    let rpc = MockRpc::start(move |request| {
        let params = &request.params;
        let token = |address: &Value| tokens.get(&address.as_str()?.to_ascii_lowercase());
        match request.method.as_str() {
            "eth_chainId" => Ok(json!(format!("{chain_id:#x}"))),
            "eth_getCode" => Ok(json!(if token(&params[0]).is_some_and(|t| t.code) {
                "0x6080"
            } else {
                "0x"
            })),
            "eth_call" => {
                let data = params[0]["data"].as_str().unwrap_or_default();
                match token(&params[0]["to"]) {
                    Some(t) if data == selector(abi::SYMBOL) => {
                        Ok(json!(format!("0x{}", abi_string(&t.symbol))))
                    }
                    Some(t) if data == selector(abi::DECIMALS) => {
                        Ok(json!(format!("0x{:064x}", t.decimals)))
                    }
                    _ => Err("execution reverted".into()),
                }
            }
            _ => Err("method not found".into()),
        }
    });
    rpc.url()
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
    let sb = seeded(&[("base", 8453, &base), ("arb", 42161, &arb)]);
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
    // `eth` answers with the wrong chain ID; `op` is unreachable.
    let wrong = token_rpc(5, HashMap::new());
    let sb = seeded(&[
        ("base", 8453, &rpc),
        ("eth", 1, &wrong),
        ("op", 10, "http://127.0.0.1:1"),
    ]);
    // A shadowing user label is a note.
    sb.ok(&["addr", "add", "weth", ROUTER, "--chain", "base"]);
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
    assert!(
        out.contains("✗ op (10): could not connect to endpoint; "),
        "{out}"
    );
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

#[test]
fn doctor_tokens_and_fix_conflict() {
    let sb = eth_and_base();
    sb.cmd()
        .args(["doctor", "--tokens", "--fix"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn addr_ls_and_find_json_and_rm_by_chain_alias() {
    let sb = eth_and_base();
    sb.ok(&["chain", "set", "base", "--alias", "basechain"]);
    sb.ok(&["addr", "add", "router", ROUTER, "--chain", "base"]);
    sb.ok(&["addr", "add", "usdc", ROUTER, "--chain", "base"]);

    let rows: Value = serde_json::from_str(&sb.ok(&["addr", "list", "--json"])).unwrap();
    let router = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "router")
        .expect("router listed");
    assert_eq!(
        router,
        &json!({"name": "router", "scope": "base", "chain_id": 8453,
                "address": ROUTER, "source": "user", "shadowed": false})
    );
    // The built-in @usdc on base is hidden by the user's label.
    let found: Value =
        serde_json::from_str(&sb.ok(&["addr", "find", USDC_BASE, "--json"])).unwrap();
    assert_eq!(
        found,
        json!([{"name": "usdc", "scope": "base", "chain_id": 8453, "address": USDC_BASE,
                "source": "built-in", "shadowed": true}])
    );
    let found: Value = serde_json::from_str(&sb.ok(&["addr", "find", ROUTER, "--json"])).unwrap();
    let names: Vec<&str> = found
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["router", "usdc"], "{found}");
    // Nothing found: an empty array, and exit 1 as in text mode.
    sb.cmd()
        .args(["addr", "find", UNKNOWN, "--json"])
        .assert()
        .code(1)
        .stdout("[]\n");

    // --chain takes an alias (exact, like a name or ID; never a prefix).
    sb.cmd()
        .args(["addr", "remove", "router", "--chain", "basechain"])
        .assert()
        .success()
        .stdout(format!("Removed @router on base ({ROUTER})\n"));
    sb.cmd()
        .args(["addr", "rm", "usdc", "--chain", "basech"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("exact chain name"));
}
