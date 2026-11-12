//! Process plumbing shared by the background servers (`serve`, `fork`):
//! private log files and signals. Detaching is `chainz_sys::launch_detached`.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::Path;

pub use chainz_sys::LogMode;

/// Open a private log (mode 0600 when created).
pub fn open_log(path: &Path, mode: LogMode) -> Result<File> {
    chainz_sys::open_private_log(path, mode)
        .with_context(|| format!("unable to open {}", path.display()))
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

    #[test]
    fn only_real_pids_are_signalled() {
        for pid in [0, 1, u32::MAX, i32::MAX as u32 + 1] {
            assert!(signal_process(pid, 0).is_err(), "{pid}");
        }
        assert!(signal_process(std::process::id(), 0).is_ok());
    }
}
