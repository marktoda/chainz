//! Loading the store for a command: `chainz-core` queues notices
//! (migration, tightened permissions) and the bin prints them on stderr.

use crate::ui;
use anyhow::Result;
use chainz_core::{Chainz, LoadMode, StorePaths};

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
