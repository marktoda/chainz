//! `chainz top`: the live terminal dashboard.
//!
//! Phase 0 stub. The Dashboard stream implements this crate on top of
//! `chainz-rpc` (probes, health history) and the control API client in
//! [`chainz_rpc::control`].

/// Error text returned until the dashboard exists.
pub const NOT_IMPLEMENTED: &str = "`chainz top` is not yet implemented";

/// Entry point for `chainz top`; the Dashboard stream replaces this.
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!(NOT_IMPLEMENTED)
}
