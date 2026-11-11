//! Structural hot-path test (Linux): `exec … -- true` and `ls` open no
//! sockets, start no threads, and open only config, secrets and runtime
//! files under the chainz directories. Skipped when `strace` is missing.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

const TEST_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

fn strace_available() -> bool {
    Command::new("strace")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn sandbox() -> TempDir {
    let root = TempDir::new().unwrap();
    for d in ["home", "config/chainz", "state", "cache", "runtime"] {
        fs::create_dir_all(root.path().join(d)).unwrap();
    }
    fs::write(
        root.path().join("config/chainz/config.toml"),
        format!(
            "version = 2\ndefault = \"eth\"\n\n[signers.default]\nbackend = \"keyring\"\naddress = \"{TEST_ADDRESS}\"\n\n[chains.eth]\nid = 1\nsigner = \"default\"\nrpc = \"a\"\n\n[chains.eth.rpcs]\na = \"https://x.invalid/${{KEY}}\"\n"
        ),
    )
    .unwrap();
    fs::write(
        root.path().join("config/chainz/secrets.toml"),
        "[vars]\nKEY = \"k\"\n",
    )
    .unwrap();
    root
}

/// Trace `chainz args…` up to its execve of the child (if any).
fn trace(root: &Path, args: &[&str]) -> Vec<String> {
    let out = root.join("trace");
    let status = Command::new("strace")
        .args([
            "-f",
            "-qq",
            "-e",
            "trace=socket,connect,clone,clone3,fork,vfork,openat,open,execve",
            "-o",
        ])
        .arg(&out)
        .arg(assert_cmd::cargo::cargo_bin!("chainz"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("CHAINZ_DISABLE_KEYRING", "1")
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "chainz {args:?} failed under strace");
    let text = fs::read_to_string(&out).unwrap();
    let mut lines = Vec::new();
    let mut execs = 0;
    for line in text.lines() {
        if line.contains("execve(") {
            execs += 1;
            // The first execve is chainz itself; the second is the child.
            if execs > 1 {
                break;
            }
        }
        lines.push(line.to_string());
    }
    lines
}

fn check(root: &Path, args: &[&str]) {
    let lines = trace(root, args);
    for line in &lines {
        for call in [
            "socket(", "connect(", "clone(", "clone3(", "fork(", "vfork(",
        ] {
            assert!(
                !line.contains(call),
                "chainz {args:?} made a {call}…) call: {line}"
            );
        }
    }
    let root_str = root.to_str().unwrap();
    let allowed = [
        "/config/chainz/config.toml",
        "/config/chainz/secrets.toml",
        "/config/chainz/config.json",
        "/runtime/chainz",
    ];
    for line in lines.iter().filter(|l| l.contains("open")) {
        let Some(start) = line.find('"') else {
            continue;
        };
        let rest = &line[start + 1..];
        let path = &rest[..rest.find('"').unwrap_or(rest.len())];
        if let Some(rel) = path.strip_prefix(root_str) {
            assert!(
                allowed.iter().any(|a| rel.starts_with(a)),
                "chainz {args:?} opened {rel}"
            );
        }
    }
}

#[test]
fn hot_path_opens_no_sockets_and_starts_no_threads() {
    if !strace_available() {
        eprintln!("skipping: strace not found");
        return;
    }
    let root = sandbox();
    check(root.path(), &["exec", "eth", "--", "true"]);
    check(root.path(), &["exec", "--", "true"]);
    check(
        root.path(),
        &["exec", "eth", "--", "echo", "@wallet", "@rpc"],
    );
    check(root.path(), &["ls"]);
    check(root.path(), &["env", "eth", "--shell", "bash"]);
}
