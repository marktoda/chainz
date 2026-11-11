//! End-to-end CLI tests.
//!
//! Every invocation runs inside a [`Sandbox`]: `HOME` and all four XDG
//! directories point into a fresh temp dir, `CHAINZ_DISABLE_KEYRING` is set
//! (the OS keyring is never touched), and context variables inherited from
//! the caller's shell are removed. Never build a `Command` for the binary
//! any other way.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Well-known anvil test keys #0 and #1.
const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const TEST_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const TEST_KEY_2: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const TEST_ADDRESS_2: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Variables chainz reads or exports; scrubbed from every test process.
const SCRUBBED: &[&str] = &[
    "CHAINZ_CHAIN",
    "ETH_RPC_URL",
    "ETH_RPC_HEADERS",
    "CHAIN_ID",
    "CHAIN",
    "CHAIN_NAME",
    "ETHERSCAN_API_KEY",
    "VERIFIER_URL",
    "VERIFIER_API_KEY",
    "ETH_FROM",
    "WALLET_ADDRESS",
    "ETH_KEYSTORE",
    "RAW_PRIVATE_KEY",
    "VISUAL",
    "EDITOR",
];

/// An isolated home for one test.
struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        for d in ["home", "config", "state", "cache", "runtime"] {
            fs::create_dir_all(root.path().join(d)).unwrap();
        }
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// `$XDG_CONFIG_HOME/chainz`.
    fn dir(&self) -> PathBuf {
        self.root.path().join("config").join("chainz")
    }

    fn config_path(&self) -> PathBuf {
        self.dir().join("config.toml")
    }

    fn secrets_path(&self) -> PathBuf {
        self.dir().join("secrets.toml")
    }

    fn apply(&self, cmd: &mut std::process::Command) {
        let r = self.root.path();
        for name in SCRUBBED {
            cmd.env_remove(name);
        }
        cmd.env("HOME", r.join("home"))
            .env("XDG_CONFIG_HOME", r.join("config"))
            .env("XDG_STATE_HOME", r.join("state"))
            .env("XDG_CACHE_HOME", r.join("cache"))
            .env("XDG_RUNTIME_DIR", r.join("runtime"))
            .env("CHAINZ_DISABLE_KEYRING", "1")
            .env("NO_COLOR", "1");
    }

    /// The binary, sandboxed.
    fn cmd(&self) -> Command {
        let mut std = std::process::Command::new(assert_cmd::cargo::cargo_bin!("chainz"));
        self.apply(&mut std);
        Command::from_std(std)
    }

    /// The binary as a plain `std::process::Command` (for spawning).
    fn process(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("chainz"));
        self.apply(&mut cmd);
        cmd
    }

    fn write_config(&self, toml: &str) {
        fs::create_dir_all(self.dir()).unwrap();
        fs::write(self.config_path(), toml).unwrap();
        set_mode(&self.config_path(), 0o600);
    }

    fn write_secrets(&self, toml: &str) {
        fs::create_dir_all(self.dir()).unwrap();
        fs::write(self.secrets_path(), toml).unwrap();
        set_mode(&self.secrets_path(), 0o600);
    }

    fn config(&self) -> String {
        fs::read_to_string(self.config_path()).unwrap()
    }

    fn secrets(&self) -> String {
        fs::read_to_string(self.secrets_path()).unwrap_or_default()
    }

    /// Chains `(name, id)` on a dead local RPC, a plaintext `default`
    /// signer (TEST_KEY) attached to each.
    fn seed(&self, chains: &[(&str, u64)]) {
        let mut toml = String::from("version = 2\n\n[signers.default]\nbackend = \"plaintext\"\n");
        toml.push_str(&format!("address = \"{TEST_ADDRESS}\"\n"));
        for (name, id) in chains {
            toml.push_str(&format!(
                "\n[chains.{name}]\nid = {id}\nsigner = \"default\"\nrpc = \"local\"\n\n[chains.{name}.rpcs]\nlocal = \"http://127.0.0.1:1\"\n"
            ));
        }
        self.write_config(&toml);
        self.write_secrets(&format!(
            "[signers.default]\nprivate_key = \"{TEST_KEY}\"\n"
        ));
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Serve `n` deterministic `eth_chainId` responses on a local port.
fn chain_id_rpc(chain_id: u64, n: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for _ in 0..n {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let body = format!(r#"{{"jsonrpc":"2.0","id":1,"result":"0x{chain_id:x}"}}"#);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{address}")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../chainz-core/tests/fixtures")
        .join(name);
    fs::read_to_string(path).unwrap()
}

// ---------------------------------------------------------------- basics

#[test]
fn version_flag_works() {
    let sb = Sandbox::new();
    sb.cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("chainz"));
}

#[test]
fn ls_on_empty_config_shows_hint_and_writes_nothing() {
    let sb = Sandbox::new();
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("No chains configured"));
    assert!(
        !sb.config_path().exists(),
        "a read-only command must not create config"
    );
}

#[test]
fn completions_generate_for_zsh_and_bash() {
    let sb = Sandbox::new();
    for shell in ["zsh", "bash", "fish"] {
        sb.cmd()
            .args(["completions", shell])
            .assert()
            .success()
            .stdout(predicate::str::contains("chainz"));
    }
}

#[test]
fn stream_commands_are_stubs() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    for args in [
        vec!["serve"],
        vec!["serve", "status"],
        vec!["top"],
        vec!["fork", "eth"],
        vec!["fork", "ls"],
        vec!["addr", "ls"],
        vec!["label"],
        vec!["doctor", "--tokens"],
    ] {
        sb.cmd()
            .args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("not yet implemented"));
    }
}

