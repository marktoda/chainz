//! End-to-end tests for `chainz top --once`.
//!
//! Every invocation is sandboxed: `HOME` and all four XDG directories point
//! into a fresh temp dir and `CHAINZ_DISABLE_KEYRING` is set. The RPC is a
//! mock on a loopback port; nothing here touches the network.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

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

    fn write(&self, name: &str, text: &str) {
        let path = self.root.path().join("config/chainz").join(name);
        fs::write(&path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    /// `live` on the mock (through a `${VAR}` in the path), `dead` on a
    /// closed port, and a keyring signer with a cached address.
    fn seed(&self, rpc: &str) {
        self.write(
            "config.toml",
            &format!(
                r#"version = 2
default = "live"

[chains.live]
id = 1
aliases = ["mainnet"]
signer = "default"
rpc = "main"

[chains.live.rpcs]
main = "{rpc}/v3/${{RPC_KEY}}"
spare = "http://127.0.0.1:1/spare"
stream = "wss://live.example.org/ws/hunter2"

[chains.dead]
id = 8453
rpc = "local"

[chains.dead.rpcs]
local = "http://127.0.0.1:1"

[signers.default]
backend = "keyring"
address = "{ADDRESS}"
"#
            ),
        );
        self.write("secrets.toml", "[vars]\nRPC_KEY = \"s3cr3t-key\"\n");
    }

    fn top(&self, args: &[&str]) -> Command {
        let r = self.root.path();
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("chainz"));
        for name in ["CHAINZ_CHAIN", "ETH_RPC_URL", "RPC_KEY"] {
            cmd.env_remove(name);
        }
        cmd.env("HOME", r.join("home"))
            .env("XDG_CONFIG_HOME", r.join("config"))
            .env("XDG_STATE_HOME", r.join("state"))
            .env("XDG_CACHE_HOME", r.join("cache"))
            .env("XDG_RUNTIME_DIR", r.join("runtime"))
            .env("CHAINZ_DISABLE_KEYRING", "1")
            .env("NO_COLOR", "1")
            .arg("top")
            .args(args);
        cmd
    }
}

/// The `(path, method)` of every request a mock received.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

fn word(n: u128) -> String {
    format!("{n:064x}")
}

/// `(bool, bytes)[]` of successful `uint256` returns, as `aggregate3` gives.
fn aggregate3_result(values: &[u128]) -> String {
    let mut out = word(32) + &word(values.len() as u128);
    for i in 0..values.len() {
        out += &word((values.len() + i * 4) as u128 * 32);
    }
    for value in values {
        out += &(word(1) + &word(64) + &word(32) + &word(*value));
    }
    format!("0x{out}")
}

/// Mainnet USDC, lower case and without `0x`.
const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

/// The target of every call in `aggregate3` calldata, lower-case hex.
fn aggregate3_targets(calldata: &str) -> Vec<String> {
    let args = &calldata[10..]; // after "0x" and the selector
    let word = |i: usize| &args[i * 64..(i + 1) * 64];
    let number = |i: usize| usize::from_str_radix(&word(i)[48..], 16).unwrap();
    let count = number(1);
    (0..count)
        .map(|i| word(2 + number(2 + i) / 32)[24..].to_lowercase())
        .collect()
}

