//! End-to-end CLI tests. Every invocation runs inside a sandbox (see
//! `common`): cleared environment, scratch `HOME` and XDG directories, the
//! OS keyring disabled.

mod common;

use common::{MockRpc, Sandbox, TEST_ADDRESS, TEST_KEY, mode, set_mode, stderr, stdout};
use predicates::prelude::*;
use std::fs;
use std::path::Path;

/// Well-known anvil test key #1.
const TEST_KEY_2: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const TEST_ADDRESS_2: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// A mock answering `eth_chainId` with `chain_id`.
fn chain_id_rpc(chain_id: u64) -> String {
    MockRpc::chain_id(chain_id).url()
}

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../chainz-core/tests/fixtures")
        .join(name);
    fs::read_to_string(path).unwrap()
}

// ---------------------------------------------------------------- basics

#[test]
fn version_flag_works() {
    let sb = Sandbox::new();
    sb.cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("chainz"));
}

#[test]
fn ls_on_empty_config_shows_hint_and_writes_nothing() {
    let sb = Sandbox::new();
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("no chains configured yet"))
        .stdout(predicate::str::contains("Quick start:"));
    assert!(
        !sb.config_path().exists(),
        "a read-only command must not create config"
    );
}

#[test]
fn completions_generate_for_zsh_and_bash() {
    let sb = Sandbox::new();
    for shell in ["zsh", "bash", "fish"] {
        sb.cmd()
            .args(["completions", shell])
            .assert()
            .success()
            .stdout(predicate::str::contains("chainz"));
    }
}

/// `chainz rpc ls | head` and friends: a reader that went away ends the
/// command quietly with exit 0, never a panic.
#[test]
fn a_closed_stdout_exits_quietly() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453), ("op", 10)]);
    sb.write_secrets("[vars]\nTOKEN = \"t\"\n");
    for args in [
        &["rpc", "ls"][..],
        &["var", "ls"],
        &["addr", "ls"],
        &["addr", "ls", "--builtin"],
        &["ls"],
    ] {
        let mut child = sb
            .process()
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Close the read end before chainz writes anything.
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        let err = stderr(&out);
        assert!(!err.contains("panicked"), "{args:?}: {err}");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {err}");
    }
}

// ---------------------------------------------------------------- config files

#[test]
fn config_and_secrets_are_owner_only() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["var", "set", "API", "value"])
        .assert()
        .success();
    assert_eq!(mode(&sb.secrets_path()), 0o600);
    assert_eq!(mode(&sb.config_path()), 0o600);
    assert_eq!(mode(&sb.dir()), 0o700);
}

#[test]
fn loose_permissions_are_tightened_on_load() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    set_mode(&sb.config_path(), 0o644);
    set_mode(&sb.secrets_path(), 0o644);
    sb.cmd().args(["var", "set", "X", "1"]).assert().success();
    assert_eq!(mode(&sb.config_path()), 0o600);
    assert_eq!(mode(&sb.secrets_path()), 0o600);
}

#[test]
fn corrupt_config_errors_and_is_not_overwritten() {
    let sb = Sandbox::new();
    sb.write_config("version = 2\n[chains.eth\n");
    sb.cmd()
        .arg("ls")
        .assert()
        .failure()
        .stderr(predicate::str::contains("config.toml"));
    sb.cmd().args(["var", "set", "X", "1"]).assert().failure();
    assert_eq!(sb.config(), "version = 2\n[chains.eth\n");
}

#[test]
fn errors_use_the_same_prefix_as_clap() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    // An error from a handler (anyhow) and a usage error (clap).
    let handler = sb.run(&["chain", "show", "nope"]);
    assert_eq!(handler.status.code(), Some(1));
    assert!(stderr(&handler).starts_with("error: "), "{}", stderr(&handler));
    let usage = sb.run(&["ls", "--nope"]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(stderr(&usage).starts_with("error: "), "{}", stderr(&usage));
}

#[test]
fn unknown_keys_are_rejected_with_their_location() {
    let sb = Sandbox::new();
    sb.write_config("version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\nrpx = 1\n[chains.eth.rpcs]\na = \"http://x.invalid\"\n");
    sb.cmd()
        .arg("ls")
        .assert()
        .failure()
        .stderr(predicate::str::contains("rpx"));
}

#[test]
fn concurrent_updates_do_not_lose_changes() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let children: Vec<_> = (0..8)
        .map(|i| {
            sb.process()
                .args(["var", "set", &format!("V{i}"), "value"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let secrets = sb.secrets();
    for i in 0..8 {
        assert!(
            secrets.contains(&format!("V{i} = ")),
            "lost V{i}: {secrets}"
        );
    }
}

#[test]
fn edits_preserve_comments_and_order() {
    let sb = Sandbox::new();
    sb.write_config(
        "# my chains\nversion = 2\n\n# mainnet first\n[chains.eth]\nid = 1\nrpc = \"a\" # keep me\n\n[chains.eth.rpcs]\na = \"http://a.invalid\"\nb = \"http://b.invalid\"\n",
    );
    sb.cmd()
        .args(["chain", "set", "eth", "--rpc", "b"])
        .assert()
        .success();
    let text = sb.config();
    assert!(text.contains("# my chains"));
    assert!(text.contains("# mainnet first"));
    assert!(text.contains("rpc = \"b\""));
}

// ---------------------------------------------------------------- migration

#[test]
fn v1_config_is_migrated_on_first_run() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.dir()).unwrap();
    fs::write(sb.dir().join("config.json"), fixture("v1-config.json")).unwrap();
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "migrated 17 chains, 149 endpoints, 3 signers, 4 variables",
        ));
    assert!(sb.config_path().exists());
    assert!(sb.dir().join("config.v1.json.bak").exists());
    assert!(!sb.dir().join("config.json").exists());
    // Second run: nothing to migrate, no notice.
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stderr(predicate::str::contains("migrated").not());
}

#[test]
fn legacy_home_config_is_migrated() {
    let sb = Sandbox::new();
    fs::write(sb.home().join(".chainz.json"), fixture("v1-config.json")).unwrap();
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stderr(predicate::str::contains("migrated 17 chains"));
    assert!(sb.config_path().exists());
    assert!(sb.home().join(".chainz.v1.json.bak").exists());
}

