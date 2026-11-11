//! Stream-owned commands. Each stream replaces its stub with a real
//! handler (keep the signature; the clap surface in `cli.rs` is frozen).

use crate::cli::{AddrCommand, ForkArgs, ForkCommand};
use anyhow::{Result, bail};

fn not_yet(command: &str, stream: &str) -> Result<i32> {
    bail!("`chainz {command}` is not yet implemented ({stream} stream)")
}

/// `top` (Dashboard stream).
pub fn top(_once: bool, _json: bool, _reveal: bool) -> Result<i32> {
    not_yet("top", "Dashboard")
}

/// `fork [ls|stop]` (Forks stream).
pub fn fork(cmd: Option<ForkCommand>, _args: ForkArgs) -> Result<i32> {
    let name = match cmd {
        None => "fork",
        Some(ForkCommand::Ls) => "fork ls",
        Some(ForkCommand::Stop { .. }) => "fork stop",
    };
    not_yet(name, "Forks")
}

/// `addr …` (Address book stream).
pub fn addr(cmd: AddrCommand) -> Result<i32> {
    let name = match cmd {
        AddrCommand::Add { .. } => "addr add",
        AddrCommand::Ls { .. } => "addr ls",
        AddrCommand::Rm { .. } => "addr rm",
        AddrCommand::Find { .. } => "addr find",
    };
    not_yet(name, "Address book")
}

/// `label` (Address book stream).
pub fn label(_chain: Option<String>) -> Result<i32> {
    not_yet("label", "Address book")
}

/// `doctor --tokens` (Address book stream).
pub fn doctor_tokens() -> Result<i32> {
    not_yet("doctor --tokens", "Address book")
}
