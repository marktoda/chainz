//! Chainz: manage EVM chain configurations, RPC endpoints, and private keys.
//!
//! This library backs the `chainz` CLI. Before 1.0, its Rust API is an
//! implementation interface rather than a stable SDK: minor releases may
//! reorganize modules while the CLI and serialized config remain compatible.

pub mod chain;
pub mod chainlist;
pub mod config;
pub mod doctor;
pub mod init;
pub mod key;
pub mod listing;
pub mod opt;
pub(crate) mod prompt;
pub mod ui;
pub mod variables;
