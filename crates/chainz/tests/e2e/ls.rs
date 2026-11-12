//! `ls` and `ls CHAIN` (the exact output is pinned in `golden.rs`).

use crate::common::*;

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
fn ls_redacts_credentials_unless_revealed() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"lit\"\n[chains.eth.rpcs]\nlit = \"https://x.invalid/v3/abcdef0123456789abcdef\"\nhdr = { url = \"https://y.invalid\", headers = { \"x-api-key\" = \"headersecret999\" } }\n",
    );
    for args in [
        vec!["ls"],
        vec!["ls", "--json"],
        vec!["ls", "eth"],
        vec!["ls", "eth", "--json"],
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
        .args(["ls", "eth", "--reveal"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("abcdef0123456789abcdef")
                .and(predicate::str::contains("headersecret999")),
        );
}

/// `ls CHAIN` words recorded health the way `doctor` does: a lagging
/// endpoint says how far behind, a failed one what went wrong.
#[test]
fn ls_chain_shows_lag_and_failures_like_doctor() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = \"http://127.0.0.1:1\"\nb = \"http://127.0.0.1:2\"\nc = \"http://127.0.0.1:3\"\n",
    );
    std::fs::create_dir_all(sb.state_file("health.json").parent().unwrap()).unwrap();
    std::fs::write(
        sb.state_file("health.json"),
        r#"{"version":1,"endpoints":[
 {"chain_id":1,"endpoint":"a","samples":[{"at_ms":1,"latency_ms":42,"head":1000,"error":null}]},
 {"chain_id":1,"endpoint":"b","samples":[{"at_ms":1,"latency_ms":7,"head":970,"error":null}]},
 {"chain_id":1,"endpoint":"c","samples":[{"at_ms":1,"latency_ms":5000,"head":null,"error":"timeout"}]}
]}"#,
    )
    .unwrap();
    let out = sb.ok(&["ls", "eth"]);
    let row = |name: &str| {
        out.lines()
            .find(|l| l.split_whitespace().any(|w| w == name))
            .unwrap_or_else(|| panic!("no row {name}: {out}"))
            .to_string()
    };
    assert!(row("a").contains("42ms "), "{out}");
    assert!(row("b").contains("7ms lag 30"), "{out}");
    assert!(row("c").contains("✗ timed out"), "{out}");
    // The JSON keeps the stable class.
    let json = sb.ok(&["ls", "eth", "--json"]);
    assert!(json.contains(r#""error": "timeout""#), "{json}");
}
