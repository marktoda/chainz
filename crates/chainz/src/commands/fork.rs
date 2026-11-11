//! `chainz fork`: disposable anvil forks, usable as chain names.
//!
//! `chainz fork CHAIN` is a **supervisor**: it pins one upstream endpoint,
//! serves it to anvil through an in-process loopback proxy ([`proxy`],
//! whose upstream is the endpoint itself, or a running gateway pinned to
//! that endpoint: the gateway token never reaches anvil's argv), starts
//! anvil as a child in its own process group, funds the chain's signer
//! ([`fund`]), and registers `forks/<name>.json` once anvil answers. It
//! then waits; Ctrl-C, SIGTERM, `chainz fork stop` or anvil exiting ends
//! the fork and removes the registry entry. Nothing is written to config.
//!
//! Replay protection lives in the exec contract (`context`, `exec`): on a
//! fork `--expose-key` is refused and `ETH_KEYSTORE` is not exported, so
//! the only signing path is anvil's `--unlocked` impersonation.
//!
//! Unix only (process groups, signals, `PR_SET_PDEATHSIG`).

#[cfg(unix)]
mod fund;
#[cfg(unix)]
mod process;
#[cfg(unix)]
mod proxy;
#[cfg(unix)]
mod supervisor;

#[cfg(unix)]
pub use supervisor::run;

/// `chainz fork …`.
#[cfg(not(unix))]
pub fn run(
    _cmd: Option<crate::cli::ForkCommand>,
    _args: crate::cli::ForkArgs,
) -> anyhow::Result<i32> {
    anyhow::bail!("`chainz fork` is supported on Linux and macOS only")
}
