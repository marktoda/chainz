//! `chainz top`: the live terminal dashboard.
//!
//! Layers, each ignorant of the ones after it:
//!
//! - `plan`: the config resolved into owned, thread-safe targets;
//! - `data`: background workers that poll, probe and read balances;
//! - `gateway`: the one adapter over the control protocol;
//! - `model`: the state the workers' updates fold into, and its
//!   classification rules;
//! - `app`: selection, tabs and key bindings;
//! - `ui`: pure rendering of (app, state, clock) into a buffer;
//! - `snapshot`: the `--once --json` document;
//! - `term`: the terminal and the event loop.
//!
//! The UI thread never touches the network: it reads keys, folds updates
//! from a channel and draws.

#![warn(missing_docs)]

pub(crate) mod app;
pub(crate) mod data;
pub(crate) mod fmt;
pub(crate) mod gateway;
pub(crate) mod model;
pub(crate) mod plan;
pub(crate) mod snapshot;
mod term;
pub(crate) mod ui;

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chainz_core::Chainz;
use chainz_core::runtime::Runtime;
use chainz_rpc::HealthStore;

use crate::app::{App, Tab};
use crate::data::{Shared, Tuning};
use crate::model::{State, now_ms};
use crate::plan::Plan;
use crate::ui::theme::Theme;

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
        return once(&chainz, runtime, &options);
    }
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!("`chainz top` needs a terminal; in a script use `chainz top --once --json`");
    }
    term::run(chainz, runtime, options)
}

/// The persisted health history; empty when it cannot be read.
fn load_store() -> HealthStore {
    chainz_core::paths::state_dir()
        .ok()
        .and_then(|dir| HealthStore::load(&dir).ok())
        .unwrap_or_else(|| {
            HealthStore::load(std::path::Path::new("/nonexistent/chainz"))
                .expect("a missing health file is an empty store")
        })
}

/// `--once`: poll every chain once and print the result.
fn once(chainz: &Chainz, runtime: Runtime, options: &Options) -> Result<i32> {
    let plan = Arc::new(Plan::build(chainz, options.reveal));
    let store = load_store();
    let mut state = State::new(&plan, &store);
    let (updates, _unused) = std::sync::mpsc::channel();
    let shared = Shared::new(
        store,
        runtime,
        chainz.paths().config.clone(),
        Tuning::default(),
        updates,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    for update in rt.block_on(data::collect_once(&shared, &plan)) {
        state.apply(update);
    }
    let now = now_ms();
    if options.json {
        let snapshot = snapshot::Snapshot::new(&state, now);
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        print!("{}", once_text(&state, now));
    }
    Ok(0)
}

/// The Chains table, then one Balances table per signer, as plain text.
fn once_text(state: &State, now_ms: u64) -> String {
    const WIDTH: u16 = 120;
    let rows = u16::try_from(state.chains.len()).unwrap_or(u16::MAX - 8);
    // Title bar, context line, header, rows, footer; an empty state needs room.
    let height = rows.max(6) + 4;
    let mut app = App::default();
    let mut out = String::new();
    let mut frame = |app: &App| {
        let ctx = ui::Ctx {
            app,
            state,
            theme: Theme::new(false),
            now_ms,
        };
        let text = ui::render_text(ctx, WIDTH, height);
        // Drop the tab bar and the key hints: they are for the live view.
        // In Balances the signer chips become one plain heading.
        let lines: Vec<&str> = text.lines().collect();
        let mut body = lines[1..lines.len() - 1].to_vec();
        let heading = state.signers.get(app.signer).and_then(|s| {
            let address = s.address?.to_checksum();
            (app.tab == Tab::Balances).then(|| format!(" signer {} {address}", s.name))
        });
        if let Some(heading) = &heading {
            body[0] = heading;
        }
        out.push_str(body.join("\n").replace('▸', " ").trim_end());
        out.push_str("\n\n");
    };
    frame(&app);
    app.tab = Tab::Balances;
    for (i, signer) in state.signers.iter().enumerate() {
        if signer.address.is_some() {
            app.signer = i;
            frame(&app);
        }
    }
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out.push('\n');
    out
}
