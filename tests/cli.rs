//! End-to-end CLI tests. Each test gets its own temp HOME so the real
//! `~/.chainz.json` is never touched and tests can run in parallel.

use assert_cmd::Command;
use chainz::chain::ChainDefinition;
use chainz::config::{Config, DEFAULT_CONFIG_RELATIVE, LEGACY_CONFIG_FILE};
use chainz::key::{Key, KeyType};
use predicates::prelude::*;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Well-known anvil test keys #0 and #1
const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const TEST_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const TEST_KEY_2: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const TEST_ADDRESS_2: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

fn chainz(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("chainz").unwrap();
    cmd.env("HOME", home);
    // The ambient environment must not redirect the config out of the temp HOME
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd
}

fn config_path(home: &Path) -> std::path::PathBuf {
    home.join(DEFAULT_CONFIG_RELATIVE)
}

/// Write raw config content at the standard location (creating parent dirs).
fn write_raw_config(home: &Path, content: &str) {
    let path = config_path(home);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Seed a config with one key and the given chains via the library's own
/// types, bypassing the CLI (non-interactive `add` requires a live RPC to
/// health-check against).
fn seed_config(home: &Path, chains: &[(&str, u64)]) {
    let config = Config {
        chains: chains
            .iter()
            .map(|(name, id)| ChainDefinition {
                name: name.to_string(),
                aliases: vec![],
                chain_id: *id,
                rpc_urls: vec!["http://localhost:1".to_string()],
                selected_rpc: "http://localhost:1".to_string(),
                verification_api_key: None,
                verification_url: None,
                key_name: "default".to_string(),
            })
            .collect(),
        keys: std::collections::HashMap::from([(
            "default".to_string(),
            Key::new(
                "default".to_string(),
                KeyType::PrivateKey {
                    value: TEST_KEY.to_string(),
                },
            ),
        )]),
        ..Default::default()
    };
    write_raw_config(home, &serde_json::to_string_pretty(&config).unwrap());
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
        .args(["var", "get", "MY_KEY", "--show"])
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
fn var_stdin_preserves_intentional_whitespace() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .args(["var", "set", "SPACED", "--stdin"])
        .write_stdin("  intentional value  \n")
        .assert()
        .success();
    chainz(home.path())
        .args(["var", "get", "SPACED", "--show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("SPACED =   intentional value  \n"));
}

#[test]
fn key_add_is_noninteractive_with_stdin_and_explicit_plaintext() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .args(["key", "add", "default", "--stdin", "--type", "private-key"])
        .write_stdin(TEST_KEY)
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
fn bare_key_add_never_silently_falls_back_to_plaintext() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .env("CHAINZ_DISABLE_KEYRING", "1")
        .args(["key", "add", "default", "--key", TEST_KEY])
        .assert()
        .failure()
        .stderr(predicate::str::contains("interactive password prompt"));
    assert!(!config_path(home.path()).exists());
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
        .args([
            "key",
            "add",
            "temp",
            "--key",
            TEST_KEY,
            "--type",
            "private-key",
        ])
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
    write_raw_config(home.path(), "{not valid json");

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
    // exec resolves the chain from config alone — no live RPC is contacted.
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
fn exec_key_override_flag() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    chainz(home.path())
        .args([
            "key",
            "add",
            "alt",
            "--key",
            TEST_KEY_2,
            "--type",
            "private-key",
        ])
        .assert()
        .success();

    // without -k: default key's wallet; with -k alt: the override's wallet
    chainz(home.path())
        .args(["exec", "testchain", "--", "echo", "@wallet"])
        .assert()
        .success()
        .stdout(predicate::str::contains(TEST_ADDRESS));
    chainz(home.path())
        .args(["exec", "testchain", "-k", "alt", "--", "echo", "@wallet"])
        .assert()
        .success()
        .stdout(predicate::str::contains(TEST_ADDRESS_2));
}

#[test]
fn key_add_duplicate_name_fails() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .args([
            "key",
            "add",
            "dup",
            "--key",
            TEST_KEY,
            "--type",
            "private-key",
        ])
        .assert()
        .success();
    chainz(home.path())
        .args([
            "key",
            "add",
            "dup",
            "--key",
            TEST_KEY_2,
            "--type",
            "private-key",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

/// Pins the on-disk wire format so existing user configs keep loading.
/// This is the ONE test that intentionally hand-writes the JSON; behavioral
/// tests should seed via the library types instead.
#[test]
fn legacy_config_format_still_loads() {
    let home = TempDir::new().unwrap();
    let legacy = r#"{
        "chains": [{
            "name": "ethereum",
            "chain_id": 1,
            "rpc_urls": ["https://eth.example.com"],
            "selected_rpc": "https://eth.example.com",
            "verification_api_key": null,
            "verification_url": null,
            "key_name": "default"
        }],
        "variables": { "MY_VAR": "abc" },
        "keys": {
            "default": { "name": "default", "type": "PrivateKey", "value": "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80" }
        }
    }"#;
    write_raw_config(home.path(), legacy);

    chainz(home.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("ethereum"));
    chainz(home.path())
        .args(["var", "get", "MY_VAR", "--show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("MY_VAR = abc"));
}

#[test]
fn exec_resolves_prefix_and_alias() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("ethereum", 1), ("optimism", 10)]);

    // unambiguous prefix
    chainz(home.path())
        .args(["exec", "eth", "--", "echo", "@chainname"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ethereum"));

    // ambiguous reference fails with candidates listed
    seed_config(home.path(), &[("base", 8453), ("basecamp", 123)]);
    chainz(home.path())
        .args(["exec", "bas", "--", "echo", "hi"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ambiguous"));
}

#[test]
fn completions_generate_for_zsh_and_bash() {
    let home = TempDir::new().unwrap();
    chainz(home.path())
        .args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("#compdef chainz"));
    chainz(home.path())
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("chainz"));
}

#[test]
fn list_json_outputs_machine_readable_chains() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("ethereum", 1)]);

    let output = chainz(home.path())
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(
        !text.contains("verification_api_key"),
        "credentials must not appear in the scripting output"
    );
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed[0]["name"], "ethereum");
    assert_eq!(parsed[0]["chain_id"], 1);
    assert_eq!(parsed[0]["is_default"], false);
    assert!(parsed[0]["aliases"].is_array(), "shape must be regular");
}

