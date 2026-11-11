//! `chainz fork` end to end.
//!
//! The upstream is a real `anvil` with no `--fork-url`; `chainz fork`
//! pins it, proxies it, and a second anvil forks from it through the
//! proxy. Tests that need anvil are skipped when it is not on `PATH`.
//!
//! Every invocation runs in a [`Sandbox`] (scratch `HOME`, XDG dirs and
//! `CHAINZ_DISABLE_KEYRING`), as in `cli.rs`.
#![cfg(unix)]

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// The signer's cached address: not one of anvil's dev accounts, so only
/// impersonation can send from it.
const TEST_ADDRESS: &str = "0x0000000000000000000000000000000000012345";
/// An address that only has a balance on the upstream.
const UPSTREAM_ONLY: &str = "0x000000000000000000000000000000000000dEaD";
/// A minimal ERC-20: `balanceOf(a)` is `sload(keccak(a . 0))`; every other
/// call (so `decimals()`) reads `keccak(0 . 0)`.
const TOKEN: &str = "0x00000000000000000000000000000000000c0de1";
const TOKEN_CODE: &str = "0x600435600052600060205260406000205460005260206000f3";
/// `keccak256` of 64 zero bytes: where the token's `decimals()` lives.
const DECIMALS_SLOT: &str = "0xad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5";
/// A label with no contract behind it.
const NOT_A_TOKEN: &str = "0x00000000000000000000000000000000000c0de2";

const SCRUBBED: &[&str] = &[
    "CHAINZ_CHAIN",
    "CHAINZ_FORK_SUPERVISOR",
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
];

// ---------------------------------------------------------------- harness

struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        for d in ["home", "config/chainz", "state", "cache", "runtime"] {
            fs::create_dir_all(root.path().join(d)).unwrap();
        }
        Self { root }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("chainz"));
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
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn write_config(&self, toml: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.root.path().join("config/chainz/config.toml");
        fs::write(&path, toml).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// Chain `local` (31337) on `rpc_url`, with a signer and two labels.
    fn seed_local(&self, rpc_url: &str) {
        self.write_config(&format!(
            r#"version = 2

[signers.default]
backend = "keyring"
address = "{TEST_ADDRESS}"

[signers.ks]
backend = "keystore"
address = "{TEST_ADDRESS}"
path = "/keys/ks"

[chains.local]
id = 31337
aliases = ["devnet"]
signer = "default"
rpc = "up"

[chains.local.rpcs]
up = "{rpc_url}"

[chains.zk]
id = 324
rpc = "up"

[chains.zk.rpcs]
up = "{rpc_url}"

[addresses.tok]
local = "{TOKEN}"

[addresses.nocode]
local = "{NOT_A_TOKEN}"
"#
        ));
    }

    fn forks_dir(&self) -> PathBuf {
        self.root.path().join("runtime/chainz/forks")
    }

    fn record(&self, name: &str) -> Option<Value> {
        let bytes = fs::read(self.forks_dir().join(format!("{name}.json"))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The anvil PID from the fork's sidecar.
    fn anvil_pid(&self, name: &str) -> u32 {
        let bytes = fs::read(self.forks_dir().join(format!("{name}.pids"))).unwrap();
        let pids: Value = serde_json::from_slice(&bytes).unwrap();
        pids["anvil"].as_u64().unwrap() as u32
    }

    fn registry_is_empty(&self) -> bool {
        fs::read_dir(self.forks_dir()).map_or(true, |mut d| d.next().is_none())
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn anvil_available() -> bool {
    let found = Command::new("anvil")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !found {
        eprintln!("skipped: anvil is not on PATH");
    }
    found
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `pid` is a running (non-zombie) process.
fn pid_running(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&output.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

fn process_args(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-ww", "-o", "args=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -{signal} {pid}");
}

/// One JSON-RPC call over a fresh connection.
fn try_rpc(port: u16, method: &str, params: Value) -> Option<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let response = post(port, "/", &body)?;
    let value: Value = serde_json::from_str(&response).ok()?;
    Some(value)
}

fn rpc(port: u16, method: &str, params: Value) -> Value {
    let response = try_rpc(port, method, params.clone())
        .unwrap_or_else(|| panic!("{method} on :{port} got no answer"));
    assert!(
        response.get("error").is_none(),
        "{method}{params}: {response}"
    );
    response["result"].clone()
}

/// POST `body` and return the response body.
fn post(port: u16, path: &str, body: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.1 200").then(|| body.to_string())
}

fn balance(port: u16, address: &str) -> String {
    rpc(port, "eth_getBalance", json!([address, "latest"]))
        .as_str()
        .unwrap()
        .to_string()
}

/// `balanceOf(holder)` on the test token, as a decimal.
fn token_balance(port: u16, holder: &str) -> u128 {
    let data = format!("0x70a08231{:0>64}", holder.trim_start_matches("0x"));
    let result = rpc(
        port,
        "eth_call",
        json!([{"to": TOKEN, "data": data}, "latest"]),
    );
    let hex = result.as_str().unwrap().trim_start_matches("0x");
    u128::from_str_radix(&hex[hex.len() - 32..], 16).unwrap()
}

/// A child that is killed when the test ends, pass or fail.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A plain anvil standing in for a real chain's endpoint.
struct Upstream {
    _anvil: Guard,
    port: u16,
}

impl Upstream {
    fn start() -> Self {
        let port = free_port();
        let anvil = Command::new("anvil")
            .args(["--port", &port.to_string(), "--chain-id", "31337"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let upstream = Upstream {
            _anvil: Guard(anvil),
            port,
        };
        wait_for("the upstream anvil", Duration::from_secs(30), || {
            try_rpc(port, "eth_chainId", json!([])).is_some()
        });
        upstream
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// State only the upstream knows, plus a few blocks of history.
    fn seed_state(&self) {
        let ether7 = "0x6124fee993bc0000";
        rpc(
            self.port,
            "anvil_setBalance",
            json!([UPSTREAM_ONLY, ether7]),
        );
        rpc(self.port, "anvil_setCode", json!([TOKEN, TOKEN_CODE]));
        rpc(
            self.port,
            "anvil_setStorageAt",
            json!([TOKEN, DECIMALS_SLOT, format!("0x{:064x}", 6)]),
        );
        rpc(self.port, "anvil_mine", json!(["0x3"]));
    }
}

/// A foreground `chainz fork` supervisor and what it printed.
struct Fork {
    child: Guard,
    out: Arc<Mutex<String>>,
}

impl Fork {
    fn start(sb: &Sandbox, args: &[&str]) -> Self {
        let mut child = sb
            .cmd()
            .arg("fork")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = Arc::new(Mutex::new(String::new()));
        for stream in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let out = out.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let mut out = out.lock().unwrap();
                    out.push_str(&line);
                    out.push('\n');
                }
            });
        }
        Fork {
            child: Guard(child),
            out,
        }
    }

    fn pid(&self) -> u32 {
        self.child.0.id()
    }

    fn output(&self) -> String {
        self.out.lock().unwrap().clone()
    }

    /// Wait until the supervisor has printed `text`.
    fn wait_for_output(&mut self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let out = self.output();
            if out.contains(text) {
                return out;
            }
            if let Some(status) = self.child.0.try_wait().unwrap() {
                std::thread::sleep(Duration::from_millis(200));
                panic!("chainz fork exited ({status}):\n{}", self.output());
            }
            assert!(Instant::now() < deadline, "no '{text}' in:\n{out}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait for the supervisor to exit and return its code.
    fn wait_exit(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.0.try_wait().unwrap() {
                return status.code();
            }
            assert!(Instant::now() < deadline, "the supervisor did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

// ------------------------------------------------------------ with anvil

#[test]
fn fork_through_proxy_registers_funds_lists_and_stops() {
    if !anvil_available() {
        return;
    }
    let upstream = Upstream::start();
    upstream.seed_state();
    let sb = Sandbox::new();
    sb.seed_local(&upstream.url());
    let port = free_port();

    let mut fork = Fork::start(
        &sb,
        &[
            "local",
            "--port",
            &port.to_string(),
            "--fund",
            "tok=1.5,nocode=3",
        ],
    );
    let out = fork.wait_for_output("ctrl-c to stop");

    // The summary and what was funded; a token that can't be dealt is
    // reported and does not stop the rest.
    assert!(
        out.contains(&format!(
            "local-fork  http://127.0.0.1:{port}  block 3  chain 31337"
        )),
        "{out}"
    );
    assert!(
        out.contains("funded 0x0000…2345 (default): 1000 ETH, 1.5 TOK"),
        "{out}"
    );
    assert!(out.contains("could not fund 3 NOCODE"), "{out}");
    assert!(
        out.contains("upstream local/up (pinned, via proxy)"),
        "{out}"
    );

    // Registered, with the supervisor's PID.
    let record = sb.record("local-fork").expect("registered");
    assert_eq!(record["pid"], fork.pid());
    assert_eq!(record["port"], port);
    assert_eq!(record["parent"], "local");
    assert_eq!(record["chain_id"], 31337);
    assert_eq!(record["fork_block"], Value::Null);

    // Anvil forks from the proxy, not from the endpoint: its argv holds a
    // loopback URL with a token, never the upstream's.
    let anvil = sb.anvil_pid("local-fork");
    let args = process_args(anvil);
    assert!(args.contains("--auto-impersonate"), "{args}");
    assert!(args.contains(&format!("--port {port}")), "{args}");
    assert!(args.contains("--fork-url http://127.0.0.1:"), "{args}");
    assert!(!args.contains(&upstream.port.to_string()), "{args}");

    // Upstream state is visible through the proxy; funding landed.
    assert_eq!(balance(port, UPSTREAM_ONLY), "0x6124fee993bc0000");
    assert_eq!(balance(port, TEST_ADDRESS), "0x3635c9adc5dea00000");
    assert_eq!(token_balance(port, TEST_ADDRESS), 1_500_000);
    // …and only on the fork.
    assert_eq!(token_balance(upstream.port, TEST_ADDRESS), 0);

    // `fork ls`.
    let ls = sb.run(&["fork", "ls"]);
    let listing = stdout(&ls);
    assert!(ls.status.success(), "{}", stderr(&ls));
    let row = listing.lines().nth(1).expect("one fork listed");
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(cells[0], "local-fork");
    assert_eq!(cells[1], format!("http://127.0.0.1:{port}"));
    assert_eq!(cells[2], "local");
    assert_eq!(cells[3], "3", "block height: {listing}");
    assert_eq!(cells.last().unwrap(), &fork.pid().to_string());
    assert!(listing.starts_with("NAME"), "{listing}");

    // The fork is a chain name; the chain ID still means the real chain.
    let explain = sb.run(&["exec", "local-fork", "--explain", "--", "true"]);
    let explained = stdout(&explain);
    assert!(
        explained.contains(&format!("ETH_RPC_URL=http://127.0.0.1:{port}")),
        "{explained}"
    );
    assert!(explained.contains("CHAINZ_CHAIN=local-fork"), "{explained}");
    let by_id = stdout(&sb.run(&["exec", "31337", "--explain", "--", "true"]));
    assert!(by_id.contains("CHAINZ_CHAIN=local\n"), "{by_id}");
    assert!(
        by_id.contains(&format!("ETH_RPC_URL=http://127.0.0.1:{}", upstream.port)),
        "{by_id}"
    );

    // Unlocked sends from the real address work with no key anywhere.
    let sent = rpc(
        port,
        "eth_sendTransaction",
        json!([{"from": TEST_ADDRESS, "to": UPSTREAM_ONLY, "value": "0xde0b6b3a7640000"}]),
    );
    assert!(sent.as_str().is_some_and(|h| h.len() == 66), "{sent}");
    assert_eq!(balance(port, UPSTREAM_ONLY), "0x6f05b59d3b200000");

    // The name is taken while the fork lives.
    let again = sb.run(&["fork", "local"]);
    assert!(!again.status.success());
    assert!(stderr(&again).contains("already running"), "{again:?}");

    // `fork stop` ends the supervisor and anvil and unregisters the fork.
    let stop = sb.run(&["fork", "stop", "local-fork"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert_eq!(stdout(&stop), "stopped local-fork\n");
    assert_eq!(fork.wait_exit(), Some(0));
    assert!(!pid_running(anvil), "anvil survived fork stop");
    assert!(sb.registry_is_empty());
    assert!(stdout(&sb.run(&["fork", "ls"])).contains("no forks running"));
    let gone = sb.run(&["exec", "local-fork", "--explain", "--", "true"]);
    assert!(!gone.status.success());
}

#[test]
fn ctrl_c_and_sigterm_stop_anvil_and_unregister() {
    if !anvil_available() {
        return;
    }
    let upstream = Upstream::start();
    let sb = Sandbox::new();
    sb.seed_local(&upstream.url());
    for signal in ["INT", "TERM"] {
        let port = free_port();
        let mut fork = Fork::start(&sb, &["local", "--port", &port.to_string(), "--no-fund"]);
        let out = fork.wait_for_output("ctrl-c to stop");
        assert!(!out.contains("funded"), "{out}");
        assert_eq!(balance(port, TEST_ADDRESS), "0x0", "--no-fund");
        let anvil = sb.anvil_pid("local-fork");
        assert!(pid_running(anvil));

        send_signal(fork.pid(), signal);
        assert_eq!(fork.wait_exit(), Some(0), "SIG{signal}");
        assert!(!pid_running(anvil), "anvil survived SIG{signal}");
        assert!(sb.registry_is_empty(), "SIG{signal}");
        assert!(try_rpc(port, "eth_chainId", json!([])).is_none());
    }
}

/// `PR_SET_PDEATHSIG`: anvil can't outlive a supervisor that had no
/// chance to clean up; the next `fork ls` drops the stale record.
#[cfg(target_os = "linux")]
#[test]
fn kill_9_of_the_supervisor_leaves_no_anvil() {
    if !anvil_available() {
        return;
    }
    let upstream = Upstream::start();
    let sb = Sandbox::new();
    sb.seed_local(&upstream.url());
    let port = free_port();
    let mut fork = Fork::start(&sb, &["local", "--port", &port.to_string()]);
    fork.wait_for_output("ctrl-c to stop");
    let anvil = sb.anvil_pid("local-fork");
    assert!(pid_running(anvil));

    send_signal(fork.pid(), "KILL");
    assert_eq!(fork.wait_exit(), None, "killed by a signal");
    wait_for(
        "anvil to die with its supervisor",
        Duration::from_secs(10),
        || !pid_running(anvil),
    );
    assert!(sb.record("local-fork").is_some(), "nobody cleaned up yet");

    let ls = sb.run(&["fork", "ls"]);
    assert!(stdout(&ls).contains("no forks running"), "{ls:?}");
    assert!(sb.registry_is_empty());
    assert!(
        Command::new("pgrep")
            .args(["-f", &format!("anvil.*--port {port}")])
            .output()
            .is_ok_and(|o| o.stdout.is_empty()),
        "an anvil is still running on port {port}"
    );
}

/// What macOS relies on (no `PDEATHSIG`): a record whose supervisor is
/// dead is reaped together with its orphaned anvil, by `fork ls` and by
/// the next `fork`.
#[test]
fn orphaned_anvils_of_dead_supervisors_are_reaped() {
    if !anvil_available() {
        return;
    }
    let sb = Sandbox::new();
    sb.seed_local("http://127.0.0.1:1");
    let mut dead = Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();

    for reaper in [vec!["fork", "ls"], vec!["fork", "local"]] {
        let port = free_port();
        let mut orphan = Guard(
            Command::new("anvil")
                .args(["--port", &port.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait_for("the orphan anvil", Duration::from_secs(30), || {
            try_rpc(port, "eth_chainId", json!([])).is_some()
        });
        fs::create_dir_all(sb.forks_dir()).unwrap();
        fs::write(
            sb.forks_dir().join("old-fork.json"),
            json!({"name": "old-fork", "pid": dead_pid, "port": port, "parent": "local",
                   "chain_id": 31337, "fork_block": null, "started_at": 0})
            .to_string(),
        )
        .unwrap();
        fs::write(
            sb.forks_dir().join("old-fork.pids"),
            json!({"supervisor": dead_pid, "anvil": orphan.0.id(), "port": port}).to_string(),
        )
        .unwrap();

        // (The second reaper then fails to fork from a dead endpoint;
        // reaping happens first.)
        sb.run(&reaper);
        assert!(sb.record("old-fork").is_none(), "{reaper:?}");
        assert!(sb.registry_is_empty(), "{reaper:?}");
        wait_for("the orphan to be killed", Duration::from_secs(5), || {
            orphan.0.try_wait().unwrap().is_some()
        });
    }
}

#[test]
fn detach_returns_once_registered_and_stop_all_stops_it() {
    if !anvil_available() {
        return;
    }
    let upstream = Upstream::start();
    let sb = Sandbox::new();
    sb.seed_local(&upstream.url());
    let port = free_port();

    let started = sb.run(&[
        "fork",
        "local",
        "--detach",
        "--name",
        "scratch",
        "--chain-id",
        "99",
        "--port",
        &port.to_string(),
    ]);
    assert!(started.status.success(), "{}", stderr(&started));
    let out = stdout(&started);
    assert!(
        out.contains(&format!(
            "scratch  http://127.0.0.1:{port}  block 0  chain 99"
        )),
        "{out}"
    );
    assert!(
        out.contains("funded 0x0000…2345 (default): 1000 ETH"),
        "{out}"
    );
    assert!(stderr(&started).contains("chainz fork stop scratch"));

    // Registered by a supervisor that outlived the command.
    let record = sb.record("scratch").expect("registered on return");
    let supervisor = record["pid"].as_u64().unwrap() as u32;
    assert_eq!(record["chain_id"], 99);
    assert!(pid_running(supervisor));
    let anvil = sb.anvil_pid("scratch");
    assert_eq!(
        rpc(port, "eth_chainId", json!([])),
        "0x63",
        "--chain-id reaches anvil"
    );
    assert_eq!(balance(port, TEST_ADDRESS), "0x3635c9adc5dea00000");
    let log = sb.root.path().join("state/chainz/forks/scratch.log");
    assert!(
        fs::read_to_string(&log).unwrap().contains("Listening on"),
        "anvil's output is in the log"
    );

    let stop = sb.run(&["fork", "stop", "--all"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert_eq!(stdout(&stop), "stopped scratch\n");
    assert!(!pid_running(supervisor) && !pid_running(anvil));
    assert!(sb.registry_is_empty());

    // A detached fork that can't start reports why and leaves nothing.
    let sb = Sandbox::new();
    sb.seed_local("http://127.0.0.1:1");
    let failed = sb.run(&["fork", "local", "--detach"]);
    assert!(!failed.status.success());
    assert!(stderr(&failed).contains("can't fork 'local'"), "{failed:?}");
    assert!(sb.registry_is_empty());
}

#[test]
fn block_needs_an_archive_endpoint() {
    if !anvil_available() {
        return;
    }
    // At genesis the upstream has no state at block 1: not archive.
    let upstream = Upstream::start();
    let sb = Sandbox::new();
    sb.seed_local(&upstream.url());
    let refused = sb.run(&["fork", "local", "--block", "0"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("needs an archive endpoint"),
        "{refused:?}"
    );
    assert!(sb.registry_is_empty());

    // With history it qualifies, and the fork is pinned to the block.
    upstream.seed_state();
    let port = free_port();
    let mut fork = Fork::start(&sb, &["local", "--block", "1", "--port", &port.to_string()]);
    let out = fork.wait_for_output("ctrl-c to stop");
    assert!(out.contains("block 1  chain 31337"), "{out}");
    assert_eq!(sb.record("local-fork").unwrap()["fork_block"], 1);
    assert!(
        process_args(sb.anvil_pid("local-fork")).contains("--fork-block-number 1"),
        "{out}"
    );
    send_signal(fork.pid(), "TERM");
    assert_eq!(fork.wait_exit(), Some(0));
}

/// With a gateway running, the fork's proxy forwards to the gateway's URL
/// pinned to the chosen endpoint, and the gateway token stays in the
/// supervisor's memory.
#[test]
fn a_running_gateway_is_used_pinned_to_the_endpoint() {
    if !anvil_available() {
        return;
    }
    let upstream = Upstream::start();
    let sb = Sandbox::new();
    sb.seed_local(&upstream.url());

    // A stand-in gateway: forwards every POST to the upstream and records
    // the paths it was asked for.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway_port = listener.local_addr().unwrap().port();
    let paths = Arc::new(Mutex::new(Vec::<String>::new()));
    let (seen, upstream_port) = (paths.clone(), upstream.port);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = seen.clone();
            std::thread::spawn(move || relay(stream, upstream_port, &seen));
        }
    });
    let runtime = sb.root.path().join("runtime/chainz");
    fs::create_dir_all(&runtime).unwrap();
    let lock = fs::File::create(runtime.join("serve.lock")).unwrap();
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a file we own; held until `lock` drops.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }
    fs::write(
        runtime.join("serve.json"),
        json!({"pid": std::process::id(), "port": gateway_port, "token": "gwtoken",
               "started_at": 0, "version": "test"})
        .to_string(),
    )
    .unwrap();

    let port = free_port();
    let mut fork = Fork::start(&sb, &["local", "--port", &port.to_string()]);
    let out = fork.wait_for_output("ctrl-c to stop");
    assert!(
        out.contains("upstream local/up (pinned, via gateway)"),
        "{out}"
    );
    // Anvil still forks from the supervisor's proxy: the gateway token
    // (which grants every chain) must never reach argv, readable by any
    // local user. Only the proxy talks to the gateway.
    let anvil = sb.anvil_pid("local-fork");
    let args = process_args(anvil);
    assert!(args.contains("--fork-url http://127.0.0.1:"), "{args}");
    assert!(!args.contains("gwtoken"), "gateway token in argv: {args}");
    assert!(!args.contains(&gateway_port.to_string()), "{args}");
    #[cfg(target_os = "linux")]
    {
        let cmdline = fs::read(format!("/proc/{anvil}/cmdline")).unwrap();
        let environ = fs::read(format!("/proc/{anvil}/environ")).unwrap();
        for bytes in [cmdline, environ] {
            assert!(!String::from_utf8_lossy(&bytes).contains("gwtoken"));
        }
    }
    assert!(!out.contains("gwtoken"), "{out}");
    let log = fs::read_to_string(sb.root.path().join("state/chainz/forks/local-fork.log"));
    assert!(!log.unwrap().contains("gwtoken"));
    assert_eq!(balance(port, TEST_ADDRESS), "0x3635c9adc5dea00000");
    let paths = paths.lock().unwrap().clone();
    assert!(!paths.is_empty());
    assert!(paths.iter().all(|p| p == "/gwtoken/local/up"), "{paths:?}");
    send_signal(fork.pid(), "INT");
    assert_eq!(fork.wait_exit(), Some(0));
    drop(lock);
}

/// Forward one request to the upstream anvil, whatever its path.
fn relay(stream: TcpStream, upstream_port: u16, seen: &Mutex<Vec<String>>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let path = line.split(' ').nth(1).unwrap_or_default().to_string();
    let mut length = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    seen.lock().unwrap().push(path);
    let answer = post(upstream_port, "/", &String::from_utf8_lossy(&body)).unwrap_or_default();
    let _ = reader.get_mut().write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
            answer.len()
        )
        .as_bytes(),
    );
}

// --------------------------------------------------------- without anvil

#[test]
fn rejects_unforkable_chains_bad_names_and_bad_fund_specs_up_front() {
    let sb = Sandbox::new();
    sb.seed_local("http://127.0.0.1:1");
    let fails = |args: &[&str], expected: &str| {
        let output = sb.run(args);
        assert!(!output.status.success(), "{args:?}");
        let err = stderr(&output);
        assert!(err.contains(expected), "{args:?}: {err}");
    };
    fails(&["fork", "zk"], "anvil does not support zkSync Era");
    fails(&["fork", "324"], "anvil does not support zkSync Era");
    fails(&["fork", "nope"], "not found");
    // `ls` and `stop` are subcommands, so they can't be fork names.
    fails(&["fork", "local", "--name", "ls"], "subcommand");
    fails(&["fork", "local", "--name", "stop"], "subcommand");
    fails(&["fork", "local", "--name", "local"], "configured chain");
    fails(&["fork", "local", "--name", "devnet"], "alias of");
    fails(&["fork", "local", "--name", "31337"], "invalid fork name");
    fails(
        &["fork", "local", "--name", "Has Space"],
        "invalid fork name",
    );
    fails(
        &["fork", "local", "--fund", "udsc=1"],
        "unknown label @udsc",
    );
    fails(&["fork", "local", "--fund", "tok"], "not NAME=AMOUNT");
    fails(
        &["fork", "local", "--fund", "tok=1e6"],
        "not a decimal amount",
    );
    fails(
        &["fork", "local", "--fund", "usdc=1"],
        "@usdc has no address on chain 31337",
    );
    fails(&["fork", "zk", "--no-fund"], "zkSync Era");
    fails(&["fork", "local", "--port", "0"], "--port 0");
    // A dead endpoint: nothing starts, nothing is left behind.
    fails(&["fork", "local"], "can't fork 'local' from endpoint 'up'");
    assert!(sb.registry_is_empty());
    assert!(!sb.root.path().join("config/chainz/secrets.toml").exists());
}

#[test]
fn ls_and_stop_with_no_forks() {
    let sb = Sandbox::new();
    sb.seed_local("http://127.0.0.1:1");
    let ls = sb.run(&["fork", "ls"]);
    assert!(ls.status.success());
    assert!(stdout(&ls).contains("no forks running"));
    let all = sb.run(&["fork", "stop", "--all"]);
    assert!(all.status.success());
    assert!(stdout(&all).contains("no forks running"));
    let named = sb.run(&["fork", "stop", "base-fork"]);
    assert!(!named.status.success());
    assert!(stderr(&named).contains("no fork named 'base-fork'"));
    let neither = sb.run(&["fork", "stop"]);
    assert!(!neither.status.success());
    assert!(stderr(&neither).contains("--all"));
}

/// Replay guards: a fork shares the real chain ID, so no real signature
/// may be produced for it. The record names this test process as the
/// supervisor; exec never checks liveness.
#[test]
fn replay_guards_on_a_fork_context() {
    let sb = Sandbox::new();
    sb.seed_local("http://127.0.0.1:1");
    fs::create_dir_all(sb.forks_dir()).unwrap();
    fs::write(
        sb.forks_dir().join("local-fork.json"),
        json!({"name": "local-fork", "pid": std::process::id(), "port": 8601,
               "parent": "local", "chain_id": 31337, "fork_block": null, "started_at": 0})
        .to_string(),
    )
    .unwrap();

    // --expose-key is refused, with or without --explain.
    for extra in [&["--expose-key"][..], &["--expose-key", "--explain"][..]] {
        let output = sb
            .cmd()
            .args(["exec", "local-fork"])
            .args(extra)
            .args(["--", "sh", "-c", "echo ran"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("--expose-key is refused on a fork"),
            "{output:?}"
        );
        assert!(!stdout(&output).contains("ran"));
    }

    // A keystore signer exports ETH_KEYSTORE on the real chain only.
    let show = "echo url=$ETH_RPC_URL keystore=${ETH_KEYSTORE:-unset} key=${RAW_PRIVATE_KEY:-unset} from=$ETH_FROM chain=$CHAINZ_CHAIN id=$CHAIN_ID";
    let on_fork = sb
        .cmd()
        .args([
            "exec",
            "local-fork",
            "--signer",
            "ks",
            "--",
            "sh",
            "-c",
            show,
        ])
        .env("ETH_KEYSTORE", "/leaked/from/the/outer/shell")
        .env("RAW_PRIVATE_KEY", "leaked")
        .output()
        .unwrap();
    assert_eq!(
        stdout(&on_fork).trim(),
        format!(
            "url=http://127.0.0.1:8601 keystore=unset key=unset from={TEST_ADDRESS} chain=local-fork id=31337"
        )
    );
    let on_chain = sb.run(&["exec", "local", "--signer", "ks", "--", "sh", "-c", show]);
    assert!(
        stdout(&on_chain).contains("keystore=/keys/ks"),
        "{on_chain:?}"
    );
    let env = sb.run(&["env", "local-fork", "--signer", "ks", "--shell", "bash"]);
    assert!(stdout(&env).contains("unset ETH_KEYSTORE;"), "{env:?}");
    assert!(!stdout(&env).contains("/keys/ks"), "{env:?}");

    // The chain ID, the chain's name and its alias never mean the fork.
    for query in ["31337", "local", "devnet"] {
        let explained = stdout(&sb.run(&["exec", query, "--explain", "--", "true"]));
        assert!(
            explained.contains("ETH_RPC_URL=http://127.0.0.1:1/\n"),
            "{query}: {explained}"
        );
        assert!(explained.contains("CHAINZ_CHAIN=local\n"), "{query}");
    }
}
