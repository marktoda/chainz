//! `var set`, `var ls`, `var rm`.

use crate::common::*;

#[test]
fn var_set_ls_rm_roundtrip() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["var", "set", "API_KEY", "supersecret123"])
        .assert()
        .success();
    sb.cmd()
        .args(["var", "set", "OTHER", "othervalue"])
        .assert()
        .success();
    // Named: only those rows.
    sb.cmd()
        .args(["var", "ls", "API_KEY"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("REDACTED")
                .and(predicate::str::contains("supersecret").not())
                .and(predicate::str::contains("OTHER").not()),
        );
    sb.cmd()
        .args(["var", "ls", "API_KEY", "--reveal"])
        .assert()
        .success()
        .stdout(predicate::str::contains("supersecret123"));
    sb.cmd()
        .args(["var", "ls", "API_KEY", "NOPE"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("variable 'NOPE' is not set"));
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
    sb.cmd().args(["var", "ls", "API_KEY"]).assert().failure();
    sb.cmd().args(["var", "get", "OTHER"]).assert().code(2);
}

#[test]
fn var_stdin_preserves_intentional_whitespace() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["var", "set", "SPACED", "--stdin"])
        .write_stdin("  value \n")
        .assert()
        .success();
    let out = sb.ok(&["var", "ls", "SPACED", "--reveal", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["value"], "  value ");
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
