//! End-to-end tests for `chainz serve` and exec auto-routing: the real
//! binary, a real gateway process, and mock upstreams on loopback.
//!
//! Every invocation is sandboxed like `cli.rs`: `HOME` and the four XDG
//! directories point into a temp dir and the OS keyring is disabled.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

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
];

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

    /// Run and require success; returns stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "chainz {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn write_config(&self, toml: &str) {
        let path = self.root.path().join("config/chainz/config.toml");
        fs::write(&path, toml).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn runtime_file(&self, name: &str) -> PathBuf {
        self.root.path().join("runtime/chainz").join(name)
    }

    /// What `exec` exports as `ETH_RPC_URL` (and headers) for these flags.
    fn exec_env(&self, flags: &[&str]) -> (String, String) {
        let mut args = vec!["exec", "eth"];
        args.extend_from_slice(flags);
        args.extend_from_slice(&[
            "--",
            "sh",
            "-c",
            r#"printf '%s\n%s' "$ETH_RPC_URL" "${ETH_RPC_HEADERS-unset}""#,
        ]);
        let out = self.ok(&args);
        let (url, headers) = out.split_once('\n').unwrap();
        (url.to_string(), headers.to_string())
    }
}

/// A threaded mock JSON-RPC upstream answering every call with its name.
struct Upstream {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    fn start(name: &'static str) -> Upstream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let log = log.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream);
                    while let Some((head, body)) = read_message(&mut reader) {
                        let request: serde_json::Value =
                            serde_json::from_slice(&body).unwrap_or_default();
                        log.lock().unwrap().push(format!(
                            "{} {head}",
                            request["method"].as_str().unwrap_or("?")
                        ));
                        let body = serde_json::json!({
                            "jsonrpc": "2.0", "id": request["id"], "result": name
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        if reader.get_mut().write_all(response.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Upstream { port, seen }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// `"<method> <lowercased http head>"` per request.
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

/// Read one HTTP message (request or response): lowercased head and body.
fn read_message(reader: &mut BufReader<TcpStream>) -> Option<(String, Vec<u8>)> {
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

/// POST one JSON-RPC call to a URL the way cast would; returns `result`.
fn rpc(url: &str, method: &str) -> serde_json::Value {
    let rest = url.strip_prefix("http://").unwrap();
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let stream = TcpStream::connect(authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":[]}}"#);
    let request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut reader = BufReader::new(stream);
    reader.get_mut().write_all(request.as_bytes()).unwrap();
    let (_, body) = read_message(&mut reader).expect("a response");
    let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
    response["result"].clone()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn config(main: &Upstream, alt: &Upstream) -> String {
    format!(
        "version = 2\n[chains.eth]\nid = 1\naliases = [\"mainnet\"]\nrpc = \"main\"\n[chains.eth.rpcs]\nmain = {{ url = \"{}/v3/${{SERVE_TEST_KEY}}\", headers = {{ x-api-key = \"hdrsekrit\" }} }}\nalt = \"{}\"\nsock = \"wss://127.0.0.1:9/ws\"\n",
        main.url(),
        alt.url()
    )
}

#[test]
fn exec_routes_through_a_running_gateway_and_direct_bypasses_it() {
    let sb = Sandbox::new();
    let main = Upstream::start("main");
    let alt = Upstream::start("alt");
    sb.write_config(&config(&main, &alt));
    sb.ok(&["var", "set", "SERVE_TEST_KEY", "urlsekrit"]);
    let direct_main = format!("{}/v3/urlsekrit", main.url());

    // No gateway: exec goes direct, with the endpoint's headers.
    assert_eq!(
        sb.exec_env(&[]),
        (direct_main.clone(), "x-api-key: hdrsekrit".to_string())
    );
    let status = sb.run(&["serve", "status"]);
    assert_eq!(status.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&status.stdout).contains("not running"));
    assert!(!sb.run(&["serve", "log"]).status.success());

    // Start it in the background.
    let port = free_port();
    let request_log = sb.root.path().join("requests.jsonl");
    let started = sb.ok(&[
        "serve",
        "--detach",
        "--port",
        &port.to_string(),
        "--log",
        request_log.to_str().unwrap(),
    ]);
    assert!(
        started.contains(&format!("running on 127.0.0.1:{port}")),
        "{started}"
    );
    for file in ["serve.json", "serve.sock"] {
        let mode = fs::metadata(sb.runtime_file(file))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{file}");
    }
    let info: serde_json::Value =
        serde_json::from_slice(&fs::read(sb.runtime_file("serve.json")).unwrap()).unwrap();
    let token = info["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);

    // A second gateway is refused.
    let second = sb.run(&["serve", "--port", &free_port().to_string()]);
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));

    // exec now hands out the tokenised loopback URL, and no headers.
    let gateway = format!("http://127.0.0.1:{port}/{token}/eth");
    assert_eq!(sb.exec_env(&[]), (gateway.clone(), "unset".to_string()));
    assert_eq!(rpc(&gateway, "eth_chainId"), "main");
    let seen = main.seen();
    assert_eq!(seen.len(), 1);
    // The gateway applied the secret URL and the header itself.
    assert!(
        seen[0].starts_with("eth_chainId post /v3/urlsekrit "),
        "{seen:?}"
    );
    assert!(seen[0].contains("x-api-key: hdrsekrit"), "{seen:?}");
    assert!(!seen[0].contains(&token), "the token went upstream");

    // --rpc NAME pins through the gateway.
    let (pinned, headers) = sb.exec_env(&["--rpc", "alt"]);
    assert_eq!(
        (pinned.as_str(), headers.as_str()),
        (format!("{gateway}/alt").as_str(), "unset")
    );
    assert_eq!(rpc(&pinned, "eth_blockNumber"), "alt");
    assert_eq!(alt.seen().len(), 1);

    // --direct bypasses it; so does a websocket endpoint.
    assert_eq!(
        sb.exec_env(&["--direct"]),
        (direct_main.clone(), "x-api-key: hdrsekrit".to_string())
    );
    assert_eq!(sb.exec_env(&["--direct", "--rpc", "alt"]).0, alt.url());
    assert_eq!(sb.exec_env(&["--rpc", "sock"]).0, "wss://127.0.0.1:9/ws");
    assert_eq!(main.seen().len(), 1, "--direct must not touch the gateway");

    // --explain shows the route without leaking the token.
    let explain = sb.ok(&["exec", "eth", "--explain", "--", "true"]);
    assert!(!explain.contains(&token), "{explain}");

    // `chainz rpc use` reaches the running gateway.
    sb.ok(&["rpc", "use", "eth", "alt"]);
    wait_until("the gateway to pick up `rpc use`", || {
        rpc(&gateway, "eth_gasPrice") == "alt"
    });

    // serve status and serve log.
    let status = sb.ok(&["serve", "status"]);
    assert!(
        status.contains(&format!("running on 127.0.0.1:{port}")),
        "{status}"
    );
    let row = status
        .lines()
        .find(|l| l.starts_with("eth "))
        .expect("eth row");
    assert!(
        row.contains(" alt "),
        "active endpoint after `rpc use`: {status}"
    );
    let log = sb.ok(&["serve", "log"]);
    assert!(log.contains("eth eth_chainId → main"), "{log}");
    assert!(log.contains("eth eth_blockNumber → alt"), "{log}");
    let filtered = sb.ok(&[
        "serve",
        "log",
        "--chain",
        "mainnet",
        "--method",
        "eth_chainId",
    ]);
    assert_eq!(filtered.lines().count(), 1, "{filtered}");
    assert!(sb.ok(&["serve", "log", "--chain", "nope"]).is_empty());
    for text in [&status, &log] {
        assert!(!text.contains(&token) && !text.contains("sekrit"), "{text}");
    }

    // serve log -f: the last records, then new ones as they happen.
    let mut follow = sb
        .cmd()
        .args(["serve", "log", "-f", "--method", "eth_feeHistory"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(follow.stdout.take().unwrap()).lines();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rpc(&gateway, "eth_feeHistory"), "alt");
    let line = lines.next().unwrap().unwrap();
    assert!(line.contains("eth eth_feeHistory"), "{line}");

    // Stop: SIGTERM, files withdrawn, exec direct again, follower ends.
    let stopped = sb.ok(&["serve", "stop"]);
    assert!(stopped.contains("stopped"), "{stopped}");
    assert!(follow.wait().unwrap().success());
    assert!(!sb.runtime_file("serve.json").exists());
    assert!(!sb.runtime_file("serve.sock").exists());
    assert_eq!(sb.exec_env(&[]).0, alt.url());
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    assert!(sb.ok(&["serve", "stop"]).contains("not running"));

    // The JSONL log has every request, and nothing secret.
    let jsonl = fs::read_to_string(&request_log).unwrap();
    assert!(jsonl.lines().count() >= 4, "{jsonl}");
    for line in jsonl.lines() {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(record["chain"], "eth");
    }
    assert!(!jsonl.contains("sekrit") && !jsonl.contains(&token));
    let serve_log = fs::read_to_string(sb.root.path().join("state/chainz/serve.log")).unwrap();
    assert!(serve_log.contains("listening on"), "{serve_log}");
    assert!(
        !serve_log.contains("sekrit") && !serve_log.contains(&token),
        "{serve_log}"
    );
}

#[test]
fn foreground_gateway_reloads_on_sighup_and_drains_on_sigterm() {
    let sb = Sandbox::new();
    let main = Upstream::start("main");
    let alt = Upstream::start("alt");
    sb.write_config(&config(&main, &alt));
    sb.ok(&["var", "set", "SERVE_TEST_KEY", "k"]);
    let port = free_port();
    let mut child = sb
        .cmd()
        .args(["serve", "--port", &port.to_string()])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until("serve.json", || sb.runtime_file("serve.json").exists());
    let (gateway, _) = sb.exec_env(&[]);
    assert!(
        gateway.starts_with(&format!("http://127.0.0.1:{port}/")),
        "{gateway}"
    );
    assert_eq!(rpc(&gateway, "eth_chainId"), "main");

    // SIGHUP reloads (and must not kill the gateway).
    let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::HUP).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(rpc(&gateway, "eth_chainId"), "main");

    // SIGTERM: a clean exit.
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains(&format!("listening on http://127.0.0.1:{port}")),
        "{stderr}"
    );
    assert!(stderr.contains("config reloaded"), "{stderr}");
    assert!(!sb.runtime_file("serve.json").exists());
    assert_eq!(sb.exec_env(&[]).0, format!("{}/v3/k", main.url()));
}

#[test]
fn serve_refuses_an_empty_config_and_a_busy_port() {
    let sb = Sandbox::new();
    let out = sb.run(&["serve"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no chains configured"));

    let main = Upstream::start("main");
    let alt = Upstream::start("alt");
    sb.write_config(&config(&main, &alt));
    // The mock's own port is taken.
    let out = sb.run(&["serve", "--port", &main.port.to_string()]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("can't listen on 127.0.0.1"), "{stderr}");
    // A failed start leaves nothing behind.
    assert!(!sb.runtime_file("serve.json").exists());
    assert!(!sb.runtime_file("serve.sock").exists());
    assert_eq!(sb.run(&["serve", "status"]).status.code(), Some(1));
    // --detach reports the failure too.
    let out = sb.run(&["serve", "--detach", "--port", &main.port.to_string()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("exited during startup"));
}
