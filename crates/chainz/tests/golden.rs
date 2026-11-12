//! Golden baselines: the exact output of the listing commands, of config
//! edits on a commented config, of the v1 migration, of `top --once` and
//! the key shape of `serve status --json`, pinned before the untangle
//! refactor (plan D6) so every later change to them shows up as a diff.
//!
//! Snapshots are plain files under `tests/golden/`. When a change to one is
//! intended, regenerate them and review the diff:
//!
//! ```console
//! CHAINZ_BLESS=1 cargo test -p chainz --test golden
//! git diff crates/chainz/tests/golden
//! ```
//!
//! Every invocation runs in a sandbox (see `common`); RPCs are mocks on
//! loopback and nothing here touches the network.

mod common;

use common::{MockRpc, Sandbox, TEST_KEY, stderr, stdout, wait_until};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// Compare `actual` with `tests/golden/<name>`, or rewrite the file when
/// `CHAINZ_BLESS=1`.
fn golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("CHAINZ_BLESS").is_some_and(|v| v == "1") {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run with CHAINZ_BLESS=1 to create it",
            path.display()
        )
    });
    if expected != actual {
        let first = expected
            .lines()
            .zip(actual.lines())
            .position(|(e, a)| e != a)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        let window = |text: &str| -> String {
            text.lines()
                .enumerate()
                .skip(first.saturating_sub(3))
                .take(7)
                .map(|(i, l)| format!("{:>5} | {l}\n", i + 1))
                .collect()
        };
        panic!(
            "{name} differs from {} at line {} (CHAINZ_BLESS=1 rewrites it)\n--- expected\n{}--- actual\n{}",
            path.display(),
            first + 1,
            window(&expected),
            window(actual)
        );
    }
}

// ---------------------------------------------------------------- listings

/// A credential-bearing literal URL, a `${VAR}` URL, a header endpoint
/// (literal and templated values), public and websocket URLs, aliases, a
/// failover list, an explorer, labels (per chain and `*`), and a plaintext
/// and a keystore signer.
const LISTING_CONFIG: &str = r#"# Golden fixture: every row kind the listings render.
version = 2
default = "eth"

# Defined once, attached per chain.
[explorers.etherscan]
api = "https://api.etherscan.io/v2/api?chainid=${CHAIN_ID}"
key = "${ETHERSCAN_API_KEY}"

[chains.eth]
id = 1
aliases = ["ethereum", "mainnet"]
signer = "deployer"
explorer = "etherscan"
rpc = "infura"
failover = ["llama", "gw"]

[chains.eth.rpcs]
infura = "https://mainnet.infura.io/v3/0123456789abcdef0123456789abcdef"
alchemy = "https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}"
gw = { url = "https://gw.example.invalid/rpc/1", headers = { x-api-key = "hdr-secret-123456", x-team = "${TEAM}" } }
llama = "https://eth.llamarpc.com"
llama-ws = "wss://eth.llamarpc.com"

[chains.base]
id = 8453
signer = "default"
rpc = "public"

[chains.base.rpcs]
public = "https://mainnet.base.org"

[signers.default]
backend = "plaintext"
address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

[signers.deployer]
backend = "keystore"
path = "~/.foundry/keystores/deployer"
address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"

[addresses.myrouter]
eth = "0x1111111111111111111111111111111111111111"
base = "0x2222222222222222222222222222222222222222"

[addresses.my-safe]
"*" = "0x5afe000000000000000000000000000000005afe"
"#;

/// Health history for `rpc ls`: a healthy, tagged active endpoint and a
/// timed-out one, both checked [`PROBED_AGO_MS`] ago (`AT_MS` becomes
/// that time).
const LISTING_HEALTH: &str = r#"{"version":1,"endpoints":[
 {"chain_id":1,"endpoint":"infura","samples":[{"at_ms":AT_MS,"latency_ms":42,"head":19000000,"error":null}],
  "capabilities":{"archive":true,"debug_trace":false,"trace_block":true,"max_logs_range":10000,"websocket":false,"probed_at":1700000000}},
 {"chain_id":1,"endpoint":"gw","samples":[{"at_ms":AT_MS,"latency_ms":5000,"head":null,"error":"timeout"}]}
]}"#;

