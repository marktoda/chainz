//! JSON-RPC client, endpoint probes, ranking, capability tags, health history,
//! the [`Upstream`] forwarding trait, the gateway control
//! API types and the chainlist registry client for chainz.
//!
//! This crate is async (tokio) and deliberately does not depend on
//! `chainz-core`: callers resolve config endpoints into [`Target`]s (with all
//! `${VAR}` references already expanded) and hand them in.
//!
//! # Secrets
//!
//! Endpoint URLs and header values may embed API keys. No error message and no
//! `Debug` output produced by this crate contains an endpoint URL or a header
//! value; header *names* may appear.

#![warn(missing_docs)]

pub mod capabilities;
pub mod chainlist;
pub mod client;
pub mod control;
pub mod erc20;
pub mod error;
pub mod hex;
pub mod history;
pub mod limiter;
pub mod probe;
pub mod rank;
pub mod target;
pub mod upstream;

pub use capabilities::{Capabilities, probe_capabilities};
pub use client::{BlockId, RpcClient};
pub use error::{ErrorClass, RpcError};
pub use history::{HealthSample, HealthStore};
pub use limiter::AimdLimiter;
pub use probe::{LAG_THRESHOLD, PROBE_DEADLINE, ProbeResult, probe, probe_all};
pub use rank::{Health, classify, rank};
pub use target::Target;
pub use upstream::{HttpUpstream, RetryOn, RetryPolicy, Upstream, UpstreamResponse};