#[test]
fn key_list_json_never_leaks_key_material() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[]);

    let output = chainz(home.path())
        .args(["key", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(
        !text.contains(TEST_KEY),
        "key material must never appear in JSON output"
    );
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed[0]["name"], "default");
    assert_eq!(parsed[0]["type"], "PrivateKey");
    assert_eq!(parsed[0]["address"], TEST_ADDRESS);
}

#[test]
fn doctor_reports_dead_rpc_and_exits_nonzero() {
    let home = TempDir::new().unwrap();
    // selected RPC and the sole alternative are both dead
    seed_config(home.path(), &[("deadchain", 31337)]);

    chainz(home.path())
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("deadchain"))
        .stdout(predicate::str::contains("failure(s)"));

    // --fix finds no healthy alternative but still exits gracefully
    chainz(home.path())
        .args(["doctor", "--fix"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("no healthy alternative"));
}

#[test]
fn doctor_warns_on_plaintext_keys() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[]);
    chainz(home.path())
        .arg("doctor")
        .assert()
        .success() // warnings alone don't fail the check
        .stdout(predicate::str::contains("plaintext"));
}

#[test]
fn use_sets_default_chain_for_exec() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("ethereum", 1), ("optimism", 10)]);

    chainz(home.path())
        .args(["use", "optimism"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Default chain set to 'optimism'"));

    // bare exec uses the default; explicit chain still wins
    chainz(home.path())
        .args(["exec", "--", "echo", "@chainname"])
        .assert()
        .success()
        .stdout(predicate::str::contains("optimism"));
    chainz(home.path())
        .args(["exec", "ethereum", "--", "echo", "@chainname"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ethereum"));

    // removing the default chain clears it
    chainz(home.path())
        .args(["remove", "optimism"])
        .assert()
        .success();
    let config = fs::read_to_string(config_path(home.path())).unwrap();
    assert!(!config.contains("default_chain"));
}

#[test]
fn use_unknown_chain_fails() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("ethereum", 1)]);
    chainz(home.path())
        .args(["use", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn legacy_config_location_is_migrated() {
    let home = TempDir::new().unwrap();
    // Seed at the pre-0.3 location: ~/.chainz.json
    seed_config(home.path(), &[("ethereum", 1)]);
    let new_path = config_path(home.path());
    let legacy_path = home.path().join(LEGACY_CONFIG_FILE);
    fs::rename(&new_path, &legacy_path).unwrap();

    chainz(home.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("ethereum"))
        .stderr(predicate::str::contains("Migrated config"));

    assert!(new_path.exists(), "config should exist at the new location");
    assert!(!legacy_path.exists(), "legacy config should be moved");

    // subsequent runs are silent (nothing left to migrate)
    chainz(home.path())
        .arg("list")
        .assert()
        .success()
        .stderr(predicate::str::contains("Migrated").not());
}

#[test]
fn shell_sets_env_and_passes_exit_code() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);

    // A fake "shell" that proves env vars arrive and exit codes pass through
    let fake_shell = home.path().join("fakeshell.sh");
    fs::write(
        &fake_shell,
        "#!/bin/sh\necho chain=$CHAINZ_CHAIN rpc=$ETH_RPC_URL key=${RAW_PRIVATE_KEY:-unset}\nexit 3\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_shell, fs::Permissions::from_mode(0o755)).unwrap();
    }

    chainz(home.path())
        .env("SHELL", &fake_shell)
        .args(["shell", "testchain"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains(
            "chain=testchain rpc=http://localhost:1 key=unset",
        ));
}

#[test]
fn shell_uses_default_chain_when_omitted() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    chainz(home.path())
        .args(["use", "testchain"])
        .assert()
        .success();

    let fake_shell = home.path().join("fakeshell.sh");
    fs::write(&fake_shell, "#!/bin/sh\necho in=$CHAINZ_CHAIN\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_shell, fs::Permissions::from_mode(0o755)).unwrap();
    }

    chainz(home.path())
        .env("SHELL", &fake_shell)
        .arg("shell")
        .assert()
        .success()
        .stdout(predicate::str::contains("in=testchain"));
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

#[test]
fn wallet_expansion_does_not_expose_private_key() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    chainz(home.path())
        .args([
            "exec",
            "testchain",
            "--",
            "sh",
            "-c",
            "echo wallet=$WALLET_ADDRESS key=${RAW_PRIVATE_KEY:-unset}",
            "@wallet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "wallet={} key=unset",
            TEST_ADDRESS
        )));
}

