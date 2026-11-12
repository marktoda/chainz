//! `add`: from chainlist or by hand.

use crate::common::*;

/// An infura-style URL whose key `add`/`update --add-rpc` move to secrets.toml.
const INFURA_URL: &str = "https://mainnet.infura.invalid/v3/0123456789abcdef0123456789abcdef";
const INFURA_KEY: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn add_with_url_checks_the_chain_id() {
    let sb = Sandbox::new();
    let rpc = Mock::chain_id(8453);
    let url = rpc.url();
    sb.cmd()
        .args(["add", "8453", "--url", &url])
        .assert()
        .success()
        .stdout(predicate::str::contains("Added chain 'base'"));
    assert!(sb.config().contains("[chains.base]"));
    // Wrong ID is refused.
    let rpc = Mock::chain_id(1);
    let url = rpc.url();
    sb.cmd()
        .args(["add", "10", "--url", &url])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RPC check failed"));
}

/// With `--url` the chain is an ID or a built-in short or long name,
/// resolved offline: chainlist is never downloaded.
#[test]
fn add_with_url_stays_offline() {
    let sb = Sandbox::new();
    let out = sb.ok(&["add", "arb", "--url", "http://127.0.0.1:1", "--no-check"]);
    assert!(out.contains("Added chain 'arb' (42161)"), "{out}");
    let out = sb.ok(&[
        "add",
        "optimism",
        "--url",
        "http://127.0.0.1:1",
        "--no-check",
    ]);
    assert!(out.contains("Added chain 'op' (10)"), "{out}");
    let out = sb.run(&[
        "add",
        "mychain",
        "--url",
        "http://127.0.0.1:1",
        "--no-check",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(
        err.contains("not a chain ID or a built-in chain name"),
        "{err}"
    );
    assert!(err.contains("--name mychain"), "{err}");
    assert!(!sb.cache_file("chains.json").exists());
}

/// `add` attaches only an only signer; with several it says the
/// chain has none and how to attach one.
#[test]
fn add_with_several_signers_names_the_fix() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["signer", "add", "two", "--backend", "plaintext", "--stdin"])
        .write_stdin(TEST_KEY_2)
        .assert()
        .success();
    let out = sb.run(&["add", "base", "--url", "http://127.0.0.1:1", "--no-check"]);
    assert!(out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("base has no signer (2 signers to choose from: default, two); attach one with `chainz update base --signer NAME`"),
        "{err}"
    );
    let out = sb.run(&[
        "add",
        "op",
        "--url",
        "http://127.0.0.1:1",
        "--no-check",
        "--no-signer",
    ]);
    assert!(!stderr(&out).contains("no signer"), "{}", stderr(&out));
}

#[test]
fn etherscan_attaches_by_default_only_to_chains_it_covers() {
    let sb = Sandbox::new();
    sb.seed(&[("base", 8453)]);
    sb.cmd()
        .args(["update", "base", "--explorer", "etherscan"])
        .assert()
        .success()
        .stderr("");
    sb.cmd()
        .args(["add", "10", "--url", "http://127.0.0.1:1", "--no-check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("  explorer: etherscan\n"));
    sb.cmd()
        .args(["add", "31337", "--name", "local"])
        .args(["--url", "http://127.0.0.1:1", "--no-check"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "  explorer: none (etherscan doesn't cover chain ID 31337)",
        ));
    // base and op only.
    assert_eq!(sb.config().matches("explorer = \"etherscan\"").count(), 2);
    // Asked for by name, it attaches anyway, with a warning.
    sb.cmd()
        .args(["update", "local", "--explorer", "etherscan"])
        .assert()
        .success()
        .stdout(predicate::str::contains("explorer: none → etherscan"))
        .stderr(predicate::str::contains(
            "etherscan doesn't cover chain ID 31337; its API will reject this chain",
        ));
}

#[test]
fn a_failed_rpc_check_never_leaks_the_url() {
    let sb = Sandbox::new();
    sb.cmd()
        .args([
            "add",
            "8453",
            "--url",
            "http://127.0.0.1:1/v3/topsecretkey1234567",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("topsecretkey").not());
}

#[test]
fn add_from_chainlist_skips_invalid_urls_and_picks_a_live_one() {
    let sb = Sandbox::new();
    let live = chain_rpc(8453, 100, Duration::ZERO);
    // An invalid URL first: every probe result must stay paired with its
    // own URL, so the live one is still the one chosen.
    seed_chainlist(
        &sb,
        "Base",
        8453,
        &[
            "http://bad host",
            "http://127.0.0.1:1",
            &live.url(),
            "https://x.invalid/${API_KEY}",
        ],
    );
    let out = sb.run(&["add", "base"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "Added chain 'base' (8453) with 2 endpoints; active: local\n"
    );
    let (active, list) = endpoints(&sb, "base");
    assert_eq!(active, "local");
    assert_eq!(
        list,
        [
            ("local".to_string(), live.url()),
            ("local-2".to_string(), "http://127.0.0.1:1".to_string())
        ]
    );
    assert!(!sb.config().contains("bad host"));
}

#[test]
fn add_and_add_rpc_move_url_keys_into_secrets() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["add", "1", "--url", INFURA_URL, "--no-check"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("moved the secret in")
                .and(predicate::str::contains("${INFURA_KEY}")),
        );
    assert!(sb.config().contains("/v3/${INFURA_KEY}"), "{}", sb.config());
    assert!(!sb.config().contains(INFURA_KEY));
    assert!(
        sb.secrets()
            .contains(&format!("INFURA_KEY = \"{INFURA_KEY}\""))
    );

    // The same key again reuses the variable; a different one gets its own.
    let other = INFURA_URL.replace("0123", "9999");
    sb.cmd()
        .args([
            "update",
            "eth",
            "--add-rpc",
            &format!("second={other}"),
            "--no-check",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("moved the secret in"));
    let config = sb.config();
    assert!(!config.contains("9999"), "{config}");
    assert!(sb.secrets().contains("9999"));
    // Exec still sees the real URL.
    sb.cmd()
        .args([
            "exec",
            "eth",
            "--rpc",
            "second",
            "--",
            "sh",
            "-c",
            "echo $ETH_RPC_URL",
        ])
        .assert()
        .success()
        .stdout(format!("{other}\n"));
}

#[test]
fn add_without_a_chain_off_a_tty_is_a_usage_error_before_any_fetch() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["add"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Usage: chainz add"))
        .stderr(predicate::str::contains("chainlist").not());
    assert!(!sb.cache_file("chains.json").exists());
}
