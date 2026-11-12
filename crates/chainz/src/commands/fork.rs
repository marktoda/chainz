//! `chainz fork`: disposable anvil forks, usable as chain names.
//!
//! `chainz fork CHAIN` is a **supervisor** ([`start`]): it pins one
//! upstream endpoint ([`endpoint`]), serves it to anvil through an
//! in-process loopback [`chainz_serve::PinnedProxy`] (whose upstream is the
//! endpoint itself, or a running gateway pinned to that endpoint: the
//! gateway token never reaches anvil's argv), starts anvil as a child in
//! its own process group ([`process`]), funds the chain's signer
//! ([`fund`]), and registers `forks/<name>.json` once anvil answers. It
//! then waits; Ctrl-C, SIGTERM, `chainz fork stop` or anvil exiting ends
//! the fork and removes the registry entry. Nothing is written to config.
//! `fork ls` and `fork stop` are in [`ls_stop`].
//!
//! Replay protection lives in the exec contract (`context`, `exec`): on a
//! fork `--expose-key` is refused and `ETH_KEYSTORE` is not exported, so
//! the only signing path is anvil's `--unlocked` impersonation.
//!
//! Unix only (process groups, signals, `PR_SET_PDEATHSIG`).

#[cfg(unix)]
mod endpoint;
#[cfg(unix)]
mod fund;
#[cfg(unix)]
mod ls_stop;
#[cfg(unix)]
mod process;
#[cfg(unix)]
mod start;

/// `chainz fork …`.
#[cfg(unix)]
pub fn run(
    cmd: Option<crate::cli::ForkCommand>,
    args: crate::cli::ForkArgs,
) -> anyhow::Result<i32> {
    use crate::cli::ForkCommand;
    match cmd {
        None => start::start(args),
        Some(ForkCommand::Ls { json }) => ls_stop::ls(json),
        Some(ForkCommand::Stop { name, all }) => ls_stop::stop(name.as_deref(), all),
    }
}

/// `chainz fork …`.
#[cfg(not(unix))]
pub fn run(
    _cmd: Option<crate::cli::ForkCommand>,
    _args: crate::cli::ForkArgs,
) -> anyhow::Result<i32> {
    anyhow::bail!("`chainz fork` is supported on Linux and macOS only")
}
