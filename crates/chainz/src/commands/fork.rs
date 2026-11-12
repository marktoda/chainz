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

mod endpoint;
mod fund;
mod ls_stop;
mod process;
mod start;

/// `chainz fork …`.
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

#[cfg(test)]
mod tests {
    use chainz_core::forks::RESERVED_FORK_NAMES;
    use clap::CommandFactory;

    /// A fork named like a `fork` subcommand could never be stopped by
    /// name: the reserved list is exactly the subcommands and aliases.
    #[test]
    fn reserved_fork_names_are_the_fork_subcommands() {
        let cli = crate::cli::Opt::command();
        let fork = cli.find_subcommand("fork").expect("fork command");
        let mut names: Vec<&str> = fork
            .get_subcommands()
            .filter(|c| c.get_name() != "help")
            .flat_map(|c| std::iter::once(c.get_name()).chain(c.get_all_aliases()))
            .collect();
        names.sort_unstable();
        let mut reserved = RESERVED_FORK_NAMES.to_vec();
        reserved.sort_unstable();
        assert_eq!(names, reserved);
    }
}
