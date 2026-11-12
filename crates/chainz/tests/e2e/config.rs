//! The config files themselves: permissions, a corrupt or unknown key,
//! concurrent writers.

use crate::common::*;

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
