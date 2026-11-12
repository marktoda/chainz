//! `update`: endpoints, signer, explorer, name and aliases.

use crate::common::*;

/// Writes say what changed, from → to, with side effects; a no-op says
/// "No changes".
#[test]
fn update_reports_what_changed() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\nfailover = [\"b\", \"c\"]\n[chains.eth.rpcs]\na = \"http://127.0.0.1:1/a\"\nb = \"http://127.0.0.1:1/b\"\nc = \"http://127.0.0.1:1/c\"\n",
    );
    let before = sb.config();
    assert_eq!(
        sb.ok(&["update", "eth", "--rpc", "a"]),
        "No changes ('eth' already has these settings)\n"
    );
    assert_eq!(sb.config(), before);
    assert_eq!(
        sb.ok(&["update", "eth", "--rpc", "b"]),
        "Updated chain 'eth'\n  active endpoint: a → b\n  failover: b, c → c\n"
    );
    let out = sb.ok(&["update", "eth", "--rm-rpc", "c"]);
    assert_eq!(
        out,
        "Updated chain 'eth'\n  removed endpoint 'c'\n  failover: c → none\n"
    );
    sb.ok(&["use", "eth"]);
    assert!(sb.ok(&["use", "eth"]).starts_with("No changes"));
}

#[test]
fn update_applies_every_flag_in_one_write() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let rpc = Mock::chain_id(1);
    let url = rpc.url();
    let text = sb
        .cmd()
        .args([
            "update",
            "eth",
            "--rpc",
            &url,
            "--header",
            "x-team: ${TEAM}",
            "--no-signer",
            "--failover",
            "local",
            "--alias",
            "l1",
        ])
        .env("TEAM", "team-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&text);
    assert!(text.contains("Updated chain 'eth'"), "{text}");
    assert!(text.contains("signer: default → none"), "{text}");
    let show = sb.ok(&["ls", "eth", "--json"]);
    let show: serde_json::Value = serde_json::from_str(&show).unwrap();
    assert_eq!(show["failover"], serde_json::json!(["local"]));
    assert_eq!(show["aliases"], serde_json::json!(["l1"]));
    assert_eq!(show["signer"], serde_json::Value::Null);
    assert_ne!(show["rpc"], "local");
    // A new URL with the wrong chain ID is refused, and nothing is written.
    let before = sb.config();
    let wrong_rpc = Mock::chain_id(10);
    let wrong = wrong_rpc.url();
    sb.cmd()
        .args(["update", "eth", "--rpc", &wrong, "--name", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RPC check failed"));
    assert_eq!(sb.config(), before);
    // An API key needs an explorer.
    sb.cmd()
        .args(["update", "eth", "--api-key-stdin"])
        .write_stdin("KEY")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no explorer"));
    sb.cmd()
        .args([
            "update",
            "eth",
            "--explorer",
            "etherscan",
            "--api-key-stdin",
        ])
        .write_stdin("KEY")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "explorer key: stored in ${ETHERSCAN_API_KEY}",
        ));
    assert!(
        sb.secrets().contains("ETHERSCAN_API_KEY = \"KEY\""),
        "{}",
        sb.secrets()
    );
}

#[test]
fn update_renames_and_aliases() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd().args(["use", "eth"]).assert().success();
    sb.cmd()
        .args(["update", "eth", "--name", "mainnet"])
        .assert()
        .success();
    sb.cmd()
        .args(["update", "mainnet", "--alias", "ethereum"])
        .assert()
        .success();
    let text = sb.config();
    assert!(text.contains("default = \"mainnet\""));
    assert!(text.contains("aliases = [\"ethereum\"]"));
    sb.cmd()
        .args(["exec", "ethereum", "--", "sh", "-c", "echo $CHAINZ_CHAIN"])
        .assert()
        .success()
        .stdout("mainnet\n");
}

