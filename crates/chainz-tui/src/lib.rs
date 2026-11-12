//! `chainz top`: the live terminal dashboard.
//!
//! Modules, from what changes least to what changes most:
//!
//! - `plan`: the config resolved into owned, thread-safe targets, and the
//!   display data (`*Info`) the rest of the crate sees;
//! - `model`: the shared vocabulary: what workers report ([`model::Update`],
//!   [`model::Refresh`]), the state it folds into, and the
//!   classification rules;
//! - `gateway`: the one adapter over the control protocol, into model
//!   types;
//! - `data`: background workers that poll, probe, read balances and watch
//!   the gateway and the config, reporting model updates;
//! - `view`: the state as each tab's rows, every figure derived once;
//! - `app`: selection, tabs and key bindings;
//! - `ui`: pure rendering of (app, view, clock) into a buffer;
//! - `snapshot` and `once`: the `--once` JSON document and text tables,
//!   from the same view;
//! - `term`: the terminal and the event loop.
//!
//! The UI thread never touches the network: it reads keys, folds updates
//! from a channel and draws.

#![warn(missing_docs)]

pub(crate) mod app;
pub(crate) mod data;
pub(crate) mod fmt;
pub(crate) mod gateway;
#[cfg(test)]
mod golden;
pub(crate) mod model;
mod once;
pub(crate) mod plan;
pub(crate) mod snapshot;
mod term;
pub(crate) mod ui;
pub(crate) mod view;

use std::io::IsTerminal;

use anyhow::{Result, bail};
use chainz_core::Chainz;
use chainz_core::runtime::Runtime;
use chainz_rpc::HealthStore;

/// Runs `chainz shell <chain>` for the `e` key and returns when it exits:
/// an error, including a shell that did not exit cleanly, is shown in the
/// dashboard's footer.
pub type ShellFn = Box<dyn FnMut(&str) -> std::io::Result<()>>;

/// How `chainz top` was invoked.
pub struct Options {
    /// Print one snapshot and exit.
    pub once: bool,
    /// With `once`: print JSON.
    pub json: bool,
    /// Show and copy raw endpoint URLs instead of redacted ones.
    pub reveal: bool,
    /// How to open a shell for a chain; without it, `e` is unavailable.
    pub shell: Option<ShellFn>,
}

/// Entry point for `chainz top`. Returns the process exit code.
pub fn run(chainz: Chainz, runtime: Runtime, options: Options) -> Result<i32> {
    if options.json && !options.once {
        bail!("`--json` needs `--once`: chainz top --once --json");
    }
    if options.once {
        return once::run(&chainz, runtime, &options);
    }
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!("`chainz top` needs a terminal; in a script use `chainz top --once --json`");
    }
    term::run(chainz, runtime, options)
}

/// The persisted health history; empty when it cannot be read.
fn load_store() -> HealthStore {
    match chainz_core::paths::state_dir() {
        Ok(dir) => HealthStore::load(&dir).unwrap_or_else(|_| HealthStore::new(&dir)),
        Err(_) => HealthStore::empty(),
    }
}
