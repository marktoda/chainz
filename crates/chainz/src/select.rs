//! Which chain a command means when the user did not name one: the
//! `$CHAINZ_CHAIN` / default / picker steps of the resolution order
//! (`chainz_core::resolve` has the matching rules).

use crate::cli::usage_error;
use crate::prompt::Ask;
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

/// How the context commands (and `label add`) say how to name a chain.
pub const NAME_A_CHAIN: &str = "set $CHAINZ_CHAIN, or set a default with `chainz use CHAIN`";

/// Interactive picker over configured chains (an error when there are
/// none). The caller has checked that `ask` is interactive.
pub fn pick_chain(chainz: &Chainz, ask: &mut dyn Ask, message: &str) -> Result<String> {
    let chains = &chainz.config().chains;
    if chains.is_empty() {
        bail!("{}", resolve::NO_CHAINS);
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

/// The chain the user named: explicit, `$CHAINZ_CHAIN`, then the default
/// (with a stderr notice). `None` when nothing names one.
pub fn chosen(
    chainz: &Chainz,
    runtime: &Runtime,
    explicit: Option<&str>,
) -> Result<Option<ChainRef>> {
    Ok(
        match resolve::select(chainz.config(), Some(runtime), explicit, chainz.env())? {
            Some((target, Source::Default)) => {
                note_default(target.name());
                Some(target)
            }
            other => other.map(|(target, _)| target),
        },
    )
}

/// Resolve the chain for a context command (exec, shell, env) or
/// `label add`: [`chosen`], then a picker through `ask`. Off a terminal
/// a missing chain is a usage error (exit 2) for `missing.command`: "no
/// chain given: {off_tty}".
pub fn select_target(
    chainz: &Chainz,
    runtime: &Runtime,
    explicit: Option<&str>,
    ask: &mut dyn Ask,
    missing: &Missing,
) -> Result<ChainRef> {
    if let Some(target) = chosen(chainz, runtime, explicit)? {
        return Ok(target);
    }
    if !ask.is_interactive() && !chainz.config().chains.is_empty() {
        return Err(usage_error(
            missing.command,
            &format!("no chain given: {}", missing.off_tty),
        ));
    }
    pick_chain(chainz, ask, "Select a chain").map(ChainRef::Configured)
}

/// Say on stderr that the default chain applies (the user didn't name one).
fn note_default(chain: &str) {
    eprintln!("chainz: {chain} (default)");
}
