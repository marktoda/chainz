//! The synchronous core of chainz.
//!
//! No async runtime, no sockets, no threads. `exec` and `env` call only
//! this crate (and `ls` reads the recorded endpoint health besides), which
//! keeps the hot path offline at compile time; the `strace` test checks it
//! at run time.
//!
//! Internal to [chainz](https://crates.io/crates/chainz), published only so
//! the binary can be installed: there is no stable API, and any release may
//! break it.

#![warn(missing_docs)]

pub mod addrbook;
pub mod address;
pub mod broadcast;
pub mod context;
pub mod contract;
mod envelope;
pub mod extract;
pub mod fmt;
pub mod forks;
mod grammar;
pub mod labels;
pub mod migrate;
pub mod model;
pub mod names;
pub mod paths;
pub mod redact;
pub mod resolve;
pub mod route;
pub mod runtime;
pub mod secrets;
pub mod signer;
pub mod store;
pub mod template;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod tx;
mod urls;
pub mod validate;

pub use address::Address;
pub use store::{Chainz, LoadMode, StorePaths, StoreStamp};
pub use tx::Tx;
