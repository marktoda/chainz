//! Stream-owned commands. Each stream replaces its stub with a real
//! handler (keep the signature; the clap surface in `cli.rs` is frozen).

use crate::cli::{ServeArgs, ServeCommand};
use anyhow::{Result, bail};

fn not_yet(command: &str, stream: &str) -> Result<i32> {
    bail!("`chainz {command}` is not yet implemented ({stream} stream)")
}

/// `top` (Dashboard stream).
pub fn top(_once: bool, _json: bool, _reveal: bool) -> Result<i32> {
    not_yet("top", "Dashboard")
}

/// `serve [status|stop|log]` (Gateway stream).
pub fn serve(cmd: Option<ServeCommand>, _args: ServeArgs) -> Result<i32> {
    let name = match cmd {
        None => "serve",
        Some(ServeCommand::Status) => "serve status",
        Some(ServeCommand::Stop) => "serve stop",
        Some(ServeCommand::Log { .. }) => "serve log",
    };
    not_yet(name, "Gateway")
}
