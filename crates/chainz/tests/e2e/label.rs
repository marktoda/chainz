//! The address book: `label 0x…`, the stdin filter, `label add/ls/rm`,
//! and `@label` tokens in `exec`.

use crate::common::*;
use chainz_core::labels::{builtin_token, builtin_tokens};
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

/// `chainz exec <chain> -- echo <args>`, returning what echo printed.
fn echo(sb: &Sandbox, chain: &str, args: &[&str]) -> String {
    let mut all = vec!["exec", chain, "--", "echo"];
    all.extend_from_slice(args);
    sb.ok(&all).trim_end().to_string()
}

#[test]
fn label_add_ls_find_rm_roundtrip() {
    let sb = eth_and_base();
    sb.cmd()
        .args(["label", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("@default").and(predicate::str::contains("signer")));

    // Per chain, with any input case; stored checksummed.
    sb.cmd()
        .args([
            "label",
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
        .args(["label", "add", "myrouter", ROUTER_ETH, "--chain", "1"])
        .assert()
        .success()
        .stdout(format!("Added @myrouter on eth: {ROUTER_ETH}\n"));
    // Every chain, only when asked.
    sb.cmd()
        .args(["label", "add", "@my-safe", SAFE, "--all-chains"])
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
    let all = sb.ok(&["label", "ls"]);
    let lines: Vec<Vec<&str>> = all
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(
        lines,
        vec![
            vec!["NAME", "CHAIN", "ADDRESS", "SOURCE"],
            vec!["@myrouter", "base", ROUTER, "user"],
            vec!["@myrouter", "eth", ROUTER_ETH, "user"],
            vec!["@my-safe", "*", SAFE, "user"],
            vec!["@default", "*", TEST_ADDRESS, "signer"],
        ],
        "{all}"
    );
    let base = sb.ok(&["label", "ls", "--chain", "base"]);
    assert!(
        base.contains(ROUTER) && !base.contains(ROUTER_ETH),
        "{base}"
    );
    assert!(base.contains(SAFE) && base.contains("@default"), "{base}");
    assert!(!base.contains("@usdc"), "{base}");
    // --builtin adds the registry.
    let base = sb.ok(&["label", "ls", "--chain", "base", "--builtin"]);
    assert!(
        base.lines()
            .any(|l| l.starts_with("@usdc ") && l.contains(USDC_BASE) && l.contains("built-in")),
        "{base}"
    );
    assert!(!base.contains(USDT_ETH), "{base}");
    // Built-in tokens on the configured chains only, unless --all-chains.
    let all = sb.ok(&["label", "ls", "--builtin"]);
    assert!(all.contains(USDC_BASE) && all.contains(USDT_ETH), "{all}");
    let configured = builtin_tokens()
        .iter()
        .filter(|t| [1, 8453].contains(&t.chain_id))
        .count();
    assert_eq!(all.lines().count(), 1 + 4 + configured, "{all}");
    let every = sb.ok(&["label", "ls", "--builtin", "--all-chains"]);
    assert_eq!(
        every.lines().count(),
        1 + 4 + builtin_tokens().len(),
        "{every}"
    );
    sb.cmd()
        .args(["label", "ls", "--all-chains"])
        .assert()
        .code(2);

    // find: every label for the address, with its source.
    sb.cmd()
        .args(["label", &ROUTER.to_ascii_lowercase()])
        .assert()
        .success()
        .stdout("@myrouter on base (user)\n");
    sb.cmd()
        .args(["label", SAFE])
        .assert()
        .success()
        .stdout("@my-safe on every chain (user)\n");
    sb.cmd()
        .args(["label", TEST_ADDRESS])
        .assert()
        .success()
        .stdout("@default on every chain (signer)\n");
    sb.cmd()
        .args(["label", USDC_BASE])
        .assert()
        .success()
        .stdout("@usdc on base (built-in)\n");
    sb.cmd()
        .args(["label", UNKNOWN])
        .assert()
        .code(1)
        .stdout("")
        .stderr(format!("no label for {UNKNOWN}\n"));

    // rm: one chain's entry, then the whole label.
    sb.cmd()
        .args(["label", "rm", "myrouter", "--chain", "base", "--yes"])
        .assert()
        .success()
        .stdout(format!("Removed @myrouter on base ({ROUTER})\n"));
    let config = sb.config();
    assert!(
        config.contains(ROUTER_ETH) && !config.contains(ROUTER),
        "{config}"
    );
    sb.cmd()
        .args(["label", "rm", "myrouter", "-y"])
        .assert()
        .success()
        .stdout(format!("Removed @myrouter on eth ({ROUTER_ETH})\n"));
    // --all-chains is --chain '*', the stored form.
    sb.ok(&["label", "add", "other-safe", SAFE, "--all-chains"]);
    sb.cmd()
        .args(["label", "rm", "other-safe", "--all-chains"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "`chainz label rm other-safe --all-chains` removes @other-safe on every chain",
        ));
    sb.cmd()
        .args(["label", "rm", "other-safe", "--all-chains", "--yes"])
        .assert()
        .success()
        .stdout(format!("Removed @other-safe on every chain ({SAFE})\n"));
    sb.cmd()
        .args(["label", "rm", "my-safe", "--chain", "*", "--yes"])
        .assert()
        .success()
        .stdout(format!("Removed @my-safe on every chain ({SAFE})\n"));
    assert!(!sb.config().contains("addresses"), "{}", sb.config());
    sb.cmd().args(["label", SAFE]).assert().code(1);
}

#[test]
fn label_add_defaults_to_the_current_chain_context() {
    let sb = eth_and_base();
    // No --chain, no $CHAINZ_CHAIN, no default, no TTY: a usage error,
    // not a guess.
    sb.cmd()
        .args(["label", "add", "myrouter", ROUTER])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no chain given: pass --chain"))
        .stderr(predicate::str::contains("Usage: chainz label add"));
    assert!(!sb.config().contains("addresses"));

    // $CHAINZ_CHAIN (set inside an `exec` subshell).
    sb.cmd()
        .env("CHAINZ_CHAIN", "base")
        .args(["label", "add", "myrouter", ROUTER])
        .assert()
        .success()
        .stdout(format!("Added @myrouter on base: {ROUTER}\n"))
        .stderr("");
    // The default chain, with its stderr notice.
    sb.cmd().args(["use", "eth"]).assert().success();
    sb.cmd()
        .args(["label", "add", "myrouter", ROUTER_ETH])
        .assert()
        .success()
        .stdout(format!("Added @myrouter on eth: {ROUTER_ETH}\n"))
        .stderr("chainz: eth (default)\n");
    // An explicit chain wins over both.
    sb.cmd()
        .env("CHAINZ_CHAIN", "eth")
        .args(["label", "add", "other", SAFE, "--chain", "base"])
        .assert()
        .success()
        .stdout(format!("Added @other on base: {SAFE}\n"));
    // --chain and --all-chains exclude each other.
    sb.cmd()
        .args(["label", "add", "x", SAFE, "--chain", "base", "--all-chains"])
        .assert()
        .failure();
}

#[test]
fn label_add_rejects_bad_names_addresses_and_overwrites() {
    let sb = eth_and_base();
    sb.ok(&["label", "add", "myrouter", ROUTER, "--chain", "base"]);
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
            &["label", "add", name, ROUTER, "--chain", "base"],
            "context token",
        );
    }
    fails(
        &["label", "add", "default", ROUTER, "--all-chains"],
        "is a signer",
    );
    fails(
        &["label", "add", "my.router", ROUTER, "--chain", "base"],
        "[a-z0-9_-]+",
    );
    // Addresses: malformed, or mixed case with a wrong checksum.
    fails(
        &["label", "add", "x", "0x1234", "--chain", "base"],
        "is not an address",
    );
    fails(
        &["label", "add", "x", "vitalik.eth", "--chain", "base"],
        "is not an address",
    );
    let typo = ROUTER.replace("7E5F", "7e5F");
    fails(
        &["label", "add", "x", &typo, "--chain", "base"],
        "is not an address",
    );
    fails(
        &["label", "add", "x", ROUTER, "--chain", "ghost"],
        "not found",
    );
    // An existing entry is never overwritten silently.
    sb.cmd()
        .args(["label", "add", "myrouter", ROUTER_ETH, "--chain", "base"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains(format!("@myrouter is already {ROUTER} on base")).and(
                predicate::str::contains("chainz label rm myrouter --chain 'base'"),
            ),
        );
    assert_eq!(sb.config(), before);
    // The same address again is fine.
    sb.cmd()
        .args([
            "label",
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
    fails(&["label", "rm", "usdc"], "built-in token");
    fails(&["label", "rm", "default"], "is a signer");
    fails(&["label", "rm", "myroutr"], "did you mean myrouter?");
    fails(
        &["label", "rm", "myrouter", "--chain", "bas"],
        "exact chain name",
    );
    fails(
        &["label", "rm", "myrouter", "--chain", "eth"],
        "no address on eth",
    );
}

#[test]
fn shadowing_a_builtin_token_is_allowed_and_noted() {
    let sb = eth_and_base();
    sb.cmd()
        .args(["label", "add", "usdc", ROUTER, "--chain", "base"])
        .assert()
        .success()
        .stderr("⚠ @usdc now shadows the built-in token on base\n");
    assert_eq!(echo(&sb, "base", &["@usdc"]), ROUTER);
    // Only on that chain.
    let usdc_eth = builtin_token(1, "usdc").unwrap().address.to_string();
    assert_eq!(echo(&sb, "eth", &["@usdc"]), usdc_eth);
    let found = sb.ok(&["label", USDC_BASE]);
    assert_eq!(found, "@usdc on base (built-in, shadowed by your @usdc)\n");
    let listed = sb.ok(&["label", "ls", "--builtin"]);
    assert!(
        listed
            .lines()
            .any(|l| l.contains(USDC_BASE) && l.ends_with("built-in (shadowed)")),
        "{listed}"
    );
}

#[test]
fn label_ls_and_find_json_and_rm_by_chain_alias() {
    let sb = eth_and_base();
    sb.ok(&["update", "base", "--alias", "basechain"]);
    sb.ok(&["label", "add", "router", ROUTER, "--chain", "base"]);
    sb.ok(&["label", "add", "usdc", ROUTER, "--chain", "base"]);

    let rows: Value = serde_json::from_str(&sb.ok(&["label", "ls", "--json"])).unwrap();
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
    let found: Value = serde_json::from_str(&sb.ok(&["label", USDC_BASE, "--json"])).unwrap();
    assert_eq!(
        found,
        json!([{"name": "usdc", "scope": "base", "chain_id": 8453, "address": USDC_BASE,
                "source": "built-in", "shadowed": true}])
    );
    let found: Value = serde_json::from_str(&sb.ok(&["label", ROUTER, "--json"])).unwrap();
    let names: Vec<&str> = found
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["router", "usdc"], "{found}");
    // Nothing found: an empty array, and exit 1 as in text mode.
    sb.cmd()
        .args(["label", UNKNOWN, "--json"])
        .assert()
        .code(1)
        .stdout("[]\n");

    // --chain takes an alias (exact, like a name or ID; never a prefix).
    sb.cmd()
        .args(["label", "rm", "router", "--chain", "basechain", "-y"])
        .assert()
        .success()
        .stdout(format!("Removed @router on base ({ROUTER})\n"));
    sb.cmd()
        .args(["label", "rm", "usdc", "--chain", "basech"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("exact chain name"));
}

#[test]
fn exec_expands_user_labels_per_chain_and_for_every_chain() {
    let sb = eth_and_base();
    sb.ok(&["label", "add", "myrouter", ROUTER, "--chain", "base"]);
    sb.ok(&["label", "add", "my-safe", SAFE, "--all-chains"]);

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
    sb.ok(&["label", "add", "my-safe", ROUTER_ETH, "--chain", "eth"]);
    assert_eq!(echo(&sb, "eth", &["@my-safe"]), ROUTER_ETH);
    assert_eq!(echo(&sb, "base", &["@my-safe"]), SAFE);

    // Removing the label makes the token unknown again.
    sb.ok(&["label", "rm", "myrouter", "--yes"]);
    sb.cmd()
        .args(["exec", "base", "--", "echo", "@myrouter"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown token @myrouter"));
}

#[test]
fn exec_errors_when_a_label_has_no_address_on_the_chain() {
    let sb = eth_and_base();
    sb.ok(&["label", "add", "myrouter", ROUTER, "--chain", "base"]);
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

#[test]
fn label_rewrites_addresses_for_the_chain() {
    let sb = eth_and_base();
    sb.ok(&["label", "add", "myrouter", ROUTER, "--chain", "base"]);
    sb.ok(&["label", "add", "pool", ROUTER, "--chain", "eth"]);
    sb.ok(&["label", "add", "my-safe", SAFE, "--all-chains"]);
    let input = format!(
        "to {ROUTER} from {} token {USDC_BASE} safe {SAFE}\nword 0x000000000000000000000000{} other {UNKNOWN}\n",
        TEST_ADDRESS.to_ascii_lowercase(),
        &ROUTER[2..]
    );
    // A `cast logs` topic: the left-padded address is labelled, kept as is.
    let word = format!("0x000000000000000000000000{}", &ROUTER[2..]);
    let rest = |name: Option<&str>| match name {
        Some(name) => format!("\nword {name}({word}) other {UNKNOWN}\n"),
        None => format!("\nword {word} other {UNKNOWN}\n"),
    };
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
        "to myrouter({ROUTER}) from default({TEST_ADDRESS}) token usdc({USDC_BASE}) safe my-safe({SAFE}){}",
        rest(Some("myrouter"))
    );
    let on_eth = format!(
        "to pool({ROUTER}) from default({TEST_ADDRESS}) token {USDC_BASE} safe my-safe({SAFE}){}",
        rest(Some("pool"))
    );
    // No chain: "*" labels, signers, and addresses with one name everywhere.
    let anywhere = format!(
        "to {ROUTER} from default({TEST_ADDRESS}) token usdc({USDC_BASE}) safe my-safe({SAFE}){}",
        rest(None)
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
