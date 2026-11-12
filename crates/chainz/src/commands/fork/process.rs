//! Process plumbing for forks: signals, the anvil child, and stopping
//! processes. Which forks exist and which are alive is the core registry
//! ([`Runtime::forks`]): a fork lives while its supervisor holds its lock.

use crate::sys::{self, signal};
use anyhow::{Context as _, Result, anyhow};
use chainz_core::paths;
use chainz_core::runtime::{ForkEntry, ForkRecord, Runtime};
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// The stop signal that arrived, or 0.
static SHUTDOWN: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: libc::c_int) {
    SHUTDOWN.store(signal, Ordering::SeqCst);
}

/// Turn SIGINT, SIGTERM and SIGHUP into a shutdown request.
pub fn install_signal_handlers() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            libc::signal(signal, handler);
        }
    }
}

/// The exit code for the stop signal that arrived (128 + its number, so
/// 130 for Ctrl-C), if one did.
pub fn shutdown_requested() -> Option<i32> {
    match SHUTDOWN.load(Ordering::SeqCst) {
        0 => None,
        signal => Some(128 + signal),
    }
}

/// Resolves once a stop signal arrives, with its exit code.
pub async fn shutdown() -> i32 {
    loop {
        if let Some(code) = shutdown_requested() {
            return code;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether a process with this PID exists (zombies included).
fn pid_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    pid > 0 && (signal(pid, 0) || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// What the OS says about a process.
struct ProcInfo {
    zombie: bool,
    args: Vec<String>,
}

#[cfg(target_os = "linux")]
fn proc_info(pid: u32) -> Option<ProcInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) S …": comm may hold spaces and parentheses.
    let state = stat.rsplit_once(')')?.1.trim_start().chars().next()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(ProcInfo {
        zombie: matches!(state, 'Z' | 'X'),
        args: cmdline
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    })
}

#[cfg(not(target_os = "linux"))]
fn proc_info(pid: u32) -> Option<ProcInfo> {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-o", "args=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut words = text.split_whitespace();
    let state = words.next()?;
    Some(ProcInfo {
        zombie: state.starts_with('Z'),
        args: words.map(str::to_string).collect(),
    })
}

/// Whether `pid` is a live anvil on `port`. Only a process that provably
/// looks like our anvil ([`AnvilSpec::started`]) is ever signalled.
pub fn anvil_alive(pid: u32, port: Option<u16>) -> bool {
    if !pid_exists(pid) {
        return false;
    }
    proc_info(pid).is_some_and(|info| !info.zombie && AnvilSpec::started(&info.args, port))
}

/// Poll `done` every 25 ms for up to `timeout`.
fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Signal anvil's process group (anvil leads its own), else the process.
fn signal_anvil(pid: u32, sig: libc::c_int) {
    if let Ok(pid) = i32::try_from(pid)
        && pid > 1
        && !signal(-pid, sig)
    {
        signal(pid, sig);
    }
}

/// Stop an anvil that is not our child: SIGTERM, then SIGKILL after 3 s.
pub fn kill_anvil(pid: u32, port: Option<u16>) {
    if !anvil_alive(pid, port) {
        return;
    }
    signal_anvil(pid, libc::SIGTERM);
    if !wait_until(Duration::from_secs(3), || !anvil_alive(pid, port)) {
        signal_anvil(pid, libc::SIGKILL);
        wait_until(Duration::from_secs(2), || !anvil_alive(pid, port));
    }
}

/// How long a supervisor may take to stop on SIGTERM: it notices within
/// 200 ms, then gives anvil up to 3 s before killing it. Generous, since a
/// loaded machine is slow at all of it.
const SUPERVISOR_GRACE: Duration = Duration::from_secs(10);

/// Whether a supervisor still holds `name`'s lock. An unreadable lock
/// counts as held: nothing is removed under a fork that may be alive.
fn live(runtime: &Runtime, name: &str) -> bool {
    runtime.fork_live(name).unwrap_or(true)
}

/// Stop a fork: SIGTERM its supervisor (which stops anvil and removes the
/// fork's files), SIGKILL after [`SUPERVISOR_GRACE`], then reap whatever
/// is left, anvil included.
pub fn stop(runtime: &Runtime, entry: &ForkEntry) -> Result<()> {
    // Signalled only while it holds the lock, so the PID is the holder's.
    if let Some(pid) = entry.supervisor
        && live(runtime, &entry.name)
        && sys::signal_process(pid, libc::SIGTERM).is_ok()
        && !wait_until(SUPERVISOR_GRACE, || !live(runtime, &entry.name))
        && live(runtime, &entry.name)
    {
        let _ = sys::signal_process(pid, libc::SIGKILL);
        wait_until(Duration::from_secs(2), || !live(runtime, &entry.name));
    }
    runtime
        .reap_fork(&entry.name, stop_orphan)
        .with_context(|| format!("unable to remove fork '{}'", entry.name))?;
    Ok(())
}

/// Remove every fork whose supervisor is gone, killing an anvil it left.
/// Returns the names reaped.
pub fn reap(runtime: &Runtime) -> Result<Vec<String>> {
    runtime
        .reap_forks(stop_orphan)
        .context("unable to clean up the fork registry")
}

/// A dead fork's anvil, if it is still there (macOS has no
/// `PR_SET_PDEATHSIG`).
fn stop_orphan(record: &ForkRecord) {
    kill_anvil(record.anvil_pid, Some(record.port));
}

/// The fork's log: anvil's output, and the supervisor's when detached.
pub fn log_path(name: &str) -> Result<PathBuf> {
    paths::fork_log(name).context("unable to find the state directory")
}

/// Log lines shown when anvil fails to start.
pub const ERROR_TAIL_LINES: usize = 12;
/// Log lines shown when a detached fork fails to start: the supervisor's
/// own error comes after anvil's.
pub const DETACHED_TAIL_LINES: usize = 15;

/// The last `lines` lines of the log from byte `from` on (where this
/// attempt's output starts), indented, for error messages.
pub fn log_tail(path: &std::path::Path, from: u64, lines: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut text = String::new();
    if let Ok(mut file) = File::open(path) {
        let _ = file.seek(SeekFrom::Start(from));
        let mut bytes = Vec::new();
        let _ = file.read_to_end(&mut bytes);
        text = String::from_utf8_lossy(&bytes).into_owned();
    }
    let kept: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    kept[kept.len().saturating_sub(lines)..]
        .iter()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// How to start anvil.
pub struct AnvilSpec<'a> {
    /// `--fork-url`: the supervisor's proxy (never a URL holding a secret).
    pub fork_url: &'a str,
    /// `--port`.
    pub port: u16,
    /// `--fork-block-number`.
    pub block: Option<u64>,
    /// `--chain-id`.
    pub chain_id: Option<u64>,
}

/// The program a fork runs.
const ANVIL: &str = "anvil";
/// Anvil's port flag: in [`AnvilSpec::args`] and [`AnvilSpec::started`].
const PORT_FLAG: &str = "--port";

impl AnvilSpec<'_> {
    /// Anvil's arguments.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--fork-url".to_string(),
            self.fork_url.to_string(),
            "--auto-impersonate".to_string(),
            PORT_FLAG.to_string(),
            self.port.to_string(),
        ];
        if let Some(block) = self.block {
            args.push("--fork-block-number".to_string());
            args.push(block.to_string());
        }
        if let Some(chain_id) = self.chain_id {
            args.push("--chain-id".to_string());
            args.push(chain_id.to_string());
        }
        args
    }

    /// Whether a process's full argv is an anvil started from a spec
    /// (on `port`, when given).
    pub fn started(argv: &[String], port: Option<u16>) -> bool {
        let program = argv
            .first()
            .is_some_and(|a| a.rsplit('/').next() == Some(ANVIL));
        let on_port = port.is_none_or(|port| {
            argv.windows(2)
                .any(|w| w[0] == PORT_FLAG && w[1] == port.to_string())
        });
        program && on_port
    }
}

