//! Process plumbing shared by the background servers (`serve`, `fork`):
//! their private logs, running this program in the background until it
//! has started ([`launch_detached`], [`Detached::wait_started`],
//! [`detached_ready`]), and signals.

use anyhow::{Context, Result};
use chainz_os::LogMode;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Open one of chainz's own logs (under the state directory), creating
/// its directory owner-only and the file mode 0600 when missing.
pub fn open_log(path: &Path, mode: LogMode) -> Result<File> {
    open_state_log(path, mode).with_context(|| format!("unable to open {}", path.display()))
}

fn open_state_log(path: &Path, mode: LogMode) -> io::Result<File> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        chainz_os::create_private_dir(dir)?;
    }
    chainz_os::open_private_log(path, mode)
}

/// Set (to `1`) in the environment of a process [`launch_detached`]
/// started: the background copy knows it is one.
pub const DETACHED_ENV: &str = "CHAINZ_DETACHED";

/// The flag [`launch_detached`] drops from the re-executed command line.
const DETACH_FLAG: &str = "--detach";

/// Whether this process is the background copy [`launch_detached`]
/// started.
pub fn is_detached() -> bool {
    std::env::var_os(DETACHED_ENV).is_some()
}

/// A background copy of this process, from [`launch_detached`].
#[derive(Debug)]
pub struct Detached {
    child: Child,
    ready: Option<ChildStdout>,
}

/// Run this program again in the background: the full command line minus
/// [`DETACH_FLAG`], marked with [`DETACHED_ENV`], in a new session (`setsid`:
/// no controlling terminal, so the terminal's Ctrl-C and hangup never reach
/// it). Its stdin is empty, its stderr is the private log at `log` (opened
/// per `mode`), and its stdout is a pipe back to us: the child prints what
/// the user should see, then closes the pipe with [`detached_ready`].
pub fn launch_detached(log: &Path, mode: LogMode) -> io::Result<Detached> {
    use std::os::unix::process::CommandExt;
    let log = open_state_log(log, mode)?;
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .args(std::env::args_os().skip(1).filter(|arg| arg != DETACH_FLAG))
        .env(DETACHED_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let ready = child.stdout.take();
    Ok(Detached { child, ready })
}

/// How a background start ended, from [`Detached::wait_started`].
#[derive(Debug)]
pub enum Started {
    /// Ready and registered: what it printed for the user.
    Up(String),
    /// It exited without registering (it was waited for).
    Failed(ExitStatus),
    /// Not ready in time (it was stopped: SIGTERM, then SIGKILL after the
    /// grace period).
    TimedOut,
}

impl Detached {
    /// The background process's ID.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait up to `timeout` for the child to be ready, then ask
    /// `registered` whether it registered itself under its PID (in
    /// `serve.json`, a fork record): a child that got ready without
    /// registering has failed, and is waited for. A child not ready in time
    /// is stopped: SIGTERM, so it cleans up after itself, and SIGKILL if it
    /// is still there after `grace`. An error from `registered` leaves the
    /// child running.
    pub fn wait_started(
        &mut self,
        timeout: Duration,
        grace: Duration,
        registered: impl FnOnce(u32) -> Result<bool>,
    ) -> Result<Started> {
        let said = match self.wait_ready(Some(timeout)) {
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                let _ = self.stop(grace);
                return Ok(Started::TimedOut);
            }
            said => said.context("lost the background process")?,
        };
        if registered(self.pid())? {
            return Ok(Started::Up(said));
        }
        Ok(Started::Failed(self.child.wait()?))
    }

    /// Wait until the child is ready (it closed its stdout, see
    /// [`detached_ready`]) or has exited, and return what it printed. An
    /// [`io::ErrorKind::TimedOut`] error after `timeout`, if one is given.
    fn wait_ready(&mut self, timeout: Option<Duration>) -> io::Result<String> {
        use std::os::fd::AsRawFd;
        let Some(mut pipe) = self.ready.take() else {
            return Ok(String::new());
        };
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let wait_ms = match deadline {
                None => -1,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        self.ready = Some(pipe);
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the background process did not get ready in time",
                        ));
                    }
                    i32::try_from(left.as_millis().max(1)).unwrap_or(i32::MAX)
                }
            };
            let mut poll = libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the duration of the call.
            match unsafe { libc::poll(&mut poll, 1, wait_ms) } {
                -1 => {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
                0 => {}
                _ => match pipe.read(&mut chunk) {
                    Ok(0) => return Ok(String::from_utf8_lossy(&out).into_owned()),
                    Ok(n) => out.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                },
            }
        }
    }

    /// Stop the child: SIGTERM, SIGKILL if it outlives `grace`, then reap
    /// it.
    fn stop(&mut self, grace: Duration) -> io::Result<()> {
        signal_process(self.pid(), libc::SIGTERM)?;
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.child.kill()?;
        self.child.wait().map(drop)
    }
}

