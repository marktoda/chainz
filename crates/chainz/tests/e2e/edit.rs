//! `edit`: config.toml or one chain's table in $EDITOR.

use crate::common::*;

#[test]
fn edit_path_prints_the_config_path() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["edit", "--path"])
        .assert()
        .success()
        .stdout(format!("{}\n", sb.config_path().display()));
    // Only the whole file has a path to print.
    sb.cmd().args(["edit", "eth", "--path"]).assert().code(2);
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
