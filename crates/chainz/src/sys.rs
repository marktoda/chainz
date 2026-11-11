//! Process plumbing shared by the background servers (`serve --detach`,
//! `fork`): private log files, detaching from the terminal, signals.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::Path;

/// Open a log for appending (mode 0600 when created), optionally emptying
/// it first.
pub fn open_log(path: &Path, truncate: bool) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("unable to open {}", path.display()))?;
    if truncate {
        file.set_len(0)?;
    }
    Ok(file)
}

/// Start `command` in a new session (`setsid`): no controlling terminal,
/// its own process group, so the terminal's Ctrl-C and hangup never reach
/// it. A no-op off Unix.
pub fn detach(command: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// Send `signal` to process `pid` (or, negative, to a process group).
/// `false` when it could not be delivered.
#[cfg(unix)]
pub fn signal(pid: i32, signal: libc::c_int) -> bool {
    // SAFETY: plain syscall.
    unsafe { libc::kill(pid, signal) == 0 }
}

/// Send `signal` to process `pid`, refusing PIDs that are not a single
/// real process (0, 1, or past `i32::MAX`, which `kill` would read as a
/// group or as "every process").
#[cfg(unix)]
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_real_pids_are_signalled() {
        for pid in [0, 1, u32::MAX, i32::MAX as u32 + 1] {
            assert!(signal_process(pid, 0).is_err(), "{pid}");
        }
        assert!(signal_process(std::process::id(), 0).is_ok());
    }

    #[test]
    fn logs_are_private_and_appended() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        open_log(&path, false).unwrap().write_all(b"a").unwrap();
        open_log(&path, false).unwrap().write_all(b"b").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ab");
        open_log(&path, true).unwrap().write_all(b"c").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "c");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