// ---------------------------------------------------------------- config files

#[test]
#[cfg(unix)]
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
#[cfg(unix)]
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

#[test]
fn edits_preserve_comments_and_order() {
    let sb = Sandbox::new();
    sb.write_config(
        "# my chains\nversion = 2\n\n# mainnet first\n[chains.eth]\nid = 1\nrpc = \"a\" # keep me\n\n[chains.eth.rpcs]\na = \"http://a.invalid\"\nb = \"http://b.invalid\"\n",
    );
    sb.cmd().args(["rpc", "use", "eth", "b"]).assert().success();
    let text = sb.config();
    assert!(text.contains("# my chains"));
    assert!(text.contains("# mainnet first"));
    assert!(text.contains("rpc = \"b\""));
}

// ---------------------------------------------------------------- migration

#[test]
fn v1_config_is_migrated_on_first_run() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.dir()).unwrap();
    fs::write(sb.dir().join("config.json"), fixture("v1-config.json")).unwrap();
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "migrated 17 chains, 149 endpoints, 3 signers, 4 variables",
        ));
    assert!(sb.config_path().exists());
    assert!(sb.dir().join("config.v1.json.bak").exists());
    assert!(!sb.dir().join("config.json").exists());
    // Second run: nothing to migrate, no notice.
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stderr(predicate::str::contains("migrated").not());
}

#[test]
fn legacy_home_config_is_migrated() {
    let sb = Sandbox::new();
    fs::write(sb.home().join(".chainz.json"), fixture("v1-config.json")).unwrap();
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stderr(predicate::str::contains("migrated 17 chains"));
    assert!(sb.config_path().exists());
    assert!(sb.home().join(".chainz.v1.json.bak").exists());
}

#[test]
fn migrate_dry_run_writes_nothing() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.dir()).unwrap();
    fs::write(sb.dir().join("config.json"), fixture("v1-config.json")).unwrap();
    let out = sb
        .cmd()
        .args(["config", "migrate", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(stdout(&out).contains("version = 2"));
    assert!(stdout(&out).contains("[chains."));
    assert!(String::from_utf8_lossy(&out.stderr).contains("dry run"));
    assert!(!sb.config_path().exists());
    assert!(!sb.secrets_path().exists());
    assert!(sb.dir().join("config.json").exists());
    // The fixture's fake keys never reach stdout.
    assert!(!stdout(&out).contains("private_key"));
}

#[test]
fn migrated_config_never_prints_secrets() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.dir()).unwrap();
    fs::write(sb.dir().join("config.json"), fixture("v1-config.json")).unwrap();
    let out = sb.cmd().arg("ls").assert().success().get_output().clone();
    let secrets = fs::read_to_string(sb.secrets_path()).unwrap();
    let all = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    for line in secrets.lines() {
        if let Some((_, value)) = line.split_once(" = ") {
            let value = value.trim_matches('"');
            if value.len() >= 12 {
                assert!(!all.contains(value), "printed a secret value");
            }
        }
    }
}

// ---------------------------------------------------------------- vars

#[test]
fn var_set_get_ls_rm_roundtrip() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["var", "set", "API_KEY", "supersecret123"])
        .assert()
        .success();
    sb.cmd()
        .args(["var", "get", "API_KEY"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("REDACTED").and(predicate::str::contains("supersecret").not()),
        );
    sb.cmd()
        .args(["var", "get", "API_KEY", "--reveal"])
        .assert()
        .success()
        .stdout("supersecret123\n");
    sb.cmd().args(["var", "ls"]).assert().success().stdout(
        predicate::str::contains("API_KEY").and(predicate::str::contains("supersecret").not()),
    );
    assert!(
        !sb.config().contains("supersecret"),
        "values live in secrets.toml"
    );
    sb.cmd().args(["var", "rm", "API_KEY"]).assert().success();
    sb.cmd().args(["var", "get", "API_KEY"]).assert().failure();
}

