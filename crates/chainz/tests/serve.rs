//! End-to-end tests for `chainz serve` and exec auto-routing: the real
//! binary, a real gateway process, and mock upstreams on loopback.
//!
//! Every invocation is sandboxed like `cli.rs`: `HOME` and the four XDG
//! directories point into a temp dir and the OS keyring is disabled.
mod common;

use common::{MockRpc, Request, Sandbox, read_message, send_signal, stderr, wait_until};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(10);

/// What `exec` exports as `ETH_RPC_URL` (and headers) for these flags.
fn exec_env(sb: &Sandbox, flags: &[&str]) -> (String, String) {
    let mut args = vec!["exec", "eth"];
    args.extend_from_slice(flags);
    args.extend_from_slice(&[
        "--",
        "sh",
        "-c",
        r#"printf '%s\n%s' "$ETH_RPC_URL" "${ETH_RPC_HEADERS-unset}""#,
    ]);
    let out = sb.ok(&args);
    let (url, headers) = out.split_once('\n').unwrap();
    (url.to_string(), headers.to_string())
}

/// A mock upstream answering every call with its name.
fn upstream(name: &'static str) -> MockRpc {
    MockRpc::start(move |_| Ok(serde_json::json!(name)))
}

/// `"<method> <lowercased http head>"` per request `rpc` received.
fn calls(rpc: &MockRpc) -> Vec<String> {
    rpc.seen()
        .iter()
        .map(|Request { method, head, .. }| format!("{method} {head}"))
        .collect()
}