/// Spawn anvil in its own process group, output to `log`. On Linux it gets
/// `PR_SET_PDEATHSIG`, so it dies with the supervisor even on SIGKILL.
///
/// Must be called from the supervisor's main thread: the death signal
/// fires when the *thread* that forked exits.
pub fn spawn_anvil(spec: &AnvilSpec<'_>, log: &File) -> Result<Child> {
    use std::os::unix::process::CommandExt;
    // SAFETY: plain syscalls with no arguments.
    #[cfg(target_os = "linux")]
    debug_assert_eq!(
        unsafe { libc::gettid() },
        unsafe { libc::getpid() },
        "anvil must be spawned from the main thread"
    );
    let mut command = Command::new(ANVIL);
    command
        .args(spec.args())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .env_remove(sys::DETACHED_ENV)
        .process_group(0);
    #[cfg(target_os = "linux")]
    {
        let supervisor = std::process::id();
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // The supervisor may have died before prctl took effect.
                if libc::getppid() as u32 != supervisor {
                    return Err(io::Error::other("the fork supervisor exited"));
                }
                Ok(())
            });
        }
    }
    command.spawn().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => {
            anyhow!("`anvil` was not found on PATH; install Foundry (https://getfoundry.sh)")
        }
        _ => anyhow!("unable to start anvil: {e}"),
    })
}

