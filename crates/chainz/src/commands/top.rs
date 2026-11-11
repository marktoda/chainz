//! `chainz top`: the live dashboard (`chainz-tui`).

use super::open;
use anyhow::Result;
use chainz_core::runtime::Runtime;
use chainz_tui::Options;

/// `e` in the dashboard: `chainz shell <chain>`, as the user would run it.
fn shell(chain: &str) -> std::io::Result<()> {
    std::process::Command::new(std::env::current_exe()?)
        .args(["shell", chain])
        .status()
        .map(|_| ())
}

/// `top [--once [--json]] [--reveal]`.
pub fn run(once: bool, json: bool, reveal: bool) -> Result<i32> {
    let chainz = open()?;
    let runtime = Runtime::resolve()?;
    chainz_tui::run(
        chainz,
        runtime,
        Options {
            once,
            json,
            reveal,
            shell: Some(Box::new(shell)),
        },
    )
}