/// How long before the run the health samples were taken: `rpc ls` prints
/// `3m ago`.
const PROBED_AGO_MS: u64 = 200_000;

/// What a sample time in the listing output is pinned to.
const PINNED_AT_MS: u64 = 1_700_000_000_000;

/// The unix time in ms the listing's health samples were taken.
fn probed_at_ms() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    u64::try_from(now.as_millis()).unwrap() - PROBED_AGO_MS
}

fn listing_sandbox(probed_at_ms: u64) -> Sandbox {
    let sb = Sandbox::new();
    sb.write_config(LISTING_CONFIG);
    sb.write_secrets(&format!(
        "[vars]\nALCHEMY_KEY = \"alchemy-secret-value\"\nTEAM = \"core\"\nETHERSCAN_API_KEY = \"etherscan-secret\"\n\n[signers.default]\nprivate_key = \"{TEST_KEY}\"\n"
    ));
    fs::create_dir_all(sb.path("state/chainz")).unwrap();
    let health = LISTING_HEALTH.replace("AT_MS", &probed_at_ms.to_string());
    fs::write(sb.state_file("health.json"), health).unwrap();
    sb
}

#[test]
fn listings() {
    let at = probed_at_ms();
    let sb = listing_sandbox(at);
    for (file, args) in [
        ("ls.txt", &["ls"][..]),
        ("ls.json", &["ls", "--json"]),
        ("chain-show.txt", &["chain", "show", "eth"]),
        ("chain-show.json", &["chain", "show", "eth", "--json"]),
        ("rpc-ls.txt", &["rpc", "ls", "eth"]),
        ("rpc-ls.json", &["rpc", "ls", "eth", "--json"]),
        ("signer-ls.txt", &["signer", "ls"]),
        ("signer-ls.json", &["signer", "ls", "--json"]),
        ("var-ls.txt", &["var", "ls"]),
        ("var-ls-verbose.txt", &["var", "ls", "-v"]),
        ("var-ls.json", &["var", "ls", "--json"]),
        ("addr-ls.json", &["addr", "ls", "--json"]),
    ] {
        // The sample time is the run's own: pin it to a fixed time, so the
        // JSON goldens stay JSON.
        let out = sb
            .ok(args)
            .replace(&at.to_string(), &PINNED_AT_MS.to_string());
        golden(&format!("listing/{file}"), &out);
    }
    // Listing never writes.
    assert_eq!(sb.config(), LISTING_CONFIG);
}

// ---------------------------------------------------------------- edits

/// Comments in every position toml_edit keeps: file header, before a
/// table, trailing a value, inside an inline table's array, and between
/// tables. Keys are deliberately out of alphabetical order.
const COMMENTED_CONFIG: &str = r#"# My chains. Hand-edited; keep this header.
version = 2
default = "eth"

# Mainnet first.
[chains.eth]
rpc = "main"   # the one I trust
id = 1
aliases = ["mainnet"] # short name
signer = "default"

# Endpoints, most preferred first.
[chains.eth.rpcs]
main = "http://127.0.0.1:1/main"
backup = "http://127.0.0.1:1/backup" # flaky

[chains.base]
id = 8453
rpc = "local"
rpcs.local = "http://127.0.0.1:1/base"

# Signers go last.
[signers.default]
backend = "plaintext"
address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

# Labels.
[addresses.router]
eth = "0x1111111111111111111111111111111111111111" # v2
"#;

fn commented_sandbox() -> Sandbox {
    let sb = Sandbox::new();
    sb.write_config(COMMENTED_CONFIG);
    sb.write_secrets(&format!(
        "[signers.default]\nprivate_key = \"{TEST_KEY}\"\n"
    ));
    sb
}

#[test]
fn rpc_add_keeps_comments_and_order() {
    let sb = commented_sandbox();
    sb.ok(&[
        "rpc",
        "add",
        "eth",
        "http://127.0.0.1:1/extra",
        "--name",
        "extra",
        "--header",
        "x-team: core",
        "--failover",
        "--no-check",
    ]);
    golden("edit/rpc-add.config.toml", &sb.config());
}

