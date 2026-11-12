//! `signer add`, `signer rm`, `signer migrate`.

use crate::common::*;

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
fn a_keystore_signer_is_a_reference() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let ks = keystore_file(&sb);
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

/// A keystore path that doesn't exist, or a keystore with no address
/// field and no `--address` (`cast wallet new` writes none), adds nothing.
#[test]
fn signer_add_keystore_needs_a_file_and_an_address() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let before = sb.config();
    let missing = sb.home().join("nope.json");
    let out = sb
        .process()
        .args([
            "signer",
            "add",
            "ks",
            "--backend",
            "keystore",
            "--address",
            TEST_ADDRESS_2,
            "--path",
        ])
        .arg(&missing)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("keystore not found"),
        "{}",
        stderr(&out)
    );
    let out = sb
        .process()
        .args(["signer", "add", "ks", "--path"])
        .arg(keystore_file(&sb))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("has no address field"), "{err}");
    assert_eq!(err.matches("--address").count(), 1, "{err}");
    assert!(err.contains("cast wallet address --keystore"), "{err}");
    assert_eq!(sb.config(), before);
}

/// An (empty) keystore file in the sandbox.
fn keystore_file(sb: &Sandbox) -> std::path::PathBuf {
    let path = sb.home().join("ks.json");
    fs::write(&path, "{}").unwrap();
    path
}

