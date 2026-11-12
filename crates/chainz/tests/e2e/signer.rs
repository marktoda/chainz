//! `signer add`, `signer rm`, `signer migrate`.

use crate::common::*;

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
fn a_keystore_signer_is_a_reference() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let ks = keystore_file(&sb);
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

/// A keystore path that doesn't exist, or a keystore with no address
/// field and no `--address` (`cast wallet new` writes none), adds nothing.
#[test]
fn signer_add_keystore_needs_a_file_and_an_address() {
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
        .args([
            "signer",
            "add",
            "second",
            "--backend",
            "plaintext",
            "--stdin",
        ])
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
    let out = sb.run(&["signer", "rm", "ks"]);
    assert!(
        stderr(&out).contains("(keystore kept); pass --yes"),
        "{}",
        stderr(&out)
    );
    sb.ok(&["signer", "rm", "ks", "--yes"]);
    // Unassigning says which chains are left without a signer.
    let out = sb.run(&["signer", "rm", "default", "--unassign", "-y"]);
    assert!(out.status.success());
    assert!(
        stderr(&out).contains(
            "eth, base have no signer ('default' was removed); attach one with `chainz update CHAIN --signer NAME`"
        ),
        "{}",
        stderr(&out)
    );
    assert!(!sb.secrets().contains(TEST_KEY));
    assert!(!sb.config().contains("[signers.default]"));
    assert!(!sb.config().contains("signer = \"default\""));
}