#[test]
fn addr_add_keeps_comments_and_order() {
    let sb = commented_sandbox();
    sb.ok(&[
        "addr",
        "add",
        "router",
        "0x3333333333333333333333333333333333333333",
        "--chain",
        "base",
    ]);
    sb.ok(&[
        "addr",
        "add",
        "vault",
        "0x4444444444444444444444444444444444444444",
        "--all-chains",
    ]);
    golden("edit/addr-add.config.toml", &sb.config());
}

// ---------------------------------------------------------------- migration

/// The automatic migration of the v1 fixture core's migrate tests use
/// (run by the first command, here `ls`): the summary and the files
/// written.
#[test]
fn v1_fixture_migration() {
    let sb = Sandbox::new();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../chainz-core/tests/fixtures/v1-config.json");
    fs::write(sb.dir().join("config.json"), fs::read(fixture).unwrap()).unwrap();
    let root = sb.root().to_str().unwrap().to_string();
    let scrub = |text: String| text.replace(&root, "<root>");

    let real = sb.run(&["ls"]);
    assert!(real.status.success(), "{}", stderr(&real));
    golden("migrate/migrate.stderr", &scrub(stderr(&real)));
    golden("migrate/config.toml", &sb.config());
    golden("migrate/secrets.toml", &sb.secrets());
}

// ---------------------------------------------------------------- serve

/// Every key path in `value`: `a.b`, with `[]` for array elements (all
/// elements merged). Maps keyed by data (`errors`, keyed by error class)
/// are not descended into.
fn key_paths(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(_) if prefix.ends_with("errors") => {}
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                out.insert(path.clone());
                key_paths(child, &path, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                key_paths(item, &format!("{prefix}[]"), out);
            }
        }
        _ => {}
    }
}

fn shape(value: &Value) -> String {
    let mut paths = BTreeSet::new();
    key_paths(value, "", &mut paths);
    paths.into_iter().map(|p| p + "\n").collect()
}

/// The keys of `serve status --json` (values vary run to run), with
/// traffic recorded so the metrics rows are present.
#[test]
fn serve_status_json_key_shape() {
    let sb = Sandbox::new();
    let upstream = MockRpc::start(|request| {
        Ok(match request.method.as_str() {
            "eth_chainId" => json!("0x1"),
            "eth_blockNumber" => json!("0x64"),
            _ => json!("ok"),
        })
    });
    sb.write_config(&format!(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"main\"\nfailover = [\"alt\"]\n[chains.eth.rpcs]\nmain = \"{url}/main\"\nalt = \"{url}/alt\"\n",
        url = upstream.url()
    ));

    let stopped = sb.run(&["serve", "status", "--json"]);
    assert_eq!(stopped.status.code(), Some(1));
    golden(
        "serve/status-stopped.keys",
        &shape(&json_of(&stdout(&stopped))),
    );

    sb.ok(&["serve", "--detach", "--port", "0"]);
    let _stop = StopGateway(&sb);
    let info: Value =
        serde_json::from_slice(&fs::read(sb.runtime_file("serve.json")).unwrap()).unwrap();
    let port = info["port"].as_u64().unwrap();
    let token = info["token"].as_str().unwrap();
    // One call through the gateway, so the metrics windows have a row.
    post(port, &format!("/{token}/eth"), "eth_blockNumber");
    wait_until("a metrics row", Duration::from_secs(10), || {
        sb.ok(&["serve", "status", "--json"])
            .contains("eth_blockNumber")
    });
    let running = json_of(&sb.ok(&["serve", "status", "--json"]));
    golden("serve/status-running.keys", &shape(&running));
}

/// Stops the sandbox's gateway, also when the test fails.
struct StopGateway<'a>(&'a Sandbox);

impl Drop for StopGateway<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(&["serve", "stop"]);
    }
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// POST one JSON-RPC call to the gateway on `port`.
fn post(port: u64, path: &str, method: &str) {
    use std::io::{BufReader, Write};
    let stream = std::net::TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":[]}}"#);
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut reader = BufReader::new(stream);
    reader.get_mut().write_all(request.as_bytes()).unwrap();
    common::read_message(&mut reader).expect("a response");
}

