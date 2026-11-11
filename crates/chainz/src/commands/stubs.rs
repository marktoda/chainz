//! Stream-owned commands. Each stream replaces its stub with a real
//! handler (keep the signature; the clap surface in `cli.rs` is frozen).

use anyhow::{Result, bail};

fn not_yet(command: &str, stream: &str) -> Result<i32> {
    bail!("`chainz {command}` is not yet implemented ({stream} stream)")
}

/// `top` (Dashboard stream).
pub fn top(_once: bool, _json: bool, _reveal: bool) -> Result<i32> {
    not_yet("top", "Dashboard")
}
