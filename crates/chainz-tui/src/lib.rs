//! `chainz top`: the live terminal dashboard.
//!
//! The UI thread never touches the network: it reads keys, folds updates
//! from a channel and draws.
//!
//! Internal to [chainz](https://crates.io/crates/chainz), published only so
//! the binary can be installed: there is no stable API, and any release may
//! break it.

// Modules, from what changes least to what changes most:
//
// - `plan`: the config resolved into owned, thread-safe targets, and the
//   display data (`*Info`) the rest of the crate sees;
// - `model`: the shared vocabulary: what workers report (`Update`,
//   `Refresh`), the state it folds into, and the
//   classification rules;
// - `gateway`: the one adapter over the control protocol, into model
//   types;
// - `data`: background workers that poll, probe, read balances and watch
//   the gateway and the config, reporting model updates;
// - `view`: the state as each tab's rows, every figure derived once;
// - `app`: selection, tabs and key bindings;
// - `ui`: rendering of (app, view, clock) into a buffer, and the
//   message and table text `--once` shares;
// - `snapshot` and `once`: the `--once` JSON document and text tables,
//   from the same view;
// - `term`: the terminal and the event loop;
// - `fmt`: number, time and text formatting for both outputs; `golden`
//   pins the `--once` output.

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

pub use once::WarnFn;
pub use term::ShellFn;

/// How `chainz top` was invoked.
pub struct Options {
    /// Show and copy raw endpoint URLs instead of redacted ones.
    pub reveal: bool,
    /// Live or one snapshot.
    pub mode: Mode,
}

/// The dashboard, or one snapshot.
pub enum Mode {
    /// The interactive dashboard.
    Live {
        /// How to open a shell for a chain (`e`).
        shell: ShellFn,
    },
    /// Poll every chain once, print, and exit.
    Once {
        /// Print JSON instead of text tables.
        json: bool,
        /// Where warnings go.
        warn: WarnFn,
    },
}

/// Entry point for `chainz top`, with the persisted health history
/// (`store`) the dashboard shows and extends. Returns the process exit
/// code.
pub fn run(chainz: Chainz, runtime: Runtime, store: HealthStore, options: Options) -> Result<i32> {
    match options.mode {
        Mode::Once { json, warn } => once::run(&chainz, runtime, store, options.reveal, json, warn),
        Mode::Live { shell } => {
            if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
                bail!("`chainz top` needs a terminal; in a script use `chainz top --once --json`");
            }
            term::run(chainz, runtime, store, options.reveal, shell)
        }
    }
}
