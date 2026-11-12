//! Shared harness for the end-to-end tests.
//!
//! Every invocation of the binary goes through a [`Sandbox`]: the
//! environment is cleared down to an allow-list (so nothing from the
//! caller's shell — context variables, `EDITOR`, keys — leaks in), `HOME`
//! and all four XDG directories point into a fresh temp dir,
//! `CHAINZ_DISABLE_KEYRING` is set (the OS keyring is never touched) and
//! stdin is empty. Never build a `Command` for the binary any other way.
//!
//! RPCs are [`MockRpc`]s on loopback; nothing here touches the network.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Well-known anvil test key #0 and its address.
pub const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
pub const TEST_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

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

/// Poll `done` every 20 ms until it holds; fail after `timeout`.
pub fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Send `signal` (e.g. `"TERM"`) to `pid` with `kill(1)`.
pub fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -{signal} {pid}");
}

/// One JSON-RPC request a [`MockRpc`] received (batch elements each count).
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub params: Value,
    /// The HTTP request path.
    pub path: String,
    /// The HTTP head, lowercased (request line and headers).
    pub head: String,
}

/// What a mock answers: a `result`, or an error message (code 3).
pub type Reply = Result<Value, String>;

type Handler = dyn Fn(&Request) -> Reply + Send + Sync;

/// A threaded, keep-alive JSON-RPC server on loopback. Every request
/// (batches too) is answered by its handler and recorded.
pub struct MockRpc {
    pub port: u16,
    seen: Arc<Mutex<Vec<Request>>>,
}

impl MockRpc {
    pub fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Request>>> = Arc::default();
        let handler: Arc<Handler> = Arc::new(handler);
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (log, handler) = (log.clone(), handler.clone());
                std::thread::spawn(move || serve_connection(stream, &log, &*handler));
            }
        });
        MockRpc { port, seen }
    }

    /// Answers `eth_chainId` with `chain_id`, and nothing else.
    pub fn chain_id(chain_id: u64) -> Self {
        Self::start(move |request| match request.method.as_str() {
            "eth_chainId" => Ok(json!(format!("{chain_id:#x}"))),
            _ => Err("method not found".into()),
        })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every request so far, in arrival order.
    pub fn seen(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }

    /// The methods of every request so far.
    pub fn methods(&self) -> Vec<String> {
        self.seen().into_iter().map(|r| r.method).collect()
    }
}

fn serve_connection(stream: TcpStream, log: &Mutex<Vec<Request>>, handler: &Handler) {
    let mut reader = BufReader::new(stream);
    while let Some((head, body)) = read_message(&mut reader) {
        let path = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        let parsed: Value = serde_json::from_slice(&body).unwrap_or_default();
        let mut answer = |call: &Value| {
            let request = Request {
                method: call["method"].as_str().unwrap_or("?").to_string(),
                params: call["params"].clone(),
                path: path.clone(),
                head: head.clone(),
            };
            let reply = handler(&request);
            log.lock().unwrap().push(request);
            match reply {
                Ok(result) => json!({"jsonrpc": "2.0", "id": call["id"], "result": result}),
                Err(message) => json!({"jsonrpc": "2.0", "id": call["id"],
                                       "error": {"code": 3, "message": message}}),
            }
        };
        let response = match &parsed {
            Value::Array(calls) => Value::Array(calls.iter().map(&mut answer).collect()),
            call => answer(call),
        }
        .to_string();
        let message = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{response}",
            response.len()
        );
        if reader.get_mut().write_all(message.as_bytes()).is_err() {
            return;
        }
    }
}

/// Read one HTTP message (request or response): lowercased head and body.
pub fn read_message(reader: &mut BufReader<TcpStream>) -> Option<(String, Vec<u8>)> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.trim_end().is_empty() {
            break;
        }
        head.push_str(&line.to_ascii_lowercase());
    }
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some((head, body))
}
