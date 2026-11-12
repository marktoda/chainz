//! Test support shared by the chainz crates. Never published: every crate
//! takes it as a path dev-dependency with no version, which `cargo package`
//! drops, and it depends on no chainz crate.
//!
//! - [`Mock`]: a JSON-RPC server on loopback, answering per call or per
//!   HTTP request, recording what it was sent.
//! - [`http`]: a raw HTTP/1.1 client (sync and async), for talking to a
//!   gateway the way a non-Rust client would.
//! - [`abi`]: the few ABI encodings the mocks need, with selectors written
//!   out by hand so a wrong constant in chainz is not echoed back.
//! - [`golden!`]: compare text with a file under `tests/golden/`.
//! - [`wait_until`], [`wait_until_async`], [`short_tempdir`],
//!   [`require_tool`], [`ChildGuard`], and the anvil test keys.

pub mod abi;
pub mod golden;
pub mod http;
mod mock;

pub use mock::{Answer, Call, Mock, Reply, Request};

use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

/// Anvil's (and Hardhat's) well-known test key #0 and its address.
pub const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
/// The address of [`TEST_KEY`].
pub const TEST_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
/// Anvil's test key #1.
pub const TEST_KEY_2: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
/// The address of [`TEST_KEY_2`].
pub const TEST_ADDRESS_2: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Poll `done` every 10 ms until it holds; panic naming `what` after
/// `timeout`.
#[track_caller]
pub fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// [`wait_until`] for async tests: sleeps on the tokio timer, so tasks on
/// the same runtime keep running.
pub async fn wait_until_async(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A temp dir directly under `/tmp`. A unix socket path must fit in
/// `sun_path` (108 bytes on Linux, 104 on macOS), and `$TMPDIR` can be long
/// enough on its own (nix shells, macOS) to push one past it.
pub fn short_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let longest = dir.path().join("runtime/chainz/serve.sock");
    let len = longest.as_os_str().len();
    assert!(
        len < 100,
        "socket path {} is {len} bytes, past the unix socket limit",
        longest.display()
    );
    dir
}

/// Whether `tool` is on `PATH`. When it is not, the test should return
/// early: this says so on stderr, past the test harness's capture, so a
/// skipped test does not pass unseen. With `CHAINZ_REQUIRE_TOOLS` set (CI
/// sets it) a missing tool fails the test instead.
pub fn require_tool(tool: &str) -> bool {
    let found = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| is_executable(&dir.join(tool))));
    if !found {
        assert!(
            std::env::var_os("CHAINZ_REQUIRE_TOOLS").is_none(),
            "CHAINZ_REQUIRE_TOOLS is set but {tool} is not on PATH"
        );
        let test = std::thread::current()
            .name()
            .unwrap_or("a test")
            .to_string();
        // Straight to the file descriptor: `eprintln!` is captured.
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED {test}: {tool} is not on PATH (CHAINZ_REQUIRE_TOOLS=1 fails instead)"
        );
    }
    found
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// A child process that is killed and reaped when the guard drops, so a
/// failing test leaves nothing running.
#[derive(Debug)]
pub struct ChildGuard(pub Child);

impl Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
