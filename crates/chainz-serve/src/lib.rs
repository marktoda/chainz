//! `chainz serve`: the local JSON-RPC gateway.
//!
//! Phase 0 stub. The Gateway stream implements this crate on top of
//! [`chainz_rpc::upstream::Upstream`] and owns the provisional control API
//! types in [`chainz_rpc::control`].

/// Error text every stream-owned command returns until it is implemented.
pub const NOT_IMPLEMENTED: &str = "`chainz serve` is not yet implemented";

/// Entry point for `chainz serve`; the Gateway stream replaces this.
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!(NOT_IMPLEMENTED)
}
