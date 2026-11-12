//! `chainz top`: the live dashboard (`chainz-tui`).

use crate::cli::TopArgs;
use crate::load::open;
use crate::ui;
use anyhow::Result;
use chainz_core::runtime::Runtime;
use chainz_tui::{Mode, Options};

/// `e` in the dashboard: `chainz exec <chain>` (a subshell), as the user
/// would run it.
fn shell(chain: &str) -> std::io::Result<()> {
    let status = std::process::Command::new(std::env::current_exe()?)
        .args(["exec", chain])
        .status()?;
    exited(status)
}

/// Any exit status is a shell the user left: an interactive shell exits
/// with the status of the last command run in it. Only a shell killed by
/// a signal is an error.
fn exited(status: std::process::ExitStatus) -> std::io::Result<()> {
    match status.code() {
        Some(_) => Ok(()),
        None => Err(std::io::Error::other(format!(
            "`chainz exec` was stopped by a signal ({status})"
        ))),
    }
}

/// `top [--once [--json]] [--reveal]`.
pub fn run(args: &TopArgs) -> Result<i32> {
    let chainz = open()?;
    let runtime = Runtime::resolve()?;
    let mode = if args.once {
        Mode::Once {
            json: args.json,
            warn: Box::new(|text| eprintln!("{}", ui::warn(text))),
        }
    } else {
        Mode::Live {
            shell: Box::new(shell),
        }
    };
    let options = Options {
        reveal: args.reveal,
        mode,
    };
    chainz_tui::run(chainz, runtime, crate::load::health_store(), options)
}

#[cfg(test)]
mod tests {
    use super::exited;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    #[test]
    fn only_a_shell_killed_by_a_signal_is_reported() {
        assert!(exited(ExitStatus::from_raw(0)).is_ok());
        // `false; exit`: the last command's status, not a failure.
        assert!(exited(ExitStatus::from_raw(1 << 8)).is_ok());
        // Killed by SIGKILL: no exit code.
        let killed = exited(ExitStatus::from_raw(9)).unwrap_err();
        assert!(killed.to_string().contains("signal"), "{killed}");
    }
}