/// The gateway's port, from its discovery record.
fn gateway_port(sb: &Sandbox) -> u16 {
    let info: serde_json::Value =
        serde_json::from_slice(&fs::read(sb.runtime_file("serve.json")).unwrap()).unwrap();
    info["port"].as_u64().unwrap() as u16
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

fn config(main: &MockRpc, alt: &MockRpc) -> String {
    format!(
        "version = 2\n[chains.eth]\nid = 1\naliases = [\"mainnet\"]\nrpc = \"main\"\n[chains.eth.rpcs]\nmain = {{ url = \"{}/v3/${{SERVE_TEST_KEY}}\", headers = {{ x-api-key = \"hdrsekrit\" }} }}\nalt = \"{}\"\nsock = \"wss://127.0.0.1:9/ws\"\n",
        main.url(),
        alt.url()
    )
}

#[test]
fn exec_routes_through_a_running_gateway_and_direct_bypasses_it() {
    let sb = Sandbox::new();
    let main = upstream("main");
    let alt = upstream("alt");
    sb.write_config(&config(&main, &alt));
    sb.ok(&["var", "set", "SERVE_TEST_KEY", "urlsekrit"]);
    let direct_main = format!("{}/v3/urlsekrit", main.url());

    // No gateway: exec goes direct, with the endpoint's headers.
    assert_eq!(
        exec_env(&sb, &[]),
        (direct_main.clone(), "x-api-key: hdrsekrit".to_string())
    );
    let status = sb.run(&["serve", "status"]);
    assert_eq!(status.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&status.stdout).contains("not running"));
    assert!(!sb.run(&["serve", "log"]).status.success());

    // Start it in the background.
    // Port 0: the OS picks a free one; serve.json says which.
    let request_log = sb.root().join("requests.jsonl");
    let started = sb.ok(&[
        "serve",
        "--detach",
        "--port",
        "0",
        "--log",
        request_log.to_str().unwrap(),
    ]);
    let port = gateway_port(&sb);
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
    let second = sb.run(&["serve", "--port", "0"]);
    assert!(!second.status.success());
    assert!(stderr(&second).contains("already running"));

    // exec now hands out the tokenised loopback URL, and no headers.
    let gateway = format!("http://127.0.0.1:{port}/{token}/eth");
    assert_eq!(exec_env(&sb, &[]), (gateway.clone(), "unset".to_string()));
    assert_eq!(rpc(&gateway, "eth_chainId"), "main");
    let seen = calls(&main);
    assert_eq!(seen.len(), 1);
    // The gateway applied the secret URL and the header itself.
    assert!(
        seen[0].starts_with("eth_chainId post /v3/urlsekrit "),
        "{seen:?}"
    );
    assert!(seen[0].contains("x-api-key: hdrsekrit"), "{seen:?}");
    assert!(!seen[0].contains(&token), "the token went upstream");

    // --rpc NAME pins through the gateway.
    let (pinned, headers) = exec_env(&sb, &["--rpc", "alt"]);
    assert_eq!(
        (pinned.as_str(), headers.as_str()),
        (format!("{gateway}/alt").as_str(), "unset")
    );
    assert_eq!(rpc(&pinned, "eth_blockNumber"), "alt");
    assert_eq!(calls(&alt).len(), 1);

    // --direct bypasses it; so does a websocket endpoint.
    assert_eq!(
        exec_env(&sb, &["--direct"]),
        (direct_main.clone(), "x-api-key: hdrsekrit".to_string())
    );
    assert_eq!(exec_env(&sb, &["--direct", "--rpc", "alt"]).0, alt.url());
    assert_eq!(exec_env(&sb, &["--rpc", "sock"]).0, "wss://127.0.0.1:9/ws");
    assert_eq!(calls(&main).len(), 1, "--direct must not touch the gateway");

    // --explain shows the route without leaking the token.
    let explain = sb.ok(&["exec", "eth", "--explain", "--", "true"]);
    assert!(!explain.contains(&token), "{explain}");

    // `chainz update --rpc` reaches the running gateway.
    sb.ok(&["update", "eth", "--rpc", "alt"]);
    wait_until("the gateway to pick up `update --rpc`", WAIT, || {
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
        "active endpoint after `update --rpc`: {status}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&sb.ok(&["serve", "status", "--json"])).unwrap();
    assert_eq!(json["running"], true, "{json}");
    assert_eq!(json["port"], port, "{json}");
    assert_eq!(json["chains"][0]["chain"], "eth", "{json}");
    assert_eq!(json["chains"][0]["active"], "alt", "{json}");
    assert!(!json.to_string().contains(&token), "{json}");
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
        .process()
        .args(["serve", "log", "-f", "--method", "eth_feeHistory"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // No need to wait for the follower: a request it misses while
    // starting shows up in its backlog instead.
    let mut lines = BufReader::new(follow.stdout.take().unwrap()).lines();
    assert_eq!(rpc(&gateway, "eth_feeHistory"), "alt");
    let line = lines.next().unwrap().unwrap();
    assert!(line.contains("eth eth_feeHistory"), "{line}");

    // Stop: SIGTERM, files withdrawn, exec direct again, follower ends.
    let stopped = sb.ok(&["serve", "stop"]);
    assert!(stopped.contains("stopped"), "{stopped}");
    assert!(follow.wait().unwrap().success());
    assert!(!sb.runtime_file("serve.json").exists());
    assert!(!sb.runtime_file("serve.sock").exists());
    assert_eq!(exec_env(&sb, &[]).0, alt.url());
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    assert!(sb.ok(&["serve", "stop"]).contains("not running"));
    let stopped = sb.run(&["serve", "status", "--json"]);
    assert_eq!(stopped.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&stopped.stdout).unwrap(),
        serde_json::json!({"running": false})
    );

    // The JSONL log has every request, and nothing secret.
    let jsonl = fs::read_to_string(&request_log).unwrap();
    assert!(jsonl.lines().count() >= 4, "{jsonl}");
    for line in jsonl.lines() {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(record["chain"], "eth");
    }
    assert!(!jsonl.contains("sekrit") && !jsonl.contains(&token));
    let serve_log = fs::read_to_string(sb.state_file("serve.log")).unwrap();
    assert!(serve_log.contains("listening on"), "{serve_log}");
    assert!(
        !serve_log.contains("sekrit") && !serve_log.contains(&token),
        "{serve_log}"
    );
}

#[test]
fn foreground_gateway_reloads_on_sighup_and_drains_on_sigterm() {
    let sb = Sandbox::new();
    let main = upstream("main");
    let alt = upstream("alt");
    sb.write_config(&config(&main, &alt));
    sb.ok(&["var", "set", "SERVE_TEST_KEY", "k"]);
    let mut child = sb
        .process()
        .args(["serve", "--port", "0"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Collect stderr as it comes, to wait on notices.
    let notices = Arc::new(Mutex::new(String::new()));
    let reader = {
        let (notices, pipe) = (notices.clone(), child.stderr.take().unwrap());
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let mut notices = notices.lock().unwrap();
                notices.push_str(&line);
                notices.push('\n');
            }
        })
    };
    let printed = |text: &str| notices.lock().unwrap().contains(text);
    wait_until("serve.json", WAIT, || {
        sb.runtime_file("serve.json").exists()
    });
    let port = gateway_port(&sb);
    let (gateway, _) = exec_env(&sb, &[]);
    assert!(
        gateway.starts_with(&format!("http://127.0.0.1:{port}/")),
        "{gateway}"
    );
    assert_eq!(rpc(&gateway, "eth_chainId"), "main");
    wait_until("the listening notice", WAIT, || {
        printed(&format!("listening on http://127.0.0.1:{port}"))
    });

    // SIGHUP reloads, with nothing changed (so not the file watcher), and
    // must not kill the gateway.
    send_signal(child.id(), "HUP");
    wait_until("a reload on SIGHUP", WAIT, || printed("config reloaded"));
    assert_eq!(rpc(&gateway, "eth_chainId"), "main");
    // A changed config is served after the reload.
    sb.write_config(&config(&main, &alt).replace("rpc = \"main\"", "rpc = \"alt\""));
    send_signal(child.id(), "HUP");
    wait_until("the new active endpoint", WAIT, || {
        rpc(&gateway, "eth_chainId") == "alt"
    });
    assert!(child.try_wait().unwrap().is_none(), "SIGHUP killed it");

    // SIGTERM: a clean exit.
    send_signal(child.id(), "TERM");
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
    reader.join().unwrap();
    assert!(!sb.runtime_file("serve.json").exists());
    assert_eq!(exec_env(&sb, &[]).0, alt.url());
}

#[test]
fn foreground_gateway_cleans_up_when_its_stderr_is_closed() {
    let sb = Sandbox::new();
    let main = upstream("main");
    let alt = upstream("alt");
    sb.write_config(&config(&main, &alt));
    sb.ok(&["var", "set", "SERVE_TEST_KEY", "k"]);
    let mut child = sb
        .process()
        .args(["serve", "--port", "0"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until("serve.json", WAIT, || {
        sb.runtime_file("serve.json").exists()
    });
    // The terminal goes away: every later notice hits a closed pipe, which
    // must not end the gateway before it removes serve.json.
    drop(child.stderr.take());
    send_signal(child.id(), "TERM");
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
    assert!(!sb.runtime_file("serve.json").exists());
}

#[test]
fn serve_refuses_an_empty_config_and_a_busy_port() {
    let sb = Sandbox::new();
    let out = sb.run(&["serve"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no chains configured"));

    let main = upstream("main");
    let alt = upstream("alt");
    sb.write_config(&config(&main, &alt));
    // The mock's own port is taken.
    let out = sb.run(&["serve", "--port", &main.port.to_string()]);
    assert!(!out.status.success());
    let stderr = stderr(&out);
    assert!(stderr.contains("can't listen on 127.0.0.1"), "{stderr}");
    // A failed start leaves nothing behind.
    assert!(!sb.runtime_file("serve.json").exists());
    assert!(!sb.runtime_file("serve.sock").exists());
    assert_eq!(sb.run(&["serve", "status"]).status.code(), Some(1));
    // --detach reports the failure too.
    let out = sb.run(&["serve", "--detach", "--port", &main.port.to_string()]);
    assert!(!out.status.success());
    assert!(common::stderr(&out).contains("exited during startup"));
}
