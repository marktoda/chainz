//! `chainz serve`: the local JSON-RPC gateway.
//!
//! One authenticated loopback URL per chain
//! (`http://127.0.0.1:<port>/<token>/<chain>[/<endpoint>]`), failover only
//! across endpoints the config approves, rate-limit smoothing, metrics, and
//! a request inspector. The control API (status, metrics, events, reload)
//! is served on `serve.sock`, never on the TCP port; its types live in
//! [`chainz_rpc::control`].
//!
//! [`run`] is the process entry point (its caller presents [`Event`]s);
//! [`Gateway`] is the embeddable server
//! (tests start it in-process against mock upstreams). [`PinnedProxy`] is
//! the single-endpoint variant (for forks); both use the same loopback
//! token and `Host` checks.
//!
//! Internal to [chainz](https://crates.io/crates/chainz), published only so
//! the binary can be installed: there is no stable API, and any release may
//! break it.

#![warn(missing_docs)]

mod auth;
mod classify;
mod control;
mod events;
mod gateway;
mod http;
mod inspector;
mod limiter;
mod metrics;
mod pinned;
mod router;
mod state;
mod summary;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use chainz_core::StorePaths;
use chainz_core::runtime::Runtime;
use tokio::signal::unix::{SignalKind, signal};

pub use gateway::{AlreadyRunning, Gateway, GatewayConfig, SHUTDOWN_BUDGET};
pub use pinned::PinnedProxy;

/// Options for [`run`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Loopback port.
    pub port: u16,
    /// Append request records to this file as JSONL.
    pub log: Option<PathBuf>,
    /// Send [`Event::Tick`] every second (for a live summary line).
    pub ticks: bool,
}

/// What [`run`] reports to its caller, which presents it.
#[derive(Debug)]
pub enum Event<'a> {
    /// The gateway serves and `serve.json` is published.
    Started(&'a Gateway),
    /// A notice: a reload, a warning.
    Notice(&'a str),
    /// A second passed (with [`Options::ticks`]).
    Tick(&'a Gateway),
    /// A stop signal arrived: in-flight requests get up to `drain`.
    Stopping {
        /// How long in-flight requests may take.
        drain: Duration,
    },
}

/// Run the gateway until SIGINT or SIGTERM; SIGHUP reloads the config.
/// `paths`, `runtime` and `state_dir` are resolved by the caller (as for
/// the dashboard). Everything worth showing goes to `on_event`; an error it
/// returns stops the gateway and is returned.
pub fn run(
    paths: StorePaths,
    runtime: Runtime,
    state_dir: Option<PathBuf>,
    options: Options,
    on_event: impl FnMut(Event<'_>) -> Result<()>,
) -> Result<()> {
    let mut config = GatewayConfig::new(paths, runtime, state_dir);
    config.port = options.port;
    config.log = options.log;

    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .context("failed to start the async runtime")?
        .block_on(serve(config, options.ticks, on_event))
}

async fn serve(
    config: GatewayConfig,
    ticks: bool,
    mut on_event: impl FnMut(Event<'_>) -> Result<()>,
) -> Result<()> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let drain = config.drain;
    let gateway = Gateway::start(config).await?;
    let mut notices = gateway.notices();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut outcome = on_event(Event::Started(&gateway));
    while outcome.is_ok() {
        tokio::select! {
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            _ = hangup.recv() => {
                // The outcome arrives as a notice.
                let _ = gateway.reload().await;
            }
            notice = notices.recv() => {
                if let Ok(notice) = notice {
                    outcome = on_event(Event::Notice(&notice));
                }
            }
            _ = ticker.tick(), if ticks => outcome = on_event(Event::Tick(&gateway)),
        }
    }
    if outcome.is_ok() {
        outcome = on_event(Event::Stopping { drain });
    }
    gateway.shutdown().await;
    outcome
}
