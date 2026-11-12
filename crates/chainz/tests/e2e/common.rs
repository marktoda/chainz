//! Shared harness for the end-to-end tests.
//!
//! Every invocation of the binary goes through a [`Sandbox`]: the
//! environment is cleared down to an allow-list (so nothing from the
//! caller's shell — context variables, `EDITOR`, keys — leaks in), `HOME`
//! and all four XDG directories point into a fresh temp dir,
//! `CHAINZ_DISABLE_KEYRING` is set (the OS keyring is never touched) and
//! stdin is empty. Never build a `Command` for the binary any other way.
//!
//! RPCs are [`Mock`]s on loopback; nothing here touches the network.

pub use chainz_testkit::{
    Answer, Call, ChildGuard, Mock, TEST_ADDRESS, TEST_ADDRESS_2, TEST_KEY, TEST_KEY_2,
    require_tool, wait_until,
};
pub use predicates::prelude::*;
pub use serde_json::{Value, json};
pub use std::fs;
pub use std::time::Duration;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

/// Inherited variables the binary may see; everything else is cleared.
const ALLOWED: &[&str] = &["PATH", "TMPDIR", "RUST_BACKTRACE", "LLVM_PROFILE_FILE"];

/// An isolated home for one test.
pub struct Sandbox {
    root: TempDir,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox {
    pub fn new() -> Self {
        let root = TempDir::new().unwrap();
        for d in ["home", "config/chainz", "state", "cache", "runtime"] {
            fs::create_dir_all(root.path().join(d)).unwrap();
        }
        Self { root }
    }

    /// The sandbox's root directory.
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// A path under the root.
    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    pub fn home(&self) -> PathBuf {
        self.path("home")
    }

    /// `$XDG_CONFIG_HOME/chainz`.
    pub fn dir(&self) -> PathBuf {
        self.path("config/chainz")
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir().join("config.toml")
    }

    pub fn secrets_path(&self) -> PathBuf {
        self.dir().join("secrets.toml")
    }

    /// `$XDG_RUNTIME_DIR/chainz/<name>`.
    pub fn runtime_file(&self, name: &str) -> PathBuf {
        self.path("runtime/chainz").join(name)
    }

    /// `$XDG_STATE_HOME/chainz/<name>`.
    pub fn state_file(&self, name: &str) -> PathBuf {
        self.path("state/chainz").join(name)
    }

    /// `$XDG_CACHE_HOME/chainz/<name>`.
    pub fn cache_file(&self, name: &str) -> PathBuf {
        self.path("cache/chainz").join(name)
    }

    /// The fork registry.
    pub fn forks_dir(&self) -> PathBuf {
        self.runtime_file("forks")
    }

    /// Point `cmd` at the sandbox, with a cleared environment.
    pub fn apply(&self, cmd: &mut Command) {
        cmd.env_clear();
        for name in ALLOWED {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        let r = self.root.path();
        cmd.env("HOME", r.join("home"))
            .env("XDG_CONFIG_HOME", r.join("config"))
            .env("XDG_STATE_HOME", r.join("state"))
            .env("XDG_CACHE_HOME", r.join("cache"))
            .env("XDG_RUNTIME_DIR", r.join("runtime"))
            .env("CHAINZ_DISABLE_KEYRING", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
    }

    /// The binary as a plain `std::process::Command` (for spawning).
    pub fn process(&self) -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("chainz"));
        self.apply(&mut cmd);
        cmd
    }

    /// The binary, sandboxed, for `assert_cmd` assertions.
    pub fn cmd(&self) -> assert_cmd::Command {
        assert_cmd::Command::from_std(self.process())
    }

    /// Run `chainz <args>` to completion.
    pub fn run(&self, args: &[&str]) -> Output {
        self.process().args(args).output().unwrap()
    }

    /// Run `chainz <args>`, require success, and return stdout.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "chainz {args:?} failed ({}): {}",
            out.status,
            stderr(&out)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Write a file in the config directory, mode 0600.
    pub fn write(&self, name: &str, text: &str) {
        let path = self.dir().join(name);
        fs::write(&path, text).unwrap();
        set_mode(&path, 0o600);
    }

    pub fn write_config(&self, toml: &str) {
        self.write("config.toml", toml);
    }

    pub fn write_secrets(&self, toml: &str) {
        self.write("secrets.toml", toml);
    }

    pub fn config(&self) -> String {
        fs::read_to_string(self.config_path()).unwrap()
    }

    /// `secrets.toml`, or empty when there is none.
    pub fn secrets(&self) -> String {
        fs::read_to_string(self.secrets_path()).unwrap_or_default()
    }

    /// Chains `(name, id)` on a dead local RPC; see [`Sandbox::seed_with`].
    pub fn seed(&self, chains: &[(&str, u64)]) {
        let chains: Vec<(&str, u64, &str)> = chains
            .iter()
            .map(|(name, id)| (*name, *id, "http://127.0.0.1:1"))
            .collect();
        self.seed_with(&chains);
    }

    /// Chains `(name, id, rpc url)`, each on endpoint `local`, with a
    /// plaintext `default` signer (TEST_KEY) attached to each.
    pub fn seed_with(&self, chains: &[(&str, u64, &str)]) {
        let mut toml = String::from("version = 2\n\n[signers.default]\nbackend = \"plaintext\"\n");
        toml.push_str(&format!("address = \"{TEST_ADDRESS}\"\n"));
        for (name, id, url) in chains {
            toml.push_str(&format!(
                "\n[chains.{name}]\nid = {id}\nsigner = \"default\"\nrpc = \"local\"\n\n[chains.{name}.rpcs]\nlocal = \"{url}\"\n"
            ));
        }
        self.write_config(&toml);
        self.write_secrets(&format!(
            "[signers.default]\nprivate_key = \"{TEST_KEY}\"\n"
        ));
    }
}

pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

pub fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Send `signal` (e.g. `"TERM"`) to `pid` with `kill(1)`.
pub fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -{signal} {pid}");
}

