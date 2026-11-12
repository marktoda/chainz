//! `exec` and `env`: the exported context, @tokens, chain resolution,
//! exit codes. (`@label` tokens are in `label.rs`, gateway routing in
//! `serve.rs`.)

use crate::common::*;

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
fn exec_rpc_takes_a_url_for_one_run_without_saving_it() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let config = sb.config();
    sb.cmd()
        .args(["exec", "1", "--rpc", "http://127.0.0.1:8545", "--"])
        .args(["sh", "-c", "echo $ETH_RPC_URL \"$1\"", "sh", "@rpc"])
        .assert()
        .success()
        .stdout("http://127.0.0.1:8545 http://127.0.0.1:8545\n");
    assert_eq!(sb.config(), config);
    sb.cmd()
        .args(["exec", "eth", "--rpc", "nope", "--", "true"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "chain 'eth' has no endpoint 'nope'",
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
fn exec_expose_key_explain_never_unlocks() {
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
fn exec_with_an_unknown_chain_fails() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["exec", "ethh"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ethh"));
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
fn exec_finds_the_configured_short_name_by_its_long_name() {
    let sb = Sandbox::new();
    sb.seed(&[("arb", 42161), ("eth", 1)]);
    for (query, chain) in [("arbitrum", "arb"), ("Ethereum", "eth"), ("mainnet", "eth")] {
        sb.cmd()
            .args(["exec", query, "--", "sh", "-c", "echo $CHAINZ_CHAIN"])
            .assert()
            .success()
            .stdout(format!("{chain}\n"));
    }
    sb.cmd()
        .args(["exec", "optimism", "--", "true"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("chain 'optimism' not found"));
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
            "update",
            "eth",
            "--add-rpc",
            "two=http://127.0.0.2:1",
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
fn exec_on_a_chain_without_a_signer_runs_until_one_is_needed() {
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

#[test]
fn exec_without_a_command_opens_a_subshell_only_on_a_terminal() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    // Off a terminal, stdin was never meant for a shell: a usage error,
    // and the shell never reads it.
    for exec in ["exec", "x"] {
        sb.cmd()
            .env("SHELL", "sh")
            .args([exec, "eth"])
            .write_stdin("echo ran\n")
            .assert()
            .code(2)
            .stdout("")
            .stderr(predicate::str::contains(
                "no command: pass it after `--` (a subshell opens only on a terminal)",
            ));
    }
    // On a terminal (util-linux `script` gives it one): the subshell, with
    // its exit code passed through.
    let has_script = std::process::Command::new("script")
        .arg("--version")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("util-linux"));
    if has_script {
        let bin = assert_cmd::cargo::cargo_bin!("chainz");
        let mut script = std::process::Command::new("script");
        sb.apply(&mut script);
        let out = assert_cmd::Command::from_std(script)
            .env("SHELL", "sh")
            .args(["-qec", &format!("{} exec eth", bin.display()), "/dev/null"])
            .write_stdin("echo id=$CHAIN_ID; exit 3\n")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(3), "{text}");
        assert!(text.contains("id=1"), "{text}");
        assert!(text.contains("entering eth shell"), "{text}");
    }
    sb.cmd()
        .env("SHELL", "sh")
        .args(["exec", "eth", "--explain"])
        .assert()
        .success()
        .stdout(predicate::str::contains("argv      sh (subshell)"));
}

#[test]
fn exec_and_env_without_a_chain_off_a_tty_are_usage_errors() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    for (args, usage) in [
        (&["exec", "--", "true"][..], "Usage: chainz exec"),
        (&["exec"], "Usage: chainz exec"),
        (&["env"], "Usage: chainz env"),
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