#[test]
fn migrate_dry_run_writes_nothing() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.dir()).unwrap();
    fs::write(sb.dir().join("config.json"), fixture("v1-config.json")).unwrap();
    let out = sb
        .cmd()
        .args(["config", "migrate", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(stdout(&out).contains("version = 2"));
    assert!(stdout(&out).contains("[chains."));
    assert!(String::from_utf8_lossy(&out.stderr).contains("dry run"));
    assert!(!sb.config_path().exists());
    assert!(!sb.secrets_path().exists());
    assert!(sb.dir().join("config.json").exists());
    // The fixture's fake keys never reach stdout.
    assert!(!stdout(&out).contains("private_key"));
}

#[test]
fn migrated_config_never_prints_secrets() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.dir()).unwrap();
    fs::write(sb.dir().join("config.json"), fixture("v1-config.json")).unwrap();
    let out = sb.cmd().arg("ls").assert().success().get_output().clone();
    let secrets = fs::read_to_string(sb.secrets_path()).unwrap();
    let all = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    for line in secrets.lines() {
        if let Some((_, value)) = line.split_once(" = ") {
            let value = value.trim_matches('"');
            if value.len() >= 12 {
                assert!(!all.contains(value), "printed a secret value");
            }
        }
    }
}

// ---------------------------------------------------------------- vars

#[test]
fn var_set_get_ls_rm_roundtrip() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["var", "set", "API_KEY", "supersecret123"])
        .assert()
        .success();
    sb.cmd()
        .args(["var", "get", "API_KEY"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("REDACTED").and(predicate::str::contains("supersecret").not()),
        );
    sb.cmd()
        .args(["var", "get", "API_KEY", "--reveal"])
        .assert()
        .success()
        .stdout("supersecret123\n");
    sb.cmd().args(["var", "ls"]).assert().success().stdout(
        predicate::str::contains("API_KEY").and(predicate::str::contains("supersecret").not()),
    );
    assert!(
        !sb.config().contains("supersecret"),
        "values live in secrets.toml"
    );
    sb.cmd()
        .args(["var", "rm", "API_KEY"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "`chainz var rm API_KEY` removes variable 'API_KEY' (its value is kept nowhere else); pass --yes to confirm",
        ));
    sb.cmd()
        .args(["var", "rm", "API_KEY", "-y"])
        .assert()
        .success();
    sb.cmd().args(["var", "get", "API_KEY"]).assert().failure();
}

#[test]
fn var_stdin_preserves_intentional_whitespace() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["var", "set", "SPACED", "--stdin"])
        .write_stdin("  value \n")
        .assert()
        .success();
    sb.cmd()
        .args(["var", "get", "SPACED", "--reveal"])
        .assert()
        .success()
        .stdout("  value \n");
}

#[test]
fn var_ls_json_is_machine_readable() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["var", "set", "A", "secretvalue"])
        .assert()
        .success();
    let out = sb
        .cmd()
        .args(["var", "ls", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v[0]["name"], "A");
    assert!(!stdout(&out).contains("secretvalue"));
}

#[test]
fn referenced_var_removal_is_blocked() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = \"https://x.invalid/v3/${INFURA}\"\n",
    );
    sb.write_secrets("[vars]\nINFURA = \"k\"\n");
    sb.cmd()
        .args(["var", "rm", "INFURA"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("chains.eth.rpcs.a"));
}

#[test]
fn builtin_var_names_are_reserved() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["var", "set", "CHAIN_ID", "1"])
        .assert()
        .failure();
}

// ---------------------------------------------------------------- signers

#[test]
fn signer_add_noninteractive_with_stdin_and_explicit_plaintext() {
    let sb = Sandbox::new();
    sb.cmd()
        .args([
            "signer",
            "add",
            "deployer",
            "--stdin",
            "--backend",
            "plaintext",
        ])
        .write_stdin(format!("{TEST_KEY}\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains(TEST_ADDRESS));
    assert!(sb.config().contains(TEST_ADDRESS));
    assert!(
        !sb.config().contains(TEST_KEY),
        "key material lives in secrets.toml"
    );
    assert!(sb.secrets().contains(TEST_KEY));
}

#[test]
fn bare_signer_add_never_falls_back_to_plaintext() {
    let sb = Sandbox::new();
    // No keyring (disabled) and no TTY: encrypted needs a prompt → error.
    sb.cmd()
        .args(["signer", "add", "deployer", "--stdin"])
        .write_stdin(TEST_KEY)
        .assert()
        .failure()
        .stderr(predicate::str::contains("interactive"));
    assert!(!sb.secrets().contains(TEST_KEY));
}

#[test]
fn signer_add_rejects_invalid_key_and_duplicates() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["signer", "add", "bad", "--stdin", "--backend", "plaintext"])
        .write_stdin("not-a-key")
        .assert()
        .failure();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args([
            "signer",
            "add",
            "default",
            "--stdin",
            "--backend",
            "plaintext",
        ])
        .write_stdin(TEST_KEY_2)
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn signer_rm_requires_unassign_when_attached() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["signer", "rm", "default"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--unassign"));
    sb.cmd()
        .args(["signer", "rm", "default", "--unassign", "--yes"])
        .assert()
        .success();
    assert!(!sb.config().contains("[signers.default]"));
    assert!(!sb.secrets().contains(TEST_KEY));
    assert!(!sb.config().contains("signer = \"default\""));
}

#[test]
fn signer_ls_json_never_leaks_key_material() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let out = sb
        .cmd()
        .args(["signer", "ls", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v[0]["address"], TEST_ADDRESS);
    assert_eq!(v[0]["chains"][0], "eth");
    assert!(!stdout(&out).contains(TEST_KEY));
}

#[test]
fn keystore_signer_is_a_reference() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let ks = sb.home().join("ks.json");
    fs::write(&ks, "{}").unwrap();
    sb.cmd()
        .args([
            "signer",
            "add",
            "ledgerish",
            "--backend",
            "keystore",
            "--path",
        ])
        .arg(&ks)
        .args(["--address", TEST_ADDRESS_2])
        .assert()
        .success();
    sb.cmd()
        .args([
            "exec",
            "eth",
            "--signer",
            "ledgerish",
            "--",
            "sh",
            "-c",
            "echo $ETH_KEYSTORE $ETH_FROM",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ks.json").and(predicate::str::contains(TEST_ADDRESS_2)));
}

// ---------------------------------------------------------------- chains

#[test]
fn chain_add_by_hand_checks_chain_id() {
    let sb = Sandbox::new();
    let url = chain_id_rpc(8453);
    sb.cmd()
        .args(["chain", "add", "8453", "--url", &url])
        .assert()
        .success()
        .stdout(predicate::str::contains("Added chain 'base'"));
    assert!(sb.config().contains("[chains.base]"));
    // Wrong ID is refused.
    let url = chain_id_rpc(1);
    sb.cmd()
        .args(["chain", "add", "10", "--url", &url])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RPC check failed"));
}

/// `chain add` attaches only an only signer; with several it says the
/// chain has none and how to attach one.
#[test]
fn chain_add_with_several_signers_names_the_fix() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["signer", "add", "two", "--backend", "plaintext", "--stdin"])
        .write_stdin(TEST_KEY_2)
        .assert()
        .success();
    let out = sb.run(&[
        "chain",
        "add",
        "base",
        "--url",
        "http://127.0.0.1:1",
        "--no-check",
    ]);
    assert!(out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("base has no signer (2 signers to choose from: default, two); attach one with `chainz chain set base --signer <name>`"),
        "{err}"
    );
    let out = sb.run(&[
        "chain",
        "add",
        "op",
        "--url",
        "http://127.0.0.1:1",
        "--no-check",
        "--no-signer",
    ]);
    assert!(!stderr(&out).contains("no signer"), "{}", stderr(&out));
}

