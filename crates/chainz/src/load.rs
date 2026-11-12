//! Loading the store for a command: `chainz-core` queues notices
//! (migration, tightened permissions) and the bin prints them on stderr.
//! Also the recorded endpoint health (`chainz-rpc`'s [`HealthStore`]: a
//! file read, no runtime and no sockets), which `ls` shows.

use crate::ui;
use anyhow::Result;
use chainz_core::{Chainz, LoadMode, StorePaths, paths};
use chainz_rpc::HealthStore;

/// Load the config (strict), migrating v1 on first run.
pub fn open() -> Result<Chainz> {
    with_notices(Chainz::open()?)
}

/// Load leniently (`doctor`, `edit`): an invalid config still loads.
pub fn open_lenient() -> Result<Chainz> {
    with_notices(Chainz::open_at(StorePaths::resolve()?, LoadMode::Lenient)?)
}

/// Print `chainz`'s notices on stderr as they happen.
fn with_notices(mut chainz: Chainz) -> Result<Chainz> {
    chainz.set_notice_sink(ui::notice);
    Ok(chainz)
}

/// The state directory (endpoint health history, capability tags), or
/// `None` with a warning on stderr when there is none.
pub fn state_dir() -> Option<std::path::PathBuf> {
    paths::state_dir()
        .map_err(|e| {
            eprintln!(
                "{}",
                ui::warn(&format!("no endpoint health history: {e:#}"))
            );
        })
        .ok()
}

/// The persisted endpoint health store. When it can't be read, that is
/// said on stderr and an empty store is used (one that still saves, when
/// there is a state directory).
pub fn health_store() -> HealthStore {
    let Some(dir) = state_dir() else {
        return HealthStore::empty();
    };
    HealthStore::load(&dir).unwrap_or_else(|e| {
        eprintln!(
            "{}",
            ui::warn(&format!("could not read the endpoint health history: {e}"))
        );
        HealthStore::new(&dir)
    })
}
