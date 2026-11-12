//! Which chain a command means when the user did not name one: the
//! `$CHAINZ_CHAIN` / default / picker steps of the resolution order
//! (`chainz_core::resolve` has the matching rules).

use crate::prompt::{self, is_tty};
use anyhow::{Result, bail};
use chainz_core::Chainz;
use chainz_core::resolve::{self, ChainRef, Source};
use chainz_core::runtime::Runtime;

/// Interactive picker over configured chains. Off a TTY it fails with
/// "no chain given: {off_tty}", where `off_tty` says how to name one for
/// this command.
pub fn pick_chain(chainz: &Chainz, message: &str, off_tty: &str) -> Result<String> {
    let chains = &chainz.config().chains;
    if chains.is_empty() {
        bail!("{}", resolve::NO_CHAINS);
    }
    if !is_tty() {
        bail!("no chain given: {off_tty}");
    }
    let items: Vec<String> = chains
        .iter()
        .map(|(n, c)| format!("{n} ({})", c.id))
        .collect();
    let index = prompt::select(message, &items, 0)?;
    Ok(chains
        .get_index(index)
        .expect("picked from the list")
        .0
        .clone())
}

/// Resolve the chain for a context command (exec, shell, env): explicit,
/// `$CHAINZ_CHAIN`, default (with a stderr notice), then a TTY picker.
pub fn select_target(
    chainz: &Chainz,
    runtime: &Runtime,
    explicit: Option<&str>,
) -> Result<ChainRef> {
    match resolve::select(chainz.config(), Some(runtime), explicit, chainz.env())? {
        Some((target, Source::Default)) => {
            note_default(target.name());
            Ok(target)
        }
        Some((target, _)) => Ok(target),
        None => pick_chain(
            chainz,
            "Select a chain",
            "pass one, set $CHAINZ_CHAIN, or set a default with `chainz use <chain>`",
        )
        .map(ChainRef::Configured),
    }
}

/// Say on stderr that the default chain applies (the user didn't name one).
pub fn note_default(chain: &str) {
    eprintln!("chainz: {chain} (default)");
}
