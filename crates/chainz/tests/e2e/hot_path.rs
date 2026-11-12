//! Structural hot-path test (Linux): `exec … -- true`, `ls` and `env` open
//! no sockets, start no threads, and open only config, secrets and runtime
//! files under the chainz directories. Needs `strace` (see
//! [`require_tool`]).
#![cfg(target_os = "linux")]

use crate::common::*;
use std::process::Command;

/// `eth` on a `${VAR}` URL, the default, with a keyring signer.
fn sandbox() -> Sandbox {
    let sb = Sandbox::new();
    sb.write_config(&format!(
        "version = 2\ndefault = \"eth\"\n\n[signers.default]\nbackend = \"keyring\"\naddress = \"{TEST_ADDRESS}\"\n\n[chains.eth]\nid = 1\nsigner = \"default\"\nrpc = \"a\"\n\n[chains.eth.rpcs]\na = \"https://x.invalid/${{KEY}}\"\n"
    ));
    sb.write_secrets("[vars]\nKEY = \"k\"\n");
    sb
}

/// Trace `chainz args…` up to its execve of the child (if any).
fn trace(sb: &Sandbox, args: &[&str]) -> Vec<String> {
    let out = sb.path("trace");
    let mut strace = Command::new("strace");
    sb.apply(&mut strace);
    let status = strace
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
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
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

fn check(sb: &Sandbox, args: &[&str]) {
    let lines = trace(sb, args);
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
    let root_str = sb.root().to_str().unwrap();
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
    if !require_tool("strace") {
        return;
    }
    let sb = sandbox();
    check(&sb, &["exec", "eth", "--", "true"]);
    check(&sb, &["exec", "--", "true"]);
    check(&sb, &["exec", "eth", "--", "echo", "@wallet", "@rpc"]);
    check(&sb, &["ls"]);
    check(&sb, &["env", "eth", "--shell", "bash"]);
}