/// Stop our anvil child: SIGTERM to its group, SIGKILL after 3 s, reap.
pub fn stop_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let pid = child.id();
    signal_anvil(pid, libc::SIGTERM);
    if !wait_until(Duration::from_secs(3), || {
        matches!(child.try_wait(), Ok(Some(_)))
    }) {
        signal_anvil(pid, libc::SIGKILL);
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chainz_core::runtime::ForkRecord;

    /// A PID that existed and is gone.
    fn dead_pid() -> u32 {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn liveness_needs_a_real_matching_process() {
        let dead = dead_pid();
        assert!(!anvil_alive(dead, None));
        // This test process is alive, but is not anvil.
        let me = std::process::id();
        assert!(pid_exists(me));
        assert!(!anvil_alive(me, None));
        assert!(!anvil_alive(0, None));
    }

    #[test]
    fn anvil_args() {
        let spec = AnvilSpec {
            fork_url: "http://127.0.0.1:9/tok",
            port: 8601,
            block: None,
            chain_id: None,
        };
        assert_eq!(
            spec.args().join(" "),
            "--fork-url http://127.0.0.1:9/tok --auto-impersonate --port 8601"
        );
        let spec = AnvilSpec {
            block: Some(123),
            chain_id: Some(99),
            ..spec
        };
        assert!(
            spec.args()
                .join(" ")
                .ends_with("--port 8601 --fork-block-number 123 --chain-id 99")
        );
        // What we spawn is what we recognise, and nothing else.
        let argv: Vec<String> = std::iter::once("/nix/store/x/bin/anvil".to_string())
            .chain(spec.args())
            .collect();
        assert!(AnvilSpec::started(&argv, Some(8601)));
        assert!(AnvilSpec::started(&argv, None));
        assert!(!AnvilSpec::started(&argv, Some(8602)));
        let mut other = argv.clone();
        other[0] = "/bin/sleep".into();
        assert!(!AnvilSpec::started(&other, Some(8601)));
    }

    #[test]
    fn reaping_signals_only_a_process_that_is_provably_anvil() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        // A dead fork (no lock holder) whose record names this test
        // process as its anvil: reaped, and nothing is signalled.
        let record = ForkRecord {
            format: ForkRecord::FORMAT,
            name: "gone-fork".into(),
            pid: dead_pid(),
            anvil_pid: std::process::id(),
            port: 8601,
            parent: "base".into(),
            chain_id: 8453,
            fork_block: None,
            started_at: 0,
            ready: true,
        };
        rt.ensure_dir().unwrap();
        std::fs::write(
            rt.forks_dir().join("gone-fork.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        assert_eq!(reap(&rt).unwrap(), ["gone-fork"]);
        assert!(std::fs::read_dir(rt.forks_dir()).unwrap().next().is_none());
    }
}