/// With `--url` the chain is an ID or a built-in short name, resolved
/// offline: chainlist is never downloaded.
#[test]
fn chain_add_with_url_stays_offline() {
    let sb = Sandbox::new();
    let out = sb.ok(&[
        "chain",
        "add",
        "arb",
        "--url",
        "http://127.0.0.1:1",
        "--no-check",
    ]);
    assert!(out.contains("Added chain 'arb' (42161)"), "{out}");
    let out = sb.run(&[
        "chain",
        "add",
        "mychain",
        "--url",
        "http://127.0.0.1:1",
        "--no-check",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(
        err.contains("not a chain ID or a built-in short name"),
        "{err}"
    );
    assert!(err.contains("--name mychain"), "{err}");
    assert!(!sb.cache_file("chains.json").exists());
}

#[test]
fn rpc_failure_never_leaks_the_url() {
    let sb = Sandbox::new();
    sb.cmd()
        .args([
            "chain",
            "add",
            "8453",
            "--url",
            "http://127.0.0.1:1/v3/topsecretkey1234567",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("topsecretkey").not());
}

/// Pasting the same Alchemy URL again names the endpoint its key moved
/// into a variable for, instead of adding `alchemy-2`.
#[test]
fn re_adding_a_url_whose_key_moved_adds_nothing() {
    let sb = Sandbox::new();
    sb.seed(&[("base", 8453)]);
    let url = "https://base-mainnet.g.alchemy.com/v2/abcdef0123456789abcdef0123455678";
    let out = sb.ok(&["chain", "set", "base", "--rpc", url, "--no-check"]);
    assert!(out.contains("added endpoint 'alchemy'"), "{out}");
    let out = sb.ok(&["chain", "set", "base", "--rpc", "local"]);
    assert!(out.contains("active endpoint"), "{out}");
    // `chain set --rpc` with the URL again activates `alchemy`.
    let out = sb.ok(&["chain", "set", "base", "--rpc", url, "--no-check"]);
    assert!(!out.contains("added endpoint"), "{out}");
    assert!(sb.config().contains("rpc = \"alchemy\""));
    // `rpc add` refuses it by name.
    sb.cmd()
        .args(["rpc", "add", "base", url, "--no-check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("as endpoint 'alchemy'"));
    assert!(!sb.config().contains("alchemy-2"));
}

#[test]
fn chain_rm_by_name_and_id_requires_exact_match() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453), ("arb", 42161)]);
    sb.cmd()
        .args(["chain", "rm", "ba", "-y"])
        .assert()
        .failure();
    sb.cmd()
        .args(["chain", "rm", "base", "-y"])
        .assert()
        .success();
    sb.cmd()
        .args(["chain", "rm", "42161", "--yes"])
        .assert()
        .success();
    sb.cmd()
        .args(["chain", "rm", "nope", "-y"])
        .assert()
        .failure();
    let text = sb.config();
    assert!(text.contains("[chains.eth]"));
    assert!(!text.contains("[chains.base]") && !text.contains("[chains.arb]"));
}

/// `chain rm` asks (off a terminal: needs --yes) and names what goes
/// with the chain: its labels and the default.
#[test]
fn chain_rm_names_its_labels_and_the_default() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("arb", 42161)]);
    sb.ok(&["use", "arb"]);
    sb.ok(&["addr", "add", "myrouter", TEST_ADDRESS_2, "--chain", "arb"]);
    sb.ok(&["addr", "add", "pool", TEST_ADDRESS, "--chain", "arb"]);
    let out = sb.run(&["chain", "rm", "arb"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("removes chain 'arb' (1 endpoint(s); labels @myrouter, @pool; it is the default chain); pass --yes"),
        "{err}"
    );
    assert!(sb.config().contains("[chains.arb]"));
    let out = sb.ok(&["chain", "rm", "arb", "--yes"]);
    assert!(out.contains("Removed chain 'arb'"), "{out}");
    assert!(out.contains("dropped labels: @myrouter, @pool"), "{out}");
    assert!(out.contains("default cleared"), "{out}");
}

/// Writes say what changed, from → to, with side effects; a no-op says
/// "No changes".
#[test]
fn chain_set_and_rpc_rm_report_what_changed() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\nfailover = [\"b\", \"c\"]\n[chains.eth.rpcs]\na = \"http://127.0.0.1:1/a\"\nb = \"http://127.0.0.1:1/b\"\nc = \"http://127.0.0.1:1/c\"\n",
    );
    let before = sb.config();
    assert_eq!(
        sb.ok(&["chain", "set", "eth", "--rpc", "a"]),
        "No changes ('eth' already has these settings)\n"
    );
    assert_eq!(sb.config(), before);
    assert_eq!(
        sb.ok(&["chain", "set", "eth", "--rpc", "b"]),
        "Updated chain 'eth'\n  active endpoint: a → b\n  failover: b, c → c\n"
    );
    let out = sb.ok(&["rpc", "rm", "eth", "c", "--yes"]);
    assert_eq!(
        out,
        "Removed endpoint 'c' from 'eth'\n  also removed from failover (now: none)\n"
    );
    sb.ok(&["use", "eth"]);
    assert!(sb.ok(&["use", "eth"]).starts_with("No changes"));
}

#[test]
fn chain_set_renames_and_aliases() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd().args(["use", "eth"]).assert().success();
    sb.cmd()
        .args(["chain", "set", "eth", "--name", "mainnet"])
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
fn ls_is_compact_and_json_is_machine_readable() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("eth").and(predicate::str::contains("8453")));
    let out = sb
        .cmd()
        .args(["ls", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v[1]["name"], "base");
    assert_eq!(v[1]["id"], 8453);
    assert_eq!(v[1]["signer"], "default");
}

