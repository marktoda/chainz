//! JSON-RPC client, endpoint probes, ranking, capability tags, health history,
//! [`HttpUpstream`] forwarding, the gateway control
//! API types and the chainlist registry client for chainz.
//!
//! This crate is async (tokio) and deliberately does not depend on
//! `chainz-core`: callers resolve config endpoints into [`Target`]s (with all
//! `${VAR}` references already expanded) and hand them in.
//!
//! Internal to [chainz](https://crates.io/crates/chainz), published only so
//! the binary can be installed: there is no stable API, and any release may
//! break it.
//!
//! # Secrets
//!
//! Endpoint URLs and header values may embed API keys. No error message and no
//! `Debug` output produced by this crate contains an endpoint URL or a header
//! value; header *names* may appear.

#![warn(missing_docs)]

pub mod abi;
pub mod capabilities;
pub mod chainlist;
pub mod client;
pub mod control;
pub mod error;
pub mod health;
pub mod hex;
pub mod history;
pub mod probe;
pub mod target;
pub mod upstream;

// The items most callers need, at the root. Import these from here (not
// from their module), so each has one import path.
pub use capabilities::{Capabilities, probe_capabilities};
pub use client::{BlockHeader, BlockId, RpcClient};
pub use error::{ErrorClass, RpcError};
pub use health::{EndpointProbe, Health, Ranking, classify};
pub use history::{HealthSample, HealthStore};
pub use probe::{ProbeResult, probe, probe_all};
pub use target::Target;
pub use upstream::{HttpUpstream, RetryPolicy};
