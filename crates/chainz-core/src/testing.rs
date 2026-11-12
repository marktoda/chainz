//! Helpers for tests in chainz crates (feature `testing`, and this crate's
//! own tests). Nothing here touches the developer's config, keyring or
//! environment.

pub use crate::signer::MemoryKeyBackend;

use crate::contract::Env;
use crate::runtime::Runtime;
use crate::store::{Chainz, LoadMode, StorePaths};

/// A store in a temporary directory: `config/` holds the config files,
/// `run/` the runtime registry. The store reads an empty environment, so
/// the developer's shell never changes a test.
pub struct TempStore {
    /// The temporary directory; removed on drop.
    pub dir: tempfile::TempDir,
    /// The store, opened strictly.
    pub chainz: Chainz,
    /// The runtime registry under `run/` (nothing created yet).
    pub runtime: Runtime,
}

impl TempStore {
    /// Write `config` (and `secrets`, when given) and open the store.
    pub fn new(config: &str, secrets: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let paths = StorePaths::in_dir(dir.path().join("config"));
        chainz_os::create_private_dir(paths.dir()).expect("config directory");
        chainz_os::write_private(paths.config(), config.as_bytes()).expect("config.toml");
        if let Some(secrets) = secrets {
            chainz_os::write_private(paths.secrets(), secrets.as_bytes()).expect("secrets.toml");
        }
        let chainz = Chainz::open_at(paths, LoadMode::Strict)
            .expect("the test config loads")
            .with_env(Env::from_fn(|_| None));
        let runtime = Runtime::at(dir.path().join("run"));
        Self {
            dir,
            chainz,
            runtime,
        }
    }

    /// The store's file paths.
    pub fn paths(&self) -> StorePaths {
        StorePaths::in_dir(self.dir.path().join("config"))
    }
}