// ---------------------------------------------------------------- top

/// The `tests/top.rs` fixture: `live` on the mock through a `${VAR}`,
/// `dead` on a closed port, and a keyring signer with a cached address.
fn top_sandbox(rpc: &MockRpc) -> Sandbox {
    let sb = Sandbox::new();
    sb.write_config(&format!(
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
address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
"#,
        rpc = rpc.url()
    ));
    sb.write_secrets("[vars]\nRPC_KEY = \"s3cr3t-key\"\n");
    sb
}

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
    let args = &calldata[10..];
    let word = |i: usize| &args[i * 64..(i + 1) * 64];
    let number = |i: usize| usize::from_str_radix(&word(i)[48..], 16).unwrap();
    let count = number(1);
    (0..count)
        .map(|i| word(2 + number(2 + i) / 32)[24..].to_lowercase())
        .collect()
}

/// The `tests/top.rs` mock: chain 1 at head 1,000 with 12 s blocks (the
/// head stamped with the mock's start time), and a Multicall3 reporting
/// 1.5 ETH, 2.5 USDC and nothing else.
///
/// Every answer takes [`TOP_RPC_DELAY`], so measured latencies have two
/// digits and the text tables keep their column widths.
fn top_rpc() -> MockRpc {
    // One clock reading for every block, so the head and the look-back
    // block are always exactly 12 s apart.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    MockRpc::start(move |request| {
        std::thread::sleep(TOP_RPC_DELAY);
        Ok(match request.method.as_str() {
            "eth_getBlockByNumber" => {
                let number = match request.params[0].as_str().unwrap() {
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
                let calldata = request.params[0]["data"].as_str().unwrap();
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
        })
    })
}

const TOP_RPC_DELAY: Duration = Duration::from_millis(20);

/// Mask what changes run to run in `top --once` text: latencies (`23ms`)
/// and ages (`0s`, `1s ago`) become `NNms` and `Ns`, width kept.
fn mask_top_text(text: &str) -> String {
    let mask = |token: &str| -> Option<String> {
        let digits = token
            .strip_suffix("ms")
            .or_else(|| token.strip_suffix('s'))?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some("N".repeat(digits.len()) + &token[digits.len()..])
    };
    text.lines()
        .map(|line| {
            let words: Vec<String> = line
                .split(' ')
                .map(|w| mask(w).unwrap_or_else(|| w.to_string()))
                .collect();
            words.join(" ") + "\n"
        })
        .collect()
}

/// Mask the clock-dependent fields of `top --once --json` (pretty
/// printed, one field per line), keeping the bytes otherwise.
fn mask_top_json(text: &str) -> String {
    const VOLATILE: &[&str] = &[
        "at",
        "block_timestamp",
        "block_age_secs",
        "latency_ms",
        "p50_ms",
    ];
    text.lines()
        .map(|line| {
            let masked = VOLATILE.iter().find_map(|key| {
                let indent = &line[..line.len() - line.trim_start().len()];
                let value = line.trim_start().strip_prefix(&format!("\"{key}\": "))?;
                let (number, comma) = match value.strip_suffix(',') {
                    Some(n) => (n, ","),
                    None => (value, ""),
                };
                number
                    .bytes()
                    .all(|b| b.is_ascii_digit())
                    .then(|| format!("{indent}\"{key}\": \"<volatile>\"{comma}"))
            });
            masked.unwrap_or_else(|| line.to_string()) + "\n"
        })
        .collect()
}

/// `top --once` and `--once --json` against the `tests/top.rs` mock. The
/// mock's port and the clock-dependent values are masked.
#[test]
fn top_once() {
    let rpc = top_rpc();
    let sb = top_sandbox(&rpc);
    let host = format!("127.0.0.1:{}", rpc.port);

    let text = sb.ok(&["top", "--once"]);
    golden(
        "top/once.txt",
        &mask_top_text(&text.replace(&host, "127.0.0.1:<port>")),
    );

    let json = sb.ok(&["top", "--once", "--json"]);
    json_of(&json);
    golden(
        "top/once.json",
        &mask_top_json(&json).replace(&host, "127.0.0.1:<port>"),
    );
}
