//! Process plumbing for forks: signals, liveness, the anvil child, and
//! reaping what a dead supervisor left behind.
//!
//! A [`ForkRecord`](chainz_core::runtime::ForkRecord) is what other
//! commands read (`exec`, `top`), and it is written only once the fork is
//! up. Each fork also has a sidecar, `forks/<name>.pids`, created
//! (exclusively) before anvil starts and naming the supervisor, then
//! anvil and its port: it is both the claim on the name and what lets a
//! later `chainz fork` find an orphaned anvil, including one whose
//! supervisor died before registering.

use crate::sys::{self, signal};
use anyhow::{Context as _, Result, anyhow, bail};
use chainz_core::paths;
use chainz_core::runtime::Runtime;
use serde::{Deserialize, Serialize};
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

/// Whether `pid` is a live `chainz fork` supervisor. A recycled PID that
/// now belongs to something else counts as dead.
pub fn supervisor_alive(pid: u32) -> bool {
    if !pid_exists(pid) {
        return false;
    }
    match proc_info(pid) {
        Some(info) => !info.zombie && info.args.iter().any(|a| a == "fork"),
        // Exists but can't be inspected: assume it is ours.
        None => true,
    }
}

/// Whether `pid` is a live anvil on `port`. Only a process that provably
/// looks like our anvil is ever signalled.
pub fn anvil_alive(pid: u32, port: Option<u16>) -> bool {
    if !pid_exists(pid) {
        return false;
    }
    let Some(info) = proc_info(pid) else {
        return false;
    };
    let is_anvil = info
        .args
        .first()
        .is_some_and(|a| a.rsplit('/').next() == Some("anvil"));
    let on_port = match port {
        Some(port) => info
            .args
            .windows(2)
            .any(|w| w[0] == "--port" && w[1] == port.to_string()),
        None => true,
    };
    !info.zombie && is_anvil && on_port
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

/// Stop a supervisor: SIGTERM (it stops anvil and removes its record),
/// then SIGKILL after [`SUPERVISOR_GRACE`].
pub fn stop_supervisor(pid: u32) {
    if !supervisor_alive(pid) {
        return;
    }
    if sys::signal_process(pid, libc::SIGTERM).is_err() {
        return;
    }
    if !wait_until(SUPERVISOR_GRACE, || !supervisor_alive(pid)) {
        let _ = sys::signal_process(pid, libc::SIGKILL);
        wait_until(Duration::from_secs(2), || !supervisor_alive(pid));
    }
}

/// The sidecar: `forks/<name>.pids`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pids {
    /// Supervisor process ID.
    pub supervisor: u32,
    /// Anvil's process ID, once spawned.
    pub anvil: Option<u32>,
    /// Anvil's port, once chosen.
    pub port: Option<u16>,
}

fn pids_path(runtime: &Runtime, name: &str) -> PathBuf {
    runtime.forks_dir().join(format!("{name}.pids"))
}

fn read_pids(runtime: &Runtime, name: &str) -> Option<Pids> {
    serde_json::from_slice(&std::fs::read(pids_path(runtime, name)).ok()?).ok()
}

/// Update the sidecar (atomic).
pub fn write_pids(runtime: &Runtime, name: &str, pids: &Pids) -> Result<()> {
    let bytes = serde_json::to_vec(pids)?;
    chainz_sys::write_private(&pids_path(runtime, name), &bytes)
        .context("unable to write the fork's pid file")
}

/// Claim `name` for this process. Fails if another live supervisor has it.
pub fn claim(runtime: &Runtime, name: &str) -> Result<()> {
    use std::io::Write;
    runtime
        .ensure_dir()
        .context("unable to create the runtime directory")?;
    let pids = Pids {
        supervisor: std::process::id(),
        anvil: None,
        port: None,
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(pids_path(runtime, name)) {
        Ok(mut file) => Ok(file.write_all(&serde_json::to_vec(&pids)?)?),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => bail!(
            "fork '{name}' is already starting or running; stop it with `chainz fork stop {name}` or pick another --name"
        ),
        Err(e) => Err(e).context("unable to claim the fork name"),
    }
}

/// Remove a fork's record and sidecar.
pub fn forget(runtime: &Runtime, name: &str) {
    let _ = runtime.remove_fork(name);
    let _ = std::fs::remove_file(pids_path(runtime, name));
}

/// Remove a fork's record and sidecar if this process still owns them.
pub fn release(runtime: &Runtime, name: &str) {
    if read_pids(runtime, name).is_none_or(|p| p.supervisor == std::process::id()) {
        forget(runtime, name);
    }
}

/// A fork known to the registry: registered, or still starting.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Fork name.
    pub name: String,
    /// Supervisor PID.
    pub supervisor: u32,
    /// Anvil PID, when known.
    pub anvil: Option<u32>,
    /// Anvil port, when known.
    pub port: Option<u16>,
}

