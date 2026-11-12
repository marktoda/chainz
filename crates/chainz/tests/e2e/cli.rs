//! Behaviour every command shares: error and usage-error shapes, removed
//! spellings, secrets passed as arguments, a reader that goes away.

use crate::common::*;

#[test]
fn errors_use_the_same_prefix_as_clap() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    // An error from a handler (anyhow) and a usage error (clap).
    let handler = sb.run(&["ls", "nope"]);
    assert_eq!(handler.status.code(), Some(1));
    assert!(
        stderr(&handler).starts_with("error: "),
        "{}",
        stderr(&handler)
    );
    let usage = sb.run(&["ls", "--nope"]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(stderr(&usage).starts_with("error: "), "{}", stderr(&usage));
}

/// `chainz ls eth | head` and friends: a reader that went away ends the
/// command quietly with exit 0, never a panic.
#[test]
fn a_closed_stdout_exits_quietly() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453), ("op", 10)]);
    sb.write_secrets("[vars]\nTOKEN = \"t\"\n");
    for args in [
        &["ls", "eth"][..],
        &["var", "ls"],
        &["label", "ls"],
        &["label", "ls", "--builtin"],
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

/// Removed and v1 spellings are unknown: clap's usage error (exit 2),
/// never a hidden alias.
#[test]
fn removed_and_v1_spellings_are_usage_errors() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    for argv in [
        &["list"][..],
        &["key", "ls"],
        &["signer", "list"],
        &["signer", "remove", "default", "-y"],
        &["var", "list"],
        &["var", "remove", "X", "-y"],
        &["var", "ls", "--show"],
        &["signer", "add", "x", "--type", "keystore"],
        &["signer", "add", "x", "--backend", "one-password"],
        &["signer", "add", "x", "--backend", "private-key", "--stdin"],
        &["chain", "ls"],
        &["chain", "show", "eth"],
        &["chain", "set", "eth", "--no-signer"],
        &["chain", "add", "1"],
        &["chain", "rm", "eth", "-y"],
        &["rpc", "ls", "eth"],
        &["rpc", "add", "eth", "http://x.invalid", "--no-check"],
        &["rpc", "rm", "eth", "local", "-y"],
        &["rpc", "discover", "eth"],
        &["addr", "ls"],
        &["addr", "find", TEST_ADDRESS],
        &["label", "find", TEST_ADDRESS],
        &["label", TEST_ADDRESS, "--chain", "eth"],
        &["config", "path"],
        &["config", "migrate"],
        &["config", "edit"],
        &["shell", "eth"],
        &["chain", "rename", "eth", "x"],
        &["chain", "alias", "eth", "x"],
        &["chain", "edit", "eth"],
        &["rpc", "use", "eth", "local"],
        &["rpc", "bench", "eth"],
        &["rpc", "add", "eth", "http://x.invalid", "--use"],
        &["add", "1", "--rpc", "http://x.invalid"],
        &["signer", "rm", "default", "--force"],
    ] {
        let out = sb.run(argv);
        assert_eq!(out.status.code(), Some(2), "{argv:?}: {}", stderr(&out));
    }
}