#[test]
fn update_adds_an_endpoint_with_headers_and_removes_it() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["var", "set", "KEY", "hdrvalue1234"])
        .assert()
        .success();
    let out = sb.ok(&[
        "update",
        "eth",
        "--add-rpc",
        "priv=https://private.invalid",
        "--add-rpc",
        "wss://ws.invalid",
        "--no-check",
    ]);
    assert!(
        out.contains("added endpoint 'priv'") && out.contains("added endpoint 'ws-ws'"),
        "{out}"
    );
    assert!(sb.config().contains("rpc = \"local\""), "not activated");
    // --header goes to the one --add-rpc endpoint; --rpc activates it.
    sb.ok(&["update", "eth", "--rm-rpc", "priv"]);
    sb.ok(&[
        "update",
        "eth",
        "--add-rpc",
        "priv=https://private.invalid",
        "--header",
        "x-api-key: ${KEY}",
        "--no-check",
    ]);
    assert!(
        sb.config().contains("\"x-api-key\" = \"${KEY}\"")
            || sb.config().contains("x-api-key = \"${KEY}\""),
        "{}",
        sb.config()
    );
    sb.ok(&["update", "eth", "--rpc", "priv"]);
    sb.cmd()
        .args(["exec", "eth", "--", "sh", "-c", "echo $ETH_RPC_HEADERS"])
        .assert()
        .success()
        .stdout("x-api-key: hdrvalue1234\n");
    sb.cmd()
        .args(["update", "eth", "--rm-rpc", "priv"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "'priv' is the active endpoint of 'eth'; activate another in the same command with --rpc NAME",
        ));
    let out = sb.ok(&["update", "eth", "--rpc", "local", "--rm-rpc", "priv"]);
    assert!(
        out.contains("removed endpoint 'priv'") && out.contains("active endpoint: priv → local"),
        "{out}"
    );
    // A new URL is checked unless --no-check (nothing listens here).
    sb.cmd()
        .args(["update", "eth", "--add-rpc", "http://127.0.0.1:1/x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RPC check failed"));
    assert!(!sb.config().contains("127.0.0.1:1/x"));
}

/// Pasting the same Alchemy URL again names the endpoint its key moved
/// into a variable for, instead of adding `alchemy-2`.
#[test]
fn update_with_a_url_whose_key_moved_adds_nothing() {
    let sb = Sandbox::new();
    sb.seed(&[("base", 8453)]);
    let url = "https://base-mainnet.g.alchemy.com/v2/abcdef0123456789abcdef0123455678";
    let out = sb.ok(&["update", "base", "--rpc", url, "--no-check"]);
    assert!(out.contains("added endpoint 'alchemy'"), "{out}");
    let out = sb.ok(&["update", "base", "--rpc", "local"]);
    assert!(out.contains("active endpoint"), "{out}");
    // `update --rpc` with the URL again activates `alchemy`.
    let out = sb.ok(&["update", "base", "--rpc", url, "--no-check"]);
    assert!(!out.contains("added endpoint"), "{out}");
    assert!(sb.config().contains("rpc = \"alchemy\""));
    // `--add-rpc` refuses it by name.
    sb.cmd()
        .args(["update", "base", "--add-rpc", url, "--no-check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("as endpoint 'alchemy'"));
    assert!(!sb.config().contains("alchemy-2"));
}

#[test]
fn update_off_a_tty_needs_a_chain_and_flags() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    for (args, usage) in [
        (&["update", "eth"][..], "Usage: chainz update"),
        (&["update"], "Usage: chainz update"),
        (&["update", "--no-signer"], "Usage: chainz update"),
    ] {
        sb.cmd()
            .args(args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains(usage))
            .stderr(predicate::str::contains("--help"));
    }
    // A usage error or an unknown chain is reported before stdin is read
    // (an empty stdin would fail as "API key from stdin was empty").
    for (args, error) in [
        (&["update", "--api-key-stdin"][..], "Usage: chainz update"),
        (
            &["update", "nope", "--api-key-stdin"],
            "chain 'nope' not found",
        ),
    ] {
        sb.cmd()
            .args(args)
            .write_stdin("")
            .assert()
            .failure()
            .stderr(predicate::str::contains(error))
            .stderr(predicate::str::contains("from stdin").not());
    }
}
