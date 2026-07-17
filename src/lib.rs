//! Chainz: manage EVM chain configurations, RPC endpoints, and private keys.
//!
//! Its deliberately small public interface contains the binary entry point
//! and serialized configuration records. Before 1.0, those Rust model types
//! may move while the CLI and serialized configuration remain compatible.

mod chain;
mod chainlist;
mod cli;
mod config;
mod doctor;
mod endpoint;
mod init;
mod key;
mod listing;
mod opt;
mod prompt;
mod ui;
mod variables;

pub use cli::run_cli;

/// Serialized configuration records supported by the pre-1.0 crate interface.
pub mod model {
    pub use crate::chain::ChainDefinition;
    pub use crate::config::{Config, LEGACY_CONFIG_FILE};
    pub use crate::key::{Key, KeyType};
    pub use crate::variables::GlobalVariables;
}