#[test]
fn output_redacts_credentials_unless_revealed() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"lit\"\n[chains.eth.rpcs]\nlit = \"https://x.invalid/v3/abcdef0123456789abcdef\"\nhdr = { url = \"https://y.invalid\", headers = { \"x-api-key\" = \"headersecret999\" } }\n",
    );
    for args in [
        vec!["ls"],
        vec!["ls", "--json"],
        vec!["chain", "show", "eth"],
        vec!["chain", "show", "eth", "--json"],
        vec!["rpc", "ls"],
    ] {
        let out = sb.cmd().args(&args).assert().success().get_output().clone();
        let s = stdout(&out);
        assert!(
            !s.contains("abcdef0123456789abcdef"),
            "{args:?} leaked a URL key"
        );
        assert!(!s.contains("headersecret999"), "{args:?} leaked a header");
    }
    sb.cmd()
        .args(["chain", "show", "eth", "--reveal"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("abcdef0123456789abcdef")
                .and(predicate::str::contains("headersecret999")),
        );
}

#[test]
fn rpc_add_with_header_activate_and_rm() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["var", "set", "KEY", "hdrvalue1234"])
        .assert()
        .success();
    sb.cmd()
        .args([
            "rpc",
            "add",
            "eth",
            "https://private.invalid",
            "--name",
            "priv",
            "--header",
            "x-api-key: ${KEY}",
            "--no-check",
            "--activate",
        ])
        .assert()
        .success();
    assert!(
        sb.config().contains("\"x-api-key\" = \"${KEY}\"")
            || sb.config().contains("x-api-key = \"${KEY}\"")
    );
    sb.cmd()
        .args(["exec", "eth", "--", "sh", "-c", "echo $ETH_RPC_HEADERS"])
        .assert()
        .success()
        .stdout("x-api-key: hdrvalue1234\n");
    sb.cmd()
        .args(["rpc", "rm", "eth", "priv"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("active"));
    sb.cmd()
        .args(["chain", "set", "eth", "--rpc", "local"])
        .assert()
        .success();
    sb.cmd()
        .args(["rpc", "rm", "eth", "priv"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pass --yes to confirm"));
    sb.cmd()
        .args(["rpc", "rm", "eth", "priv", "--yes"])
        .assert()
        .success();
}

// ---------------------------------------------------------------- exec

#[test]
fn exec_exports_context_and_expands_tokens() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["exec", "eth", "--", "sh", "-c", "echo $ETH_RPC_URL $CHAIN_ID $CHAINZ_CHAIN $CHAIN_NAME $ETH_FROM $WALLET_ADDRESS \"$1\" \"$2\" \"$3\"", "sh", "@chainid", "@wallet", "@rpc"])
        .assert()
        .success()
        .stdout(format!(
            "http://127.0.0.1:1 1 eth eth {TEST_ADDRESS} {TEST_ADDRESS} 1 {TEST_ADDRESS} http://127.0.0.1:1\n"
        ));
}

#[test]
fn exec_x_alias_and_embedded_tokens() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    sb.cmd()
        .args([
            "x",
            "eth",
            "--",
            "echo",
            "id=@chainid",
            "other=@chainid:base",
            "mail@@example.com",
        ])
        .assert()
        .success()
        .stdout("id=1 other=8453 mail@example.com\n");
}

#[test]
fn exec_unknown_whole_token_errors_with_suggestion() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["exec", "eth", "--", "echo", "@walet"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("did you mean @wallet"));
    // Embedded unknown tokens pass through.
    sb.cmd()
        .args(["exec", "eth", "--", "echo", "a@b.c"])
        .assert()
        .success()
        .stdout("a@b.c\n");
}

#[test]
fn exec_key_token_is_removed_and_deprecated_tokens_warn() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["exec", "eth", "--", "echo", "@key"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--expose-key"));
    sb.cmd()
        .args(["exec", "eth", "--", "echo", "@chainname"])
        .assert()
        .success()
        .stdout("eth\n")
        .stderr(predicate::str::contains("deprecated"));
}

#[test]
fn exec_does_not_expose_key_unless_requested() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["exec", "eth", "--", "sh", "-c", "echo [$RAW_PRIVATE_KEY]"])
        .assert()
        .success()
        .stdout("[]\n");
    sb.cmd()
        .args([
            "exec",
            "eth",
            "--expose-key",
            "--",
            "sh",
            "-c",
            "echo $RAW_PRIVATE_KEY",
        ])
        .assert()
        .success()
        .stdout(format!("{TEST_KEY}\n"));
}

#[test]
fn exec_removes_stale_context_from_the_parent_env() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .env("ETHERSCAN_API_KEY", "stale")
        .env("RAW_PRIVATE_KEY", "stale")
        .args([
            "exec",
            "eth",
            "--",
            "sh",
            "-c",
            "echo [$ETHERSCAN_API_KEY][$RAW_PRIVATE_KEY]",
        ])
        .assert()
        .success()
        .stdout("[][]\n");
}

#[test]
fn exec_passes_through_exit_code() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["exec", "eth", "--", "sh", "-c", "exit 42"])
        .assert()
        .code(42);
}

#[test]
fn exec_without_command_or_unknown_chain_fails() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd().args(["exec", "eth"]).assert().failure();
    sb.cmd()
        .args(["exec", "ethh", "--", "true"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ethh"));
}

#[test]
fn exec_resolves_prefix_alias_and_id() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.arbitrum]\nid = 42161\naliases = [\"arb1\"]\nrpc = \"a\"\n[chains.arbitrum.rpcs]\na = \"http://127.0.0.1:1\"\n",
    );
    for q in ["arbitrum", "arbi", "arb1", "42161"] {
        sb.cmd()
            .args(["exec", q, "--", "sh", "-c", "echo $CHAINZ_CHAIN"])
            .assert()
            .success()
            .stdout("arbitrum\n");
    }
}

#[test]
fn exec_signer_and_rpc_overrides() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args([
            "signer",
            "add",
            "other",
            "--stdin",
            "--backend",
            "plaintext",
        ])
        .write_stdin(TEST_KEY_2)
        .assert()
        .success();
    sb.cmd()
        .args([
            "rpc",
            "add",
            "eth",
            "http://127.0.0.2:1",
            "--name",
            "two",
            "--no-check",
        ])
        .assert()
        .success();
    sb.cmd()
        .args([
            "exec",
            "eth",
            "--signer",
            "other",
            "--rpc",
            "two",
            "--",
            "sh",
            "-c",
            "echo $ETH_FROM $ETH_RPC_URL \"$1\"",
            "sh",
            "@default",
        ])
        .assert()
        .success()
        .stdout(format!(
            "{TEST_ADDRESS_2} http://127.0.0.2:1 {TEST_ADDRESS}\n"
        ));
}

