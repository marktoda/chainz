//! `use`: the default chain.

use crate::common::*;

#[test]
fn use_sets_the_default_and_exec_says_when_it_falls_back() {
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
    // No chain: show the default, never a picker or an error.
    sb.cmd()
        .arg("use")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("default: base\n"))
        .stderr("");
    sb.cmd().args(["use", "--clear"]).assert().success();
    assert!(!sb.config().contains("default ="));
    sb.cmd()
        .arg("use")
        .assert()
        .success()
        .stdout("default: none\nSet one with `chainz use CHAIN`\n");
}

#[test]
fn use_help_says_when_exec_falls_back_to_the_default() {
    let sb = Sandbox::new();
    sb.cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Set the default chain, or show it; exec says on stderr when it falls back to it",
        ));
}