/// A mock chain 1: head 1,000 with 12 s blocks, and a Multicall3 that
/// reports 1.5 ETH, 2.5 USDC and nothing of any other token. Returns its
/// URL and the request paths and methods it saw.
fn mock_rpc() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            let method = request["method"].as_str().unwrap_or("batch").to_string();
            let path = request_line.split_whitespace().nth(1).unwrap().to_string();
            let result = match method.as_str() {
                "eth_getBlockByNumber" => {
                    // The head is always fresh: its time is the request's.
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let tag = request["params"][0].as_str().unwrap();
                    let number = match tag {
                        "latest" => 1_000,
                        hex => u64::from_str_radix(&hex[2..], 16).unwrap(),
                    };
                    json!({
                        "number": format!("{number:#x}"),
                        "timestamp": format!("{:#x}", now - (1_000 - number) * 12),
                        "baseFeePerGas": "0x3b9aca00",
                    })
                }
                "eth_call" => {
                    let calldata = request["params"][0]["data"].as_str().unwrap();
                    let values: Vec<u128> = aggregate3_targets(calldata)
                        .iter()
                        .enumerate()
                        .map(|(i, target)| match (i, target.as_str()) {
                            (0, _) => 1_500_000_000_000_000_000,
                            (_, USDC) => 2_500_000,
                            _ => 0,
                        })
                        .collect();
                    json!(aggregate3_result(&values))
                }
                _ => Value::Null,
            };
            log.lock().unwrap().push((path, method));
            let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
            let reply = reply.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
        }
    });
    (url, seen)
}