#[test]
fn exec_expands_variables_in_urls() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = \"https://x.invalid/${CHAIN_ID}/${KEY}\"\n",
    );
    sb.write_secrets("[vars]\nKEY = \"k123\"\n");
    sb.cmd()
        .args(["exec", "eth", "--", "sh", "-c", "echo $ETH_RPC_URL"])
        .assert()
        .success()
        .stdout("https://x.invalid/1/k123\n");
    // The environment overrides secrets.toml.
    sb.cmd()
        .env("KEY", "fromenv")
        .args(["exec", "eth", "--", "sh", "-c", "echo $ETH_RPC_URL"])
        .assert()
        .success()
        .stdout("https://x.invalid/1/fromenv\n");
}

#[test]
fn exec_explain_redacts_and_does_not_run() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = \"https://x.invalid/v3/${KEY}\"\n",
    );
    sb.write_secrets("[vars]\nKEY = \"verysecretkey12345\"\n");
    sb.cmd()
        .args([
            "exec",
            "eth",
            "--explain",
            "--",
            "sh",
            "-c",
            "exit 9",
            "@rpc",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ETH_RPC_URL=")
                .and(predicate::str::contains("argv"))
                .and(predicate::str::contains("verysecretkey12345").not()),
        );
}

#[test]
fn use_sets_default_with_stderr_notice() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    sb.cmd().args(["exec", "--", "true"]).assert().failure();
    sb.cmd().args(["use", "base"]).assert().success();
    sb.cmd()
        .args(["exec", "--", "sh", "-c", "echo $CHAIN_ID"])
        .assert()
        .success()
        .stdout("8453\n")
        .stderr(predicate::str::contains("chainz: base (default)"));
    // $CHAINZ_CHAIN beats the default, silently.
    sb.cmd()
        .env("CHAINZ_CHAIN", "eth")
        .args(["exec", "--", "sh", "-c", "echo $CHAIN_ID"])
        .assert()
        .success()
        .stdout("1\n")
        .stderr(predicate::str::contains("default").not());
    sb.cmd().args(["use", "nope"]).assert().failure();
    sb.cmd().args(["use", "--clear"]).assert().success();
    assert!(!sb.config().contains("default ="));
}

#[test]
fn shell_exports_context_and_passes_exit_code() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .env("SHELL", "sh")
        .args(["shell", "eth"])
        .write_stdin("echo $CHAIN_ID $CHAINZ_CHAIN; exit 3\n")
        .assert()
        .code(3)
        .stdout("1 eth\n");
}

#[test]
fn env_prints_shell_exports() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["env", "eth", "--shell", "bash"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("export CHAIN_ID=1;")
                .and(predicate::str::contains("unset ETHERSCAN_API_KEY;"))
                .and(predicate::str::contains(TEST_KEY).not()),
        );
    sb.cmd()
        .args(["env", "eth", "--shell", "fish"])
        .assert()
        .success()
        .stdout(predicate::str::contains("set -gx CHAIN_ID '1';"));
}

#[test]
fn rpc_only_chain_runs_until_a_signer_is_needed() {
    let sb = Sandbox::new();
    sb.write_config("version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = \"http://127.0.0.1:1\"\n");
    sb.cmd()
        .args(["exec", "eth", "--", "true"])
        .assert()
        .success();
    sb.cmd()
        .args(["exec", "eth", "--", "echo", "@wallet"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("signer"));
}

// ---------------------------------------------------------------- doctor

#[test]
fn doctor_reports_dead_rpc_and_plaintext_signer() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd().arg("doctor").assert().code(1).stdout(
        predicate::str::contains("plaintext")
            .and(predicate::str::contains("eth: active endpoint 'local'")),
    );
}

#[test]
fn doctor_fix_moves_literal_credentials_into_variables() {
    let sb = Sandbox::new();
    let url = chain_id_rpc(1);
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
fn config_path_lists_every_location() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(format!("{}\n", sb.config_path().display()));
    sb.cmd()
        .args(["config", "path", "--all"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("secrets")
                .and(predicate::str::contains("runtime"))
                .and(predicate::str::contains("serve.log"))
                .and(predicate::str::contains("forks/<name>.log")),
        );
}

#[test]
fn edit_validates_before_saving() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let before = sb.config();
    // A non-interactive "editor" that breaks a reference.
    sb.cmd()
        .env(
            "EDITOR",
            "sed -i s/rpc\\ =\\ \\\"local\\\"/rpc\\ =\\ \\\"nope\\\"/",
        )
        .args(["edit"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nope"));
    assert_eq!(sb.config(), before);
    sb.cmd()
        .env("EDITOR", "sed -i s/id\\ =\\ 1$/id\\ =\\ 5/")
        .args(["edit"])
        .assert()
        .success();
    assert!(sb.config().contains("id = 5"));
}

#[test]
fn edit_chain_replaces_one_chain() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    // A chain ID change is named in the report.
    sb.cmd()
        .env("EDITOR", "sed -i s/id\\ =\\ 8453/id\\ =\\ 84532/")
        .args(["edit", "ba"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Updated chain 'base'\n  chain ID: 8453 → 84532 (its endpoints were not re-checked)\n",
        ));
    assert!(sb.config().contains("id = 84532"));
    assert!(sb.config().contains("[chains.eth]"));
}

// ------------------------------------------------- regressions and v1 names

/// An infura-style URL whose key `chain add`/`rpc add` move to secrets.toml.
const INFURA_URL: &str = "https://mainnet.infura.invalid/v3/0123456789abcdef0123456789abcdef";
const INFURA_KEY: &str = "0123456789abcdef0123456789abcdef";

/// A mock chain: `eth_chainId` is `chain_id`, the head is `head`, and every
/// answer takes `delay`.
fn chain_rpc(chain_id: u64, head: u64, delay: std::time::Duration) -> MockRpc {
    MockRpc::start(move |request| {
        std::thread::sleep(delay);
        Ok(match request.method.as_str() {
            "eth_chainId" => serde_json::json!(format!("{chain_id:#x}")),
            "eth_blockNumber" => serde_json::json!(format!("{head:#x}")),
            _ => serde_json::Value::Null,
        })
    })
}

/// A fresh chainlist cache with one entry.
fn seed_chainlist(sb: &Sandbox, name: &str, chain_id: u64, rpcs: &[&str]) {
    let path = sb.cache_file("chains.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let entry = serde_json::json!([{
        "name": name, "chainId": chain_id, "shortName": name.to_lowercase(), "rpc": rpcs,
        "nativeCurrency": {"name": "Ether", "symbol": "ETH", "decimals": 18}
    }]);
    fs::write(path, entry.to_string()).unwrap();
}