/// In the background copy, once it is ready: flush stdout and point it at
/// the log (its stderr), which closes the pipe [`Detached::wait_ready`]
/// waits on. Does nothing in a process [`launch_detached`] did not start.
pub fn detached_ready() -> io::Result<()> {
    if !is_detached() {
        return Ok(());
    }
    io::stdout().flush()?;
    // SAFETY: dup2 of the standard descriptors onto each other.
    if unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Send `signal` to process `pid` (or, negative, to a process group).
/// `false` when it could not be delivered.
pub fn signal(pid: i32, signal: libc::c_int) -> bool {
    // SAFETY: plain syscall.
    unsafe { libc::kill(pid, signal) == 0 }
}

/// Send `signal` to process `pid`, refusing PIDs that are not a single
/// real process (0, 1, or past `i32::MAX`, which `kill` would read as a
/// group or as "every process").
pub fn signal_process(pid: u32, sig: libc::c_int) -> std::io::Result<()> {
    let raw = i32::try_from(pid)
        .ok()
        .filter(|p| *p > 1)
        .ok_or_else(|| std::io::Error::other(format!("{pid} is not a process ID to signal")))?;
    if signal(raw, sig) {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn background(script: &str) -> Detached {
        let mut child = Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ready = child.stdout.take();
        Detached { child, ready }
    }

    #[test]
    fn readiness_is_the_child_closing_its_stdout() {
        // Ready while still running: what it printed comes back.
        let mut up = background("printf 'up\n'; exec 1>&2; sleep 5");
        let started = Instant::now();
        let said = up.wait_ready(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(said, "up\n");
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(up.child.try_wait().unwrap().is_none(), "still running");
        up.stop(Duration::ZERO).unwrap();

        // Exiting counts too; the caller tells the two apart.
        let mut failed = background("printf 'no'; exit 3");
        assert_eq!(failed.wait_ready(None).unwrap(), "no");
        assert_eq!(failed.child.wait().unwrap().code(), Some(3));

        // Never ready: a timeout, and the child is left to the caller.
        let mut stuck = background("sleep 5");
        let err = stuck
            .wait_ready(Some(Duration::from_millis(100)))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        stuck.stop(Duration::ZERO).unwrap();
    }

    /// A child not ready in time gets SIGTERM first, so it can clean up,
    /// and SIGKILL only once the grace period is over.
    #[test]
    fn a_late_child_is_terminated_before_it_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let cleaned = dir.path().join("cleaned");
        let mut polite = background(&format!(
            "trap 'touch {}; exit 0' TERM; while :; do sleep 0.05; done",
            cleaned.display()
        ));
        // Let sh install its trap first.
        std::thread::sleep(Duration::from_millis(200));
        let started = polite
            .wait_started(Duration::from_millis(100), Duration::from_secs(10), |_| {
                Ok(true)
            })
            .unwrap();
        assert!(matches!(started, Started::TimedOut));
        assert!(cleaned.exists(), "the child ran its SIGTERM cleanup");

        let mut stubborn = background("trap '' TERM; while :; do sleep 0.05; done");
        std::thread::sleep(Duration::from_millis(200));
        let begun = Instant::now();
        let started = stubborn
            .wait_started(
                Duration::from_millis(100),
                Duration::from_millis(300),
                |_| Ok(true),
            )
            .unwrap();
        assert!(matches!(started, Started::TimedOut));
        assert!(
            begun.elapsed() >= Duration::from_millis(400),
            "it waited the grace"
        );
        assert!(
            stubborn.child.try_wait().unwrap().is_some(),
            "killed and reaped"
        );
    }

    #[test]
    fn only_real_pids_are_signalled() {
        for pid in [0, 1, u32::MAX, i32::MAX as u32 + 1] {
            assert!(signal_process(pid, 0).is_err(), "{pid}");
        }
        assert!(signal_process(std::process::id(), 0).is_ok());
    }
}
