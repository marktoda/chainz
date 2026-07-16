//! End-to-end CLI tests. Each test gets its own temp HOME so the real
//! `~/.chainz.json` is never touched and tests can run in parallel.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Well-known anvil test key #0
const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const TEST_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

fn chainz(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("chainz").unwrap();
    cmd.env("HOME", home);
    cmd
}

fn config_path(home: &Path) -> std::path::PathBuf {
    home.join(".chainz.json")
}

/// Seed a config with one key and the given chains, bypassing the CLI
/// (non-interactive `add` requires a live RPC to health-check against).
fn seed_config(home: &Path, chains: &[(&str, u64)]) {
    let chains: Vec<serde_json::Value> = chains
        .iter()
        .map(|(name, id)| {
            serde_json::json!({
                "name": name,
                "chain_id": id,
                "rpc_urls": ["http://localhost:1"],
                "selected_rpc": "http://localhost:1",
                "verification_api_key": null,
                "verification_url": null,
                "key_name": "default",
            })
        })
        .collect();
    let config = serde_json::json!({
        "chains": chains,
        "variables": {},
        "keys": {
            "default": { "name": "default", "type": "PrivateKey", "value": TEST_KEY }
        },
    });
    fs::write(config_path(home), config.to_string()).unwrap();
}

#[test]
fn version_flag_works() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("chainz "));
}

#[test]
fn list_empty_config_shows_hint() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("No chains configured"));
}

#[test]
fn var_set_get_list_rm_roundtrip() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .args(["var", "set", "MY_KEY", "my_value"])
        .assert()
        .success();
    chainz(home.path())
        .args(["var", "get", "MY_KEY"])
        .assert()
        .success()
        .stdout(predicate::str::contains("MY_KEY = my_value"));
    chainz(home.path())
        .args(["var", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("MY_KEY"));
    chainz(home.path())
        .args(["var", "rm", "MY_KEY"])
        .assert()
        .success();
    chainz(home.path())
        .args(["var", "get", "MY_KEY"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not found"));
}

#[test]
fn key_add_is_noninteractive_with_key_flag() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .args(["key", "add", "default", "--key", TEST_KEY])
        .assert()
        .success()
        .stdout(predicate::str::contains("Added key 'default'"));
    chainz(home.path())
        .args(["key", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(TEST_ADDRESS));
}

#[test]
fn key_add_rejects_invalid_key() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .args(["key", "add", "bad", "--key", "not-a-key"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid private key"));
}

#[test]
fn key_remove_works() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .args(["key", "add", "temp", "--key", TEST_KEY])
        .assert()
        .success();
    chainz(home.path())
        .args(["key", "remove", "temp"])
        .assert()
        .success();
    chainz(home.path())
        .args(["key", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No stored keys"));
}

#[cfg(unix)]
#[test]
fn config_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .args(["var", "set", "A", "b"])
        .assert()
        .success();
    let mode = fs::metadata(config_path(home.path()))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "config must not be group/world readable");
}

#[cfg(unix)]
#[test]
fn loose_config_permissions_are_tightened_on_load() {
    use std::os::unix::fs::PermissionsExt;
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[]);
    fs::set_permissions(config_path(home.path()), fs::Permissions::from_mode(0o644)).unwrap();

    chainz(home.path()).args(["key", "list"]).assert().success();

    let mode = fs::metadata(config_path(home.path()))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn corrupt_config_errors_and_is_not_overwritten() {
    let home = TempDir::new().unwrap();
    fs::write(config_path(home.path()), "{not valid json").unwrap();

    // Read commands fail loudly
    chainz(home.path())
        .arg("list")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to parse config"));

    // Write commands fail too, leaving the file untouched for manual repair
    chainz(home.path())
        .args(["var", "set", "A", "b"])
        .assert()
        .failure();
    let content = fs::read_to_string(config_path(home.path())).unwrap();
    assert_eq!(
        content, "{not valid json",
        "corrupt config must be preserved"
    );
}

#[test]
fn remove_chain_by_name_and_id() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("ethereum", 1), ("optimism", 10)]);

    chainz(home.path())
        .args(["remove", "ethereum"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed chain 'ethereum'"));

    // rm alias, by chain ID
    chainz(home.path())
        .args(["rm", "10"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed chain 'optimism'"));

    chainz(home.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("No chains configured"));
}

#[test]
fn remove_unknown_chain_fails() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("ethereum", 1)]);
    chainz(home.path())
        .args(["remove", "nonexistent"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn exec_expands_env_and_tokens() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);

    // env vars are set for the child process; @-tokens expand in args.
    // HTTP providers connect lazily, so no live RPC is needed for `echo`.
    chainz(home.path())
        .args([
            "exec",
            "testchain",
            "--",
            "sh",
            "-c",
            "echo rpc=$ETH_RPC_URL id=$CHAIN_ID name=$CHAIN_NAME",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "rpc=http://localhost:1 id=31337 name=testchain",
        ));

    chainz(home.path())
        .args(["exec", "31337", "--", "echo", "wallet=@wallet"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("wallet={}", TEST_ADDRESS)));
}

#[test]
fn exec_does_not_expose_key_unless_requested() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);

    chainz(home.path())
        .args([
            "exec",
            "testchain",
            "--",
            "sh",
            "-c",
            "echo key=${RAW_PRIVATE_KEY:-unset}",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("key=unset"));
}

#[test]
fn exec_passes_through_exit_code() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);

    chainz(home.path())
        .args(["exec", "testchain", "--", "sh", "-c", "exit 7"])
        .assert()
        .code(7);
}

#[test]
fn exec_without_command_fails() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    chainz(home.path())
        .args(["exec", "testchain"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No command specified"));
}

#[test]
fn exec_unknown_chain_fails() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    chainz(home.path())
        .args(["exec", "unknown", "--", "echo", "hi"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn add_noninteractive_requires_existing_key() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .args([
            "add",
            "--name",
            "local",
            "--chain-id",
            "31337",
            "--rpc-url",
            "http://localhost:1",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Key 'default' not found"));
}