#[test]
fn var_stdin_preserves_intentional_whitespace() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["var", "set", "SPACED", "--stdin"])
        .write_stdin("  value \n")
        .assert()
        .success();
    sb.cmd()
        .args(["var", "get", "SPACED", "--reveal"])
        .assert()
        .success()
        .stdout("  value \n");
}

#[test]
fn var_ls_json_is_machine_readable() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["var", "set", "A", "secretvalue"])
        .assert()
        .success();
    let out = sb
        .cmd()
        .args(["var", "ls", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v[0]["name"], "A");
    assert!(!stdout(&out).contains("secretvalue"));
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

// ---------------------------------------------------------------- signers

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
fn signer_rm_requires_force_when_attached() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["signer", "rm", "default"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--force"));
    sb.cmd()
        .args(["signer", "rm", "default", "--force"])
        .assert()
        .success();
    assert!(!sb.config().contains("[signers.default]"));
    assert!(!sb.secrets().contains(TEST_KEY));
    assert!(!sb.config().contains("signer = \"default\""));
}

#[test]
fn signer_ls_json_never_leaks_key_material() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let out = sb
        .cmd()
        .args(["signer", "ls", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v[0]["address"], TEST_ADDRESS);
    assert_eq!(v[0]["chains"][0], "eth");
    assert!(!stdout(&out).contains(TEST_KEY));
}

#[test]
fn keystore_signer_is_a_reference() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let ks = sb.home().join("ks.json");
    fs::write(&ks, "{}").unwrap();
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

// ---------------------------------------------------------------- chains

#[test]
fn chain_add_by_hand_checks_chain_id() {
    let sb = Sandbox::new();
    let url = chain_id_rpc(8453, 1);
    sb.cmd()
        .args(["chain", "add", "8453", "--rpc", &url])
        .assert()
        .success()
        .stdout(predicate::str::contains("Added chain 'base'"));
    assert!(sb.config().contains("[chains.base]"));
    // Wrong ID is refused.
    let url = chain_id_rpc(1, 1);
    sb.cmd()
        .args(["chain", "add", "10", "--rpc", &url])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RPC check failed"));
}

#[test]
fn rpc_failure_never_leaks_the_url() {
    let sb = Sandbox::new();
    sb.cmd()
        .args([
            "chain",
            "add",
            "8453",
            "--rpc",
            "http://127.0.0.1:1/v3/topsecretkey1234567",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("topsecretkey").not());
}

#[test]
fn chain_rm_by_name_and_id_requires_exact_match() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453), ("arb", 42161)]);
    sb.cmd().args(["chain", "rm", "ba"]).assert().failure();
    sb.cmd().args(["chain", "rm", "base"]).assert().success();
    sb.cmd().args(["chain", "rm", "42161"]).assert().success();
    sb.cmd().args(["chain", "rm", "nope"]).assert().failure();
    let text = sb.config();
    assert!(text.contains("[chains.eth]"));
    assert!(!text.contains("[chains.base]") && !text.contains("[chains.arb]"));
}

#[test]
fn chain_rename_and_alias() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd().args(["use", "eth"]).assert().success();
    sb.cmd()
        .args(["chain", "rename", "eth", "mainnet"])
        .assert()
        .success();
    sb.cmd()
        .args(["chain", "alias", "mainnet", "ethereum"])
        .assert()
        .success();
    let text = sb.config();
    assert!(text.contains("default = \"mainnet\""));
    assert!(text.contains("aliases = [\"ethereum\"]"));
    sb.cmd()
        .args(["exec", "ethereum", "--", "sh", "-c", "echo $CHAINZ_CHAIN"])
        .assert()
        .success()
        .stdout("mainnet\n");
}

#[test]
fn ls_is_compact_and_json_is_machine_readable() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("eth").and(predicate::str::contains("8453")));
    let out = sb
        .cmd()
        .args(["ls", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v[1]["name"], "base");
    assert_eq!(v[1]["id"], 8453);
    assert_eq!(v[1]["signer"], "default");
}

