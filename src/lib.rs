//! Chainz: manage EVM chain configurations, RPC endpoints, and private keys.
//!
//! This library backs the `chainz` CLI. The modules are exposed so the
//! binary and integration tests can drive them directly.

pub mod chain;
pub mod chainlist;
pub mod config;
pub mod doctor;
pub mod init;
pub mod key;
pub mod listing;
pub mod opt;
pub mod ui;
pub mod variables;