/// `(active, [(name, url)])` of a chain, URLs revealed.
fn endpoints(sb: &Sandbox, chain: &str) -> (String, Vec<(String, String)>) {
    let shown: serde_json::Value =
        serde_json::from_str(&sb.ok(&["chain", "show", chain, "--json", "--reveal"])).unwrap();
    let list = shown["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap().to_string(),
                e["url"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    (shown["rpc"].as_str().unwrap().to_string(), list)
}

#[test]
fn chain_add_from_chainlist_skips_invalid_urls_and_picks_a_live_one() {
    let sb = Sandbox::new();
    let live = chain_rpc(8453, 100, std::time::Duration::ZERO);
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
    let out = sb.run(&["chain", "add", "base"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "Added chain 'base' (8453) with 2 endpoint(s); active: local\n"
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
fn chain_add_and_rpc_add_move_url_keys_into_secrets() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["chain", "add", "1", "--url", INFURA_URL, "--no-check"])
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
            "rpc",
            "add",
            "eth",
            &other,
            "--name",
            "second",
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
fn rpc_bench_fix_activates_the_best_and_yes_writes_failover() {
    let sb = Sandbox::new();
    let fast = chain_rpc(1, 1_000, std::time::Duration::ZERO);
    let slow = chain_rpc(1, 1_000, std::time::Duration::from_millis(300));
    let config = format!(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"dead\"\n[chains.eth.rpcs]\ndead = \"http://127.0.0.1:1\"\nfast = \"{}\"\nslow = \"{}\"\n",
        fast.url(),
        slow.url()
    );
    sb.write_config(&config);

    // Report only.
    let report = sb.ok(&["rpc", "bench", "eth"]);
    assert!(
        report.contains("best: fast (run with --fix to activate)"),
        "{report}"
    );
    assert!(sb.config().contains("rpc = \"dead\""));

    // --fix off a TTY activates the best but leaves failover unwritten.
    let fixed = sb.ok(&["rpc", "bench", "eth", "--fix"]);
    assert!(
        fixed.contains("eth: active → fast; proposed failover: slow"),
        "{fixed}"
    );
    assert!(fixed.contains("failover lists not written"), "{fixed}");
    assert!(sb.config().contains("rpc = \"fast\""));
    assert!(!sb.config().contains("failover"));

    // --yes writes it.
    sb.write_config(&config);
    sb.ok(&["rpc", "bench", "--fix", "--yes"]);
    let written = sb.config();
    assert!(written.contains("rpc = \"fast\""), "{written}");
    assert!(written.contains("failover = [\"slow\"]"), "{written}");

    // --yes needs --fix.
    sb.cmd().args(["rpc", "bench", "--yes"]).assert().code(2);
}

/// Every endpoint down: no latencies, nothing ranked, `--fix` says so, and
/// the exit code tells scripts.
#[test]
fn rpc_bench_with_every_endpoint_down_fails() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.uni]\nid = 130\nrpc = \"a\"\n[chains.uni.rpcs]\na = \"http://127.0.0.1:1\"\nb = \"http://127.0.0.1:1/b\"\n",
    );
    for fix in [false, true] {
        let args: &[&str] = if fix {
            &["rpc", "bench", "uni", "--fix"]
        } else {
            &["rpc", "bench", "uni"]
        };
        let out = sb.run(args);
        let text = stdout(&out);
        assert_eq!(out.status.code(), Some(1), "{text}");
        assert!(text.contains("(0 of 2 answered)"), "{text}");
        assert!(text.contains("✗ connection failed"), "{text}");
        assert!(!text.contains("ms"), "{text}");
        let lines: Vec<&str> = text.lines().filter(|l| l.contains("✗")).collect();
        assert!(
            lines[0].contains(" a ") && lines[1].contains(" b "),
            "{text}"
        );
        assert!(text.contains("no healthy endpoint on uni"), "{text}");
        assert_eq!(text.contains("nothing to fix"), fix, "{text}");
    }
    assert!(sb.config().contains("rpc = \"a\""));
}

#[test]
fn rpc_discover_lists_off_a_tty_and_yes_adds_the_live_ones() {
    let sb = Sandbox::new();
    let live = chain_rpc(1, 1_000, std::time::Duration::ZERO);
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
    let listed = sb.run(&["rpc", "discover", "eth"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let text = stdout(&listed);
    assert!(text.contains(&live.url()), "{text}");
    assert!(text.contains("chainz rpc discover eth --yes"), "{text}");
    assert!(
        stderr(&listed).contains("skipping [REDACTED URL]: invalid endpoint: invalid URL"),
        "{}",
        stderr(&listed)
    );
    assert_eq!(sb.config(), before);

    // --yes adds every endpoint that answered, and only those.
    let added = sb.ok(&["rpc", "discover", "eth", "--yes"]);
    assert_eq!(added, "Added 1 endpoint(s) to 'eth': local\n");
    let (_, list) = endpoints(&sb, "eth");
    assert_eq!(
        list,
        [
            ("known".to_string(), "http://127.0.0.1:9/".to_string()),
            ("local".to_string(), live.url())
        ]
    );
}

/// A keystore path that doesn't exist, or a keystore with no address
/// field and no `--address` (`cast wallet new` writes none), adds nothing.
#[test]
fn keystore_signers_need_a_file_and_an_address() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let before = sb.config();
    let missing = sb.home().join("nope.json");
    let out = sb
        .process()
        .args([
            "signer",
            "add",
            "ks",
            "--backend",
            "keystore",
            "--address",
            TEST_ADDRESS_2,
            "--path",
        ])
        .arg(&missing)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("keystore not found"),
        "{}",
        stderr(&out)
    );
    let out = sb
        .process()
        .args(["signer", "add", "ks", "--path"])
        .arg(keystore_file(&sb))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("has no address field"), "{err}");
    assert_eq!(err.matches("--address").count(), 1, "{err}");
    assert!(err.contains("cast wallet address --keystore"), "{err}");
    assert_eq!(sb.config(), before);
}

/// An (empty) keystore file in the sandbox.
fn keystore_file(sb: &Sandbox) -> std::path::PathBuf {
    let path = sb.home().join("ks.json");
    fs::write(&path, "{}").unwrap();
    path
}

