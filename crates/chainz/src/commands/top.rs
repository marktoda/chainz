//! `chainz top`: the live dashboard (`chainz-tui`).

use crate::cli::TopArgs;
use crate::load::open;
use crate::{net, ui};
use anyhow::Result;
use chainz_core::runtime::Runtime;
use chainz_tui::{Mode, Options};

/// `e` in the dashboard: `chainz shell <chain>`, as the user would run it.
/// A shell that does not exit cleanly is an error, so the dashboard says
/// so instead of "back from `<chain>`".
fn shell(chain: &str) -> std::io::Result<()> {
    let status = std::process::Command::new(std::env::current_exe()?)
        .args(["shell", chain])
        .status()?;
    exited(status)
}

fn exited(status: std::process::ExitStatus) -> std::io::Result<()> {
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(std::io::Error::other(format!(
            "`chainz shell` exited with status {code}"
        ))),
        None => Err(std::io::Error::other(format!(
            "`chainz shell` was stopped by a signal ({status})"
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
            shell: Some(Box::new(shell)),
        }
    };
    let options = Options {
        reveal: args.reveal,
        mode,
    };
    chainz_tui::run(chainz, runtime, net::health_store(), options)
}

#[cfg(test)]
mod tests {
    use super::exited;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    #[test]
    fn a_shell_that_fails_is_reported() {
        assert!(exited(ExitStatus::from_raw(0)).is_ok());
        let failed = exited(ExitStatus::from_raw(2 << 8)).unwrap_err();
        assert_eq!(failed.to_string(), "`chainz shell` exited with status 2");
        // Killed by SIGKILL: no exit code.
        let killed = exited(ExitStatus::from_raw(9)).unwrap_err();
        assert!(killed.to_string().contains("signal"), "{killed}");
    }
}