#[test]
fn signer_migrate_off_a_tty_changes_nothing_and_reports_each_failure() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1)]);
    let unchanged = |sb: &Sandbox| (sb.config(), sb.secrets());
    let before = unchanged(&sb);

    // The keyring is disabled in tests; encrypted (the default without a
    // keyring) needs a prompt.
    let out = sb.run(&["signer", "migrate", "default", "--to", "keyring"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("keyring is unavailable"),
        "{}",
        stderr(&out)
    );
    let out = sb.run(&["signer", "migrate", "default"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("interactive password prompt"),
        "{}",
        stderr(&out)
    );
    assert_eq!(unchanged(&sb), before);

    sb.cmd()
        .args([
            "signer",
            "add",
            "second",
            "--backend",
            "plaintext",
            "--stdin",
        ])
        .write_stdin(TEST_KEY_2)
        .assert()
        .success();
    let before = unchanged(&sb);
    let out = sb.run(&["signer", "migrate"]);
    assert!(
        stderr(&out).contains("2 plaintext signers (default, second); pass a name or --all"),
        "{}",
        stderr(&out)
    );
    // --all tries every one and names each failure.
    let out = sb.run(&["signer", "migrate", "--all", "--to", "encrypted"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("✗ 'default': ") && err.contains("✗ 'second': "),
        "{err}"
    );
    assert!(
        err.contains("2 of 2 signers not moved: default, second"),
        "{err}"
    );
    assert_eq!(unchanged(&sb), before);

    // References are never moved.
    sb.cmd()
        .args([
            "signer",
            "add",
            "ks",
            "--backend",
            "keystore",
            "--address",
            TEST_ADDRESS_2,
        ])
        .arg("--path")
        .arg(keystore_file(&sb))
        .assert()
        .success();
    let out = sb.run(&["signer", "migrate", "ks"]);
    assert!(
        stderr(&out).contains("chainz does not move it"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn signer_rm_needs_yes_off_a_tty_and_names_every_flag_at_once() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    let out = sb.run(&["signer", "rm", "default"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("unassigns it from eth, base and deletes its key in secrets.toml"),
        "{err}"
    );
    assert!(err.contains("pass --unassign --yes"), "{err}");
    let out = sb.run(&["signer", "rm", "default", "--unassign"]);
    assert!(stderr(&out).contains("; pass --yes"), "{}", stderr(&out));
    assert!(sb.secrets().contains(TEST_KEY), "nothing deleted");
    assert!(sb.config().contains("[signers.default]"));

    // A keystore reference deletes no key material, but is a removal too.
    sb.cmd()
        .args([
            "signer",
            "add",
            "ks",
            "--backend",
            "keystore",
            "--address",
            TEST_ADDRESS_2,
        ])
        .arg("--path")
        .arg(keystore_file(&sb))
        .assert()
        .success();
    let out = sb.run(&["signer", "rm", "ks"]);
    assert!(
        stderr(&out).contains("(keystore kept); pass --yes"),
        "{}",
        stderr(&out)
    );
    sb.ok(&["signer", "rm", "ks", "--yes"]);
    // Unassigning says which chains are left without a signer.
    let out = sb.run(&["signer", "rm", "default", "--unassign", "-y"]);
    assert!(out.status.success());
    assert!(
        stderr(&out).contains(
            "eth, base have no signer ('default' was removed); attach one with `chainz update CHAIN --signer NAME`"
        ),
        "{}",
        stderr(&out)
    );
    assert!(!sb.secrets().contains(TEST_KEY));
    assert!(!sb.config().contains("[signers.default]"));
    assert!(!sb.config().contains("signer = \"default\""));
}

/// Hidden prompts read the controlling terminal, not stdin: with a key
/// piped in and a terminal present, `encrypted` still asks for its
/// password (on the terminal) instead of failing.
#[cfg(target_os = "linux")]
#[test]
fn a_piped_key_still_gets_its_password_prompt_on_a_terminal() {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let sb = Sandbox::new();
    let (mut master, slave) = {
        let (mut m, mut s) = (0, 0);
        // SAFETY: openpty writes two fds we then own.
        let rc = unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        // SAFETY: both fds are fresh and owned only here.
        unsafe { (fs::File::from_raw_fd(m), OwnedFd::from_raw_fd(s)) }
    };
    let slave_fd = slave.as_raw_fd();
    let mut cmd = sb.process();
    cmd.args(["signer", "add", "enc", "--stdin", "--no-attach"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(fs::File::from(slave.try_clone().unwrap()));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            // A new session whose controlling terminal is the pty, so
            // `/dev/tty` opens it.
            if libc::setsid() < 0 || libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    // The command holds a copy of the terminal; drop it so the terminal
    // closes once the child exits and `slave` (kept to read the terminal's
    // modes) is dropped.
    drop(cmd);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(TEST_KEY.as_bytes())
        .unwrap();

    // Answer each password prompt as it appears on the terminal.
    let mut answers = master.try_clone().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        while let Ok(n @ 1..) = master.read(&mut buf) {
            if tx
                .send(String::from_utf8_lossy(&buf[..n]).into_owned())
                .is_err()
            {
                break;
            }
        }
    });
    let mut seen = String::new();
    let mut answered = 0;
    while answered < 2 {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(text) => seen.push_str(&text),
            Err(_) => {
                let _ = child.kill();
                panic!("no password prompt on the terminal; saw {seen:?}");
            }
        }
        // Both prompts ("Encryption password for enc: ", "Confirm …: ")
        // end in ": ".
        if seen.matches(": ").count() > answered {
            // Answer once the prompt has turned echo off, as a person
            // would: typed earlier, the line would race its setup.
            wait_until(
                "the prompt to turn echo off",
                Duration::from_secs(30),
                || {
                    // SAFETY: a zeroed termios is a valid out-parameter.
                    let mut term: libc::termios = unsafe { std::mem::zeroed() };
                    // SAFETY: `slave_fd` is open (`slave` is alive).
                    let read = unsafe { libc::tcgetattr(slave_fd, &mut term) };
                    read == 0 && term.c_lflag & libc::ECHO == 0
                },
            );
            answers.write_all(b"hunter2\n").unwrap();
            answered += 1;
        }
    }
    drop(slave);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{seen}{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(TEST_ADDRESS));
    assert!(sb.config().contains("encrypted"));
    assert!(!sb.secrets().contains(TEST_KEY));
}