#[test]
fn signer_migrate_off_a_tty_changes_nothing_and_reports_each_failure() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let unchanged = |sb: &Sandbox| (sb.config(), sb.secrets());
    let before = unchanged(&sb);

    // The keyring is disabled in tests; encrypted (the default without a
    // keyring) needs a prompt.
    let out = sb.run(&["signer", "migrate", "default", "--to", "keyring"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("keyring is unavailable"),
        "{}",
        stderr(&out)
    );
    let out = sb.run(&["signer", "migrate", "default"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("interactive password prompt"),
        "{}",
        stderr(&out)
    );
    assert_eq!(unchanged(&sb), before);

    sb.cmd()
        .args(["key", "add", "second", "--type", "private-key", "--stdin"])
        .write_stdin(TEST_KEY_2)
        .assert()
        .success();
    let before = unchanged(&sb);
    let out = sb.run(&["signer", "migrate"]);
    assert!(
        stderr(&out).contains("2 plaintext signers (default, second); pass a name or --all"),
        "{}",
        stderr(&out)
    );
    // --all tries every one and names each failure.
    let out = sb.run(&["signer", "migrate", "--all", "--to", "encrypted"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("✗ 'default': ") && err.contains("✗ 'second': "),
        "{err}"
    );
    assert!(
        err.contains("2 of 2 signers not moved: default, second"),
        "{err}"
    );
    assert_eq!(unchanged(&sb), before);

    // References are never moved.
    sb.cmd()
        .args([
            "signer",
            "add",
            "ks",
            "--backend",
            "keystore",
            "--address",
            TEST_ADDRESS_2,
        ])
        .arg("--path")
        .arg(keystore_file(&sb))
        .assert()
        .success();
    let out = sb.run(&["signer", "migrate", "ks"]);
    assert!(
        stderr(&out).contains("chainz does not move it"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn signer_rm_needs_yes_off_a_tty_and_names_every_flag_at_once() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    let out = sb.run(&["signer", "rm", "default"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("unassigns it from eth, base and deletes its key in secrets.toml"),
        "{err}"
    );
    assert!(err.contains("pass --unassign --yes"), "{err}");
    let out = sb.run(&["signer", "rm", "default", "--unassign"]);
    assert!(stderr(&out).contains("; pass --yes"), "{}", stderr(&out));
    assert!(sb.secrets().contains(TEST_KEY), "nothing deleted");
    assert!(sb.config().contains("[signers.default]"));

    // A keystore reference deletes no key material, but is a removal too.
    sb.cmd()
        .args([
            "signer",
            "add",
            "ks",
            "--backend",
            "keystore",
            "--address",
            TEST_ADDRESS_2,
        ])
        .arg("--path")
        .arg(keystore_file(&sb))
        .assert()
        .success();
    let out = sb.run(&["signer", "remove", "ks"]);
    assert!(
        stderr(&out).contains("(keystore kept); pass --yes"),
        "{}",
        stderr(&out)
    );
    sb.ok(&["signer", "remove", "ks", "--yes"]);
    // Unassigning says which chains are left without a signer.
    let out = sb.run(&["key", "rm", "default", "--unassign", "-y"]);
    assert!(out.status.success());
    assert!(
        stderr(&out).contains(
            "eth, base have no signer ('default' was removed); attach one with `chainz chain set <chain> --signer <name>`"
        ),
        "{}",
        stderr(&out)
    );
    assert!(!sb.secrets().contains(TEST_KEY));
}

#[test]
fn secrets_given_as_arguments_are_warned_about() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["var", "set", "TOKEN", "abc"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "visible in shell history and `ps`; prefer --stdin",
        ));
    sb.cmd()
        .args([
            "signer",
            "add",
            "argkey",
            "--key",
            TEST_KEY_2,
            "--backend",
            "plaintext",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "a private key given as an argument",
        ));
    // --stdin is quiet.
    sb.cmd()
        .args(["var", "set", "OTHER", "--stdin"])
        .write_stdin("abc")
        .assert()
        .success()
        .stderr("");
}

#[test]
fn v1_command_names_still_work() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    assert!(sb.ok(&["list"]).contains("eth"));
    assert!(sb.ok(&["chain", "list"]).contains("eth"));
    assert!(sb.ok(&["rpc", "list"]).contains("local"));
    assert!(sb.ok(&["key", "list"]).contains("default"));
    sb.cmd()
        .args(["var", "set", "TOKEN", "--stdin"])
        .write_stdin("hunter2")
        .assert()
        .success();
    assert!(sb.ok(&["var", "list", "--show"]).contains("hunter2"));
    assert_eq!(sb.ok(&["var", "get", "TOKEN", "--show"]), "hunter2\n");
    sb.ok(&["var", "remove", "TOKEN", "-y"]);
    assert!(!sb.ok(&["var", "list"]).contains("TOKEN"));
    sb.cmd()
        .args([
            "key",
            "add",
            "one",
            "--type",
            "one-password",
            "--vault",
            "v",
            "--item",
            "i",
            "--address",
            TEST_ADDRESS_2,
        ])
        .assert()
        .success();
    assert!(
        sb.config().contains("backend = \"1password\""),
        "{}",
        sb.config()
    );
    sb.ok(&["key", "remove", "one", "-y"]);
    sb.ok(&["chain", "remove", "eth", "-y"]);
}

#[test]
fn exec_exit_codes_for_missing_and_unrunnable_commands() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["exec", "eth", "--", "chainz-no-such-command"])
        .assert()
        .code(127)
        .stderr(predicate::str::contains(
            "failed to run 'chainz-no-such-command': command not found",
        ));
    let script = sb.home().join("not-executable.sh");
    fs::write(&script, "#!/bin/sh\necho ran\n").unwrap();
    set_mode(&script, 0o644);
    sb.cmd()
        .args(["exec", "eth", "--"])
        .arg(&script)
        .assert()
        .code(126)
        .stdout("")
        .stderr(predicate::str::contains("permission denied"));
}

#[test]
fn v1_explorer_tokens_expand_with_a_deprecation_warning() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[explorers.scan]\napi = \"https://scan.invalid/api\"\nkey = \"${SCAN_KEY}\"\n[chains.eth]\nid = 1\nrpc = \"a\"\nexplorer = \"scan\"\n[chains.eth.rpcs]\na = \"http://127.0.0.1:1\"\n",
    );
    sb.write_secrets("[vars]\nSCAN_KEY = \"scankey\"\n");
    sb.cmd()
        .args([
            "exec",
            "eth",
            "--",
            "echo",
            "@verification_url",
            "@verifier_api_key",
        ])
        .assert()
        .success()
        .stdout("https://scan.invalid/api scankey\n")
        .stderr(
            predicate::str::contains("@verification_url is deprecated")
                .and(predicate::str::contains("@verifier_api_key is deprecated")),
        );
}