#[test]
fn expose_key_uses_environment_without_argv_warning() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    chainz(home.path())
        .args([
            "exec",
            "testchain",
            "--expose-key",
            "--",
            "sh",
            "-c",
            "echo key=$RAW_PRIVATE_KEY",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(TEST_KEY))
        .stderr(predicate::str::contains("process arguments").not());
}

#[test]
fn key_token_warns_about_process_argument_exposure() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    chainz(home.path())
        .args(["exec", "testchain", "--", "echo", "@key"])
        .assert()
        .success()
        .stderr(predicate::str::contains("process arguments"));
}

#[test]
fn chain_output_redacts_credentials_unless_explicitly_requested() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    let path = config_path(home.path());
    let mut config: Config = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let secret_url = "https://user:password@rpc.example.com/v2/rpc-secret?token=query-secret";
    config.chains[0].selected_rpc = secret_url.to_string();
    config.chains[0].rpc_urls = vec![secret_url.to_string()];
    config.chains[0].verification_api_key = Some("verifier-secret".to_string());
    write_raw_config(home.path(), &serde_json::to_string_pretty(&config).unwrap());

    chainz(home.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("rpc-secret").not())
        .stdout(predicate::str::contains("query-secret").not())
        .stdout(predicate::str::contains("verifier-secret").not())
        .stdout(predicate::str::contains("Configured"));
    chainz(home.path())
        .args(["list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rpc-secret").not())
        .stdout(predicate::str::contains("query-secret").not());
    chainz(home.path())
        .args(["list", "--show-secrets"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rpc-secret"))
        .stdout(predicate::str::contains("verifier-secret"));
}

#[test]
fn rpc_failure_error_chain_never_leaks_the_url() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[]);
    chainz(home.path())
        .args([
            "add",
            "--name",
            "leak-test",
            "--chain-id",
            "1",
            "--rpc-url",
            "http://user:password@127.0.0.1:1/literal-secret",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("REDACTED"))
        .stderr(predicate::str::contains("password").not())
        .stderr(predicate::str::contains("literal-secret").not());
}

#[test]
fn referenced_key_removal_is_blocked() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    chainz(home.path())
        .args(["key", "remove", "default"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("still used"));
}

#[test]
fn doctor_can_report_semantically_invalid_config() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    let path = config_path(home.path());
    let mut config: Config = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    config.keys.clear();
    write_raw_config(home.path(), &serde_json::to_string_pretty(&config).unwrap());

    chainz(home.path())
        .arg("doctor")
        .assert()
        .failure()
        .stdout(predicate::str::contains("Configuration"))
        .stdout(predicate::str::contains("missing key"));
}

#[test]
fn migrate_all_failure_never_prints_key_material() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[]);
    chainz(home.path())
        .env("CHAINZ_DISABLE_KEYRING", "1")
        .args(["key", "migrate", "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains(TEST_KEY).not())
        .stderr(predicate::str::contains(TEST_KEY).not())
        .stderr(predicate::str::contains("Failed to migrate"));
}