#[test]
fn output_redacts_credentials_unless_revealed() {
    let sb = Sandbox::new();
    sb.write_config(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"lit\"\n[chains.eth.rpcs]\nlit = \"https://x.invalid/v3/abcdef0123456789abcdef\"\nhdr = { url = \"https://y.invalid\", headers = { \"x-api-key\" = \"headersecret999\" } }\n",
    );
    for args in [
        vec!["ls"],
        vec!["ls", "--json"],
        vec!["chain", "show", "eth"],
        vec!["chain", "show", "eth", "--json"],
        vec!["rpc", "ls"],
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
        .args(["chain", "show", "eth", "--reveal"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("abcdef0123456789abcdef")
                .and(predicate::str::contains("headersecret999")),
        );
}

#[test]
fn rpc_add_with_header_and_use_and_rm() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .args(["var", "set", "KEY", "hdrvalue1234"])
        .assert()
        .success();
    sb.cmd()
        .args([
            "rpc",
            "add",
            "eth",
            "https://private.invalid",
            "--name",
            "priv",
            "--header",
            "x-api-key: ${KEY}",
            "--no-check",
            "--use",
        ])
        .assert()
        .success();
    assert!(
        sb.config().contains("\"x-api-key\" = \"${KEY}\"")
            || sb.config().contains("x-api-key = \"${KEY}\"")
    );
    sb.cmd()
        .args(["exec", "eth", "--", "sh", "-c", "echo $ETH_RPC_HEADERS"])
        .assert()
        .success()
        .stdout("x-api-key: hdrvalue1234\n");
    sb.cmd()
        .args(["rpc", "rm", "eth", "priv"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("active"));
    sb.cmd()
        .args(["rpc", "use", "eth", "local"])
        .assert()
        .success();
    sb.cmd()
        .args(["rpc", "rm", "eth", "priv"])
        .assert()
        .success();
}

// ---------------------------------------------------------------- exec

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
fn exec_without_command_or_unknown_chain_fails() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd().args(["exec", "eth"]).assert().failure();
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
            "rpc",
            "add",
            "eth",
            "http://127.0.0.2:1",
            "--name",
            "two",
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
fn use_sets_default_with_stderr_notice() {
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
    sb.cmd().args(["use", "--clear"]).assert().success();
    assert!(!sb.config().contains("default ="));
}

#[test]
fn shell_exports_context_and_passes_exit_code() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd()
        .env("SHELL", "sh")
        .args(["shell", "eth"])
        .write_stdin("echo $CHAIN_ID $CHAINZ_CHAIN; exit 3\n")
        .assert()
        .code(3)
        .stdout("1 eth\n");
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

#[test]
fn rpc_only_chain_runs_until_a_signer_is_needed() {
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

// ---------------------------------------------------------------- doctor

#[test]
fn doctor_reports_dead_rpc_and_plaintext_signer() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    sb.cmd().arg("doctor").assert().code(1).stdout(
        predicate::str::contains("plaintext")
            .and(predicate::str::contains("eth: active endpoint 'local'")),
    );
}

#[test]
fn doctor_fix_moves_literal_credentials_into_variables() {
    let sb = Sandbox::new();
    let url = chain_id_rpc(1, 4);
    sb.write_config(&format!(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"local\"\n[chains.eth.rpcs]\nlocal = \"{url}\"\ninfura = \"https://mainnet.infura.invalid/v3/0123456789abcdef0123456789abcdef\"\n"
    ));
    sb.cmd()
        .args(["doctor", "--fix"])
        .assert()
        .stdout(predicate::str::contains("${INFURA_KEY}"));
    assert!(sb.config().contains("/v3/${INFURA_KEY}"));
    assert!(
        sb.secrets()
            .contains("INFURA_KEY = \"0123456789abcdef0123456789abcdef\"")
    );
}

#[test]
fn doctor_reports_semantically_invalid_config() {
    let sb = Sandbox::new();
    sb.write_config("version = 2\n[chains.eth]\nid = 1\nrpc = \"missing\"\n[chains.eth.rpcs]\na = \"http://x.invalid\"\n");
    sb.cmd().arg("ls").assert().failure();
    sb.cmd()
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("missing"));
}

#[test]
fn config_path_lists_every_location() {
    let sb = Sandbox::new();
    sb.cmd()
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(format!("{}\n", sb.config_path().display()));
    sb.cmd()
        .args(["config", "path", "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("secrets").and(predicate::str::contains("runtime")));
}

#[test]
fn config_edit_validates_before_saving() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let before = sb.config();
    // A non-interactive "editor" that breaks a reference.
    sb.cmd()
        .env(
            "EDITOR",
            "sed -i s/rpc\\ =\\ \\\"local\\\"/rpc\\ =\\ \\\"nope\\\"/",
        )
        .args(["config", "edit"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nope"));
    assert_eq!(sb.config(), before);
    sb.cmd()
        .env("EDITOR", "sed -i s/id\\ =\\ 1$/id\\ =\\ 5/")
        .args(["config", "edit"])
        .assert()
        .success();
    assert!(sb.config().contains("id = 5"));
}

#[test]
fn chain_edit_replaces_one_chain() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    sb.cmd()
        .env("EDITOR", "sed -i s/id\\ =\\ 8453/id\\ =\\ 84532/")
        .args(["chain", "edit", "base"])
        .assert()
        .success();
    assert!(sb.config().contains("id = 84532"));
    assert!(sb.config().contains("[chains.eth]"));
}