/// Every fork with a record or a sidecar.
pub fn entries(runtime: &Runtime) -> Vec<Entry> {
    let mut names: Vec<String> = runtime
        .list_forks()
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.name)
        .collect();
    if let Ok(dir) = std::fs::read_dir(runtime.forks_dir()) {
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "pids")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| {
            let record = runtime.read_fork(&name).ok().flatten();
            let pids = read_pids(runtime, &name);
            let supervisor = record
                .as_ref()
                .map(|r| r.pid)
                .or(pids.as_ref().map(|p| p.supervisor))?;
            Some(Entry {
                supervisor,
                anvil: pids.as_ref().and_then(|p| p.anvil),
                port: record
                    .as_ref()
                    .map(|r| r.port)
                    .or(pids.as_ref().and_then(|p| p.port)),
                name,
            })
        })
        .collect()
}

/// Remove every fork whose supervisor is dead, killing its orphaned anvil
/// (Linux's `PR_SET_PDEATHSIG` normally has already; macOS has no
/// equivalent). Returns the names reaped.
pub fn reap(runtime: &Runtime) -> Vec<String> {
    let mut reaped = Vec::new();
    for entry in entries(runtime) {
        if supervisor_alive(entry.supervisor) {
            continue;
        }
        if let Some(anvil) = entry.anvil {
            kill_anvil(anvil, entry.port);
        }
        forget(runtime, &entry.name);
        reaped.push(entry.name);
    }
    reaped
}

/// The fork's log: anvil's output, and the supervisor's when detached.
pub fn log_path(name: &str) -> Result<PathBuf> {
    let dir = paths::state_dir()
        .context("unable to find the state directory")?
        .join("forks");
    chainz_sys::create_private_dir(&dir).context("unable to create the fork log directory")?;
    Ok(dir.join(format!("{name}.log")))
}

/// The last `lines` lines of the log, indented, for error messages.
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

impl AnvilSpec<'_> {
    /// Anvil's arguments.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--fork-url".to_string(),
            self.fork_url.to_string(),
            "--auto-impersonate".to_string(),
            "--port".to_string(),
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
}

/// Spawn anvil in its own process group, output to `log`. On Linux it gets
/// `PR_SET_PDEATHSIG`, so it dies with the supervisor even on SIGKILL.
///
/// Must be called from the supervisor's main thread: the death signal
/// fires when the *thread* that forked exits.
pub fn spawn_anvil(spec: &AnvilSpec<'_>, log: &File) -> Result<Child> {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("anvil");
    command
        .args(spec.args())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
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

/// Point this process's stdout and stderr at `log` (detached supervisor,
/// once the summary is out). Closing the inherited stdout pipe is what
/// tells the waiting parent the fork is up.
pub fn redirect_output(log: &File) {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let _ = io::stdout().flush();
    // SAFETY: dup2 onto the standard descriptors.
    unsafe {
        libc::dup2(log.as_raw_fd(), 1);
        libc::dup2(log.as_raw_fd(), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chainz_core::runtime::ForkRecord;

    fn record(name: &str, pid: u32, port: u16) -> ForkRecord {
        ForkRecord {
            name: name.into(),
            pid,
            port,
            parent: "base".into(),
            chain_id: 8453,
            fork_block: None,
            started_at: 0,
        }
    }

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
        assert!(!supervisor_alive(dead) && !anvil_alive(dead, None));
        // This test process is alive, but is neither a supervisor nor anvil.
        let me = std::process::id();
        assert!(pid_exists(me));
        assert!(!anvil_alive(me, None));
        assert!(!supervisor_alive(0));
        // A zombie is not alive.
        let mut child = Command::new("true").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!supervisor_alive(child.id()));
        child.wait().unwrap();
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
    }

    #[test]
    fn claims_are_exclusive_and_released_by_their_owner() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        claim(&rt, "base-fork").unwrap();
        let err = claim(&rt, "base-fork").unwrap_err().to_string();
        assert!(err.contains("already starting or running"), "{err}");
        // Releasing is a no-op for someone else's claim.
        let mut theirs = read_pids(&rt, "base-fork").unwrap();
        theirs.supervisor += 1;
        write_pids(&rt, "base-fork", &theirs).unwrap();
        release(&rt, "base-fork");
        assert_eq!(entries(&rt).len(), 1);
        forget(&rt, "base-fork");
        claim(&rt, "base-fork").unwrap();
        forget(&rt, "base-fork");
        assert!(entries(&rt).is_empty());
    }

    #[test]
    fn reaps_records_of_dead_supervisors_only() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        let dead = dead_pid();
        rt.write_fork(&record("gone-fork", dead, 8601)).unwrap();
        write_pids(
            &rt,
            "gone-fork",
            &Pids {
                supervisor: dead,
                anvil: Some(dead_pid()),
                port: Some(8601),
            },
        )
        .unwrap();
        // A sidecar with no record: a supervisor that died while starting.
        write_pids(
            &rt,
            "half-fork",
            &Pids {
                supervisor: dead,
                anvil: None,
                port: None,
            },
        )
        .unwrap();
        let found = entries(&rt);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].port, Some(8601));
        assert_eq!((found[1].name.as_str(), found[1].port), ("half-fork", None));
        assert_eq!(reap(&rt), vec!["gone-fork", "half-fork"]);
        assert!(rt.list_forks().unwrap().is_empty());
        assert!(std::fs::read_dir(rt.forks_dir()).unwrap().next().is_none());
    }
}
