//! Which chain a command means when the user did not name one: the
//! `$CHAINZ_CHAIN` / default / picker steps of the resolution order
//! (`chainz_core::resolve` has the matching rules).

use crate::cli::usage_error;
use crate::prompt::{Ask, Tty};
use anyhow::{Result, bail};
use chainz_core::Chainz;
use chainz_core::resolve::{self, ChainRef, Source};
use chainz_core::runtime::Runtime;

/// How a command asks for a chain it was not given: its subcommand path
/// (for the usage error) and how to name a chain for it.
pub struct Missing<'a> {
    /// The subcommand path, as [`usage_error`] takes it.
    pub command: &'a [&'a str],
    /// How to name a chain for this command, e.g. "pass one".
    pub off_tty: &'a str,
}

/// How the context commands (and `addr add`) say how to name a chain.
pub const NAME_A_CHAIN: &str = "set $CHAINZ_CHAIN, or set a default with `chainz use <chain>`";

/// Interactive picker over configured chains. Off a TTY it is a usage
/// error (exit 2) for `missing.command`: "no chain given: {off_tty}".
pub fn pick_chain(
    chainz: &Chainz,
    ask: &mut dyn Ask,
    message: &str,
    missing: &Missing,
) -> Result<String> {
    let chains = &chainz.config().chains;
    if chains.is_empty() {
        bail!("{}", resolve::NO_CHAINS);
    }
    if !ask.is_interactive() {
        return Err(usage_error(
            missing.command,
            &format!("no chain given: {}", missing.off_tty),
        ));
    }
    let items: Vec<String> = chains
        .iter()
        .map(|(n, c)| format!("{n} ({})", c.id))
        .collect();
    let index = ask.select(message, &items, 0)?;
    Ok(chains
        .get_index(index)
        .expect("picked from the list")
        .0
        .clone())
}

/// Resolve the chain for a context command (exec, shell, env) or
/// `addr add`: explicit, `$CHAINZ_CHAIN`, default (with a stderr notice),
/// then a TTY picker.
pub fn select_target(
    chainz: &Chainz,
    runtime: &Runtime,
    explicit: Option<&str>,
    missing: &Missing,
) -> Result<ChainRef> {
    match resolve::select(chainz.config(), Some(runtime), explicit, chainz.env())? {
        Some((target, Source::Default)) => {
            note_default(target.name());
            Ok(target)
        }
        Some((target, _)) => Ok(target),
        None => pick_chain(chainz, &mut Tty, "Select a chain", missing).map(ChainRef::Configured),
    }
}

/// Say on stderr that the default chain applies (the user didn't name one).
pub fn note_default(chain: &str) {
    eprintln!("chainz: {chain} (default)");
}