#[test]
fn expose_key_explain_never_unlocks() {
    let sb = Sandbox::new();
    // An encrypted signer: unlocking would need a password prompt, which
    // fails off a TTY. --explain must not try.
    sb.write_config(&format!(
        "version = 2\n[signers.vault]\nbackend = \"encrypted\"\naddress = \"{TEST_ADDRESS}\"\n[chains.eth]\nid = 1\nsigner = \"vault\"\nrpc = \"a\"\n[chains.eth.rpcs]\na = \"http://127.0.0.1:1\"\n"
    ));
    sb.write_secrets("[signers.vault]\nciphertext = \"not-real\"\n");
    let explained = sb.ok(&["exec", "eth", "--expose-key", "--explain", "--", "true"]);
    assert!(explained.contains("RAW_PRIVATE_KEY"), "{explained}");
    sb.cmd()
        .args(["exec", "eth", "--expose-key", "--", "true"])
        .assert()
        .failure();
}

// ------------------------------------------------------- first run and `chain set`

#[test]
fn first_run_attaches_the_signer_and_the_explorer() {
    let sb = Sandbox::new();
    // No chains: the quick start, signer first; `init` off a terminal too.
    let text = sb.ok(&[]);
    assert!(text.contains("Quick start:"), "{text}");
    let signer = text.find("signer add").unwrap();
    assert!(signer < text.find("chain add").unwrap(), "{text}");
    assert_eq!(
        sb.ok(&["init"]),
        text.replacen("chainz: no chains configured yet.\n", "", 1)
    );

    // A chain with no signer, then a first signer: it attaches.
    let url = chain_id_rpc(8453);
    sb.ok(&["chain", "add", "8453", "--url", &url]);
    assert!(!sb.config().contains("signer ="), "{}", sb.config());
    let out = sb
        .cmd()
        .args([
            "signer",
            "add",
            "deployer",
            "--stdin",
            "--backend",
            "plaintext",
        ])
        .write_stdin(TEST_KEY)
        .assert()
        .success();
    let text = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(text.contains("Attached 'deployer' to base"), "{text}");
    assert!(sb.config().contains("signer = \"deployer\""));

    // The next chain takes the only signer, and etherscan once it exists.
    sb.cmd()
        .args(["chain", "set", "base", "--explorer", "etherscan"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created [explorers.etherscan]"));
    let url = chain_id_rpc(10);
    let text = sb.ok(&["chain", "add", "10", "--url", &url]);
    assert!(text.contains("signer: deployer"), "{text}");
    assert!(text.contains("explorer: etherscan"), "{text}");
    let url = chain_id_rpc(42161);
    let text = sb.ok(&["chain", "add", "42161", "--url", &url, "--no-signer"]);
    assert!(!text.contains("signer:"), "{text}");
    let show = sb.ok(&["chain", "show", "arb", "--json"]);
    assert!(show.contains("\"signer\": null"), "{show}");

    // A second signer attaches nowhere.
    let text = sb
        .cmd()
        .args(["signer", "add", "ops", "--stdin", "--backend", "plaintext"])
        .write_stdin(TEST_KEY.replace("ac09", "bc09"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(!String::from_utf8_lossy(&text).contains("Attached"));
}

#[test]
fn chain_add_without_a_chain_off_a_tty_is_a_usage_error_before_any_fetch() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["chain", "add"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Usage: chainz chain add"))
        .stderr(predicate::str::contains("chainlist").not());
    assert!(!sb.cache_file("chains.json").exists());
}

#[test]
fn a_missing_chain_off_a_tty_is_a_usage_error() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    for (args, usage) in [
        (&["exec", "--", "true"][..], "Usage: chainz exec"),
        (&["shell"], "Usage: chainz shell"),
        (&["env"], "Usage: chainz env"),
        (&["use"], "Usage: chainz use"),
    ] {
        sb.cmd()
            .args(args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("no chain given: pass "))
            .stderr(predicate::str::contains(usage));
    }
}

#[test]
fn other_missing_arguments_are_usage_errors() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    for (args, message, usage) in [
        (
            &["fork", "stop"][..],
            "which fork?",
            "Usage: chainz fork stop",
        ),
        (
            &["var", "set", "TOKEN"],
            "no value given",
            "Usage: chainz var set",
        ),
        (
            &["signer", "add", "hot"],
            "no key given",
            "Usage: chainz signer add",
        ),
        (
            &["signer", "add", "ks", "--backend", "keystore"],
            "needs --path",
            "Usage: chainz signer add",
        ),
        (
            &["signer", "add", "op", "--backend", "1password"],
            "needs --vault and --item",
            "Usage: chainz signer add",
        ),
    ] {
        sb.cmd()
            .args(args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains(message))
            .stderr(predicate::str::contains(usage));
    }
    for signer in ["hot", "ks", "op"] {
        let table = format!("[signers.{signer}]");
        assert!(!sb.config().contains(&table), "{}", sb.config());
    }
}

#[test]
fn chain_set_off_a_tty_needs_a_chain_and_flags() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    for (args, usage) in [
        (&["chain", "set", "eth"][..], "Usage: chainz chain set"),
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
        (
            &["chain", "set", "--api-key-stdin"][..],
            "Usage: chainz chain set",
        ),
        (
            &["chain", "set", "nope", "--api-key-stdin"],
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

#[test]
fn chain_set_applies_every_flag_in_one_write() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let url = chain_id_rpc(1);
    let text = sb
        .cmd()
        .args([
            "chain",
            "set",
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
    let show = sb.ok(&["chain", "show", "eth", "--json"]);
    let show: serde_json::Value = serde_json::from_str(&show).unwrap();
    assert_eq!(show["failover"], serde_json::json!(["local"]));
    assert_eq!(show["aliases"], serde_json::json!(["l1"]));
    assert_eq!(show["signer"], serde_json::Value::Null);
    assert_ne!(show["rpc"], "local");
    // A new URL with the wrong chain ID is refused, and nothing is written.
    let before = sb.config();
    let wrong = chain_id_rpc(10);
    sb.cmd()
        .args(["chain", "set", "eth", "--rpc", &wrong, "--name", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RPC check failed"));
    assert_eq!(sb.config(), before);
    // An API key needs an explorer.
    sb.cmd()
        .args(["chain", "set", "eth", "--api-key-stdin"])
        .write_stdin("KEY")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no explorer"));
    sb.cmd()
        .args([
            "chain",
            "set",
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
fn removed_commands_are_gone() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    for args in [
        &["chain", "rename", "eth", "x"][..],
        &["chain", "alias", "eth", "x"],
        &["chain", "edit", "eth"],
        &["rpc", "use", "eth", "local"],
        &["config", "edit"],
        &["chain", "add", "1", "--rpc", "http://x.invalid"],
        &["rpc", "add", "eth", "http://x.invalid", "--use"],
        &["signer", "rm", "default", "--force"],
    ] {
        sb.cmd().args(args).assert().code(2);
    }
}