#[test]
fn once_json_prints_one_snapshot() {
    let sandbox = Sandbox::new();
    let (rpc, seen) = mock_rpc();
    sandbox.seed(&rpc);
    let output = sandbox.top(&["--once", "--json"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let snapshot: Value = serde_json::from_str(&stdout).expect("stdout is one JSON document");

    assert_eq!(snapshot["gateway"], json!({"running": false, "port": null}));
    assert_eq!(snapshot["traffic"], Value::Null);

    let live = &snapshot["chains"][0];
    assert_eq!(live["name"], "live");
    assert_eq!(live["id"], 1);
    assert_eq!(live["aliases"], json!(["mainnet"]));
    assert_eq!(live["rpc"], "main");
    assert_eq!(live["status"], "ok");
    assert_eq!(live["head"], 1_000);
    assert_eq!(live["block_time_secs"], 12.0);
    assert_eq!(live["base_fee_wei"], "1000000000");
    assert_eq!(live["base_fee_gwei"], 1.0);
    assert_eq!(live["via_gateway"], false);
    assert_eq!(live["error"], Value::Null);
    assert!(live["latency_ms"].is_u64() && live["block_age_secs"].as_u64().unwrap() < 5);

    // Endpoints are listed with their last known health; `--once` does
    // not probe them.
    let endpoints = live["endpoints"].as_array().unwrap();
    let names: Vec<&str> = endpoints
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["main", "spare", "stream"]);
    assert_eq!(endpoints[0]["active"], true);
    assert_eq!(endpoints[0]["status"], "unknown");
    assert_eq!(endpoints[2]["status"], "websocket");
    assert_eq!(endpoints[2]["tags"], json!(["ws"]));

    // A dead chain is reported, not fatal.
    let dead = &snapshot["chains"][1];
    assert_eq!(dead["status"], "down");
    assert_eq!(dead["head"], Value::Null);
    assert_eq!(dead["error"]["class"], "connect");

    // Balances: the cached address, one Multicall3 call, registry decimals.
    let balances = &snapshot["balances"][0];
    assert_eq!(balances["signer"], "default");
    assert_eq!(balances["address"], ADDRESS);
    let on_live = &balances["chains"][0];
    assert_eq!(on_live["via"], "multicall3");
    assert_eq!(
        on_live["native"],
        json!({"symbol": "ETH", "decimals": 18, "raw": "1500000000000000000", "formatted": "1.5", "saturated": false})
    );
    let token = |name: &str| {
        let tokens = on_live["tokens"].as_array().unwrap();
        tokens.iter().find(|t| t["name"] == name).unwrap().clone()
    };
    assert_eq!(token("usdc")["balance"]["formatted"], "2.5");
    assert_eq!(token("usdc")["balance"]["decimals"], 6);
    assert_eq!(token("weth")["balance"]["formatted"], "0");
    assert_eq!(balances["chains"][1]["error"]["class"], "connect");
    assert_eq!(balances["chains"][1]["native"], Value::Null);

    // Secrets: the variable was expanded for the request and nowhere else.
    assert!(
        !stdout.contains("s3cr3t-key") && !stdout.contains("hunter2"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("RPC_KEY}") || stdout.contains("REDACTED"),
        "{stdout}"
    );
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().all(|(path, _)| path == "/v3/s3cr3t-key"),
        "{seen:?}"
    );
    // The traffic budget: a head, one look-back block, one multicall.
    let methods: Vec<&str> = seen.iter().map(|(_, m)| m.as_str()).collect();
    assert_eq!(
        methods,
        ["eth_getBlockByNumber", "eth_getBlockByNumber", "eth_call"]
    );
}

#[test]
fn reveal_controls_url_redaction() {
    let sandbox = Sandbox::new();
    let (rpc, _seen) = mock_rpc();
    sandbox.seed(&rpc);
    let url_of = |args: &[&str], index: usize| -> String {
        let output = sandbox.top(args).output().unwrap();
        assert!(output.status.success());
        let snapshot: Value = serde_json::from_slice(&output.stdout).unwrap();
        snapshot["chains"][0]["endpoints"][index]["url"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let redacted = url_of(&["--once", "--json"], 2);
    assert_eq!(redacted, "wss://redacted.example.org/REDACTED");
    // --reveal shows the config value: templates stay unexpanded.
    assert_eq!(
        url_of(&["--once", "--json", "--reveal"], 2),
        "wss://live.example.org/ws/hunter2"
    );
    let revealed = url_of(&["--once", "--json", "--reveal"], 0);
    assert_eq!(revealed, format!("{rpc}/v3/${{RPC_KEY}}"));
}

#[test]
fn once_without_json_prints_the_tables() {
    let sandbox = Sandbox::new();
    let (rpc, _seen) = mock_rpc();
    sandbox.seed(&rpc);
    let output = sandbox.top(&["--once"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], " 2 chains · 1 ok · 1 down");
    let cells = |line: &str| {
        line.split_whitespace()
            .map(String::from)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        cells(lines[1]),
        [
            "CHAIN", "ID", "HEAD", "AGE", "BLOCK", "BASE", "FEE", "RPC", "P50", "HEALTH", "STATUS"
        ]
    );
    let live = cells(lines[2]);
    // The age is a second or two at most, depending on where the second
    // boundaries fall; its shape is what matters.
    let age = &live[3];
    assert!(
        age.strip_suffix('s')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        "AGE {age}"
    );
    assert_eq!(live[..3], ["live", "1", "1,000"]);
    assert_eq!(live[4..8], ["12.0s", "1.00", "gwei", "main"]);
    assert!(
        lines[2].ends_with("ok") && lines[3].ends_with("down connect"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(" signer default {ADDRESS}")),
        "{stdout}"
    );
    let balances = lines.iter().find(|l| l.contains("multicall")).unwrap();
    assert_eq!(
        cells(balances)[..5],
        ["live", "1.5000", "ETH", "0", "2.5000"],
        "{stdout}"
    );
    // Plain text: no escape sequences, no selection marker, no key hints.
    assert!(!stdout.contains('\x1b') && !stdout.contains('▸') && !stdout.contains("quit"));
}

#[test]
fn once_with_no_chains_is_an_empty_snapshot() {
    let sandbox = Sandbox::new();
    let output = sandbox.top(&["--once", "--json"]).output().unwrap();
    assert!(output.status.success());
    let snapshot: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(snapshot["chains"], json!([]));
    assert_eq!(snapshot["balances"], json!([]));
    let text = sandbox.top(&["--once"]).output().unwrap();
    assert!(String::from_utf8_lossy(&text.stdout).contains("No chains configured yet."));
}

#[test]
fn the_live_view_needs_a_terminal_and_json_needs_once() {
    let sandbox = Sandbox::new();
    sandbox.seed("http://127.0.0.1:1");
    // Under the test harness stdout is a pipe: refuse, and say what to use.
    sandbox
        .top(&[])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("needs a terminal"))
        .stderr(predicates::str::contains("chainz top --once --json"));
    sandbox
        .top(&["--json"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("`--json` needs `--once`"));
}