/// A mock answering each call with `handler`: `Ok(result)`, or
/// `Err(message)` for a JSON-RPC error (code 3).
pub fn mock(handler: impl Fn(&Call) -> Result<Value, String> + Send + Sync + 'static) -> Mock {
    Mock::start(move |call| handler(call).into())
}

/// A mock chain: `eth_chainId` is `chain_id`, the head is `head`, and every
/// answer takes `delay`.
pub fn chain_rpc(chain_id: u64, head: u64, delay: Duration) -> Mock {
    mock(move |call| {
        std::thread::sleep(delay);
        Ok(match call.method.as_str() {
            "eth_chainId" => json!(format!("{chain_id:#x}")),
            "eth_blockNumber" => json!(format!("{head:#x}")),
            _ => Value::Null,
        })
    })
}

/// A fresh chainlist cache with one entry.
pub fn seed_chainlist(sb: &Sandbox, name: &str, chain_id: u64, rpcs: &[&str]) {
    let path = sb.cache_file("chains.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let entry = json!([{
        "name": name, "chainId": chain_id, "shortName": name.to_lowercase(), "rpc": rpcs,
        "nativeCurrency": {"name": "Ether", "symbol": "ETH", "decimals": 18}
    }]);
    fs::write(path, entry.to_string()).unwrap();
}

/// `(active, [(name, url)])` of a chain, URLs revealed.
pub fn endpoints(sb: &Sandbox, chain: &str) -> (String, Vec<(String, String)>) {
    let shown: Value = serde_json::from_str(&sb.ok(&["ls", chain, "--json", "--reveal"])).unwrap();
    let list = shown["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap().to_string(),
                e["url"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    (shown["rpc"].as_str().unwrap().to_string(), list)
}

/// The v1 `config.json` fixture chainz-core's migration tests use.
pub fn v1_fixture() -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../chainz-core/tests/fixtures/v1-config.json");
    fs::read_to_string(path).unwrap()
}

/// Runs `chainz serve stop` in the sandbox when dropped, so a gateway
/// started with `serve --detach` never outlives its test.
pub struct StopGateway<'a>(pub &'a Sandbox);

impl Drop for StopGateway<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(&["serve", "stop"]);
    }
}
