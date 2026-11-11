//! `chainz serve`: the local JSON-RPC gateway.
//!
//! One authenticated loopback URL per chain
//! (`http://127.0.0.1:<port>/<token>/<chain>[/<endpoint>]`), failover only
//! across endpoints the config approves, rate-limit smoothing, metrics, and
//! a request inspector. The control API (status, metrics, events, reload)
//! is served on `serve.sock`, never on the TCP port; its types live in
//! [`chainz_rpc::control`].
//!
//! [`run`] is the process entry point; [`Gateway`] is the embeddable server
//! (tests start it in-process against mock upstreams). [`PinnedProxy`] is
//! the single-endpoint variant (for forks), and [`auth`] holds the loopback
//! token and `Host` checks both use.

#![warn(missing_docs)]

pub mod auth;
mod classify;
mod gateway;
mod http;
mod inspector;
mod metrics;
pub mod pinned;
mod router;
mod state;
mod summary;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chainz_core::runtime::Runtime;
use chainz_core::{StorePaths, paths};
use tokio::signal::unix::{SignalKind, signal};

pub use gateway::{DEFAULT_PORT, Gateway, GatewayConfig};
pub use metrics::Summary;
pub use pinned::PinnedProxy;

/// Unix time in milliseconds.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Options for [`run`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Loopback port.
    pub port: u16,
    /// Append request records to this file as JSONL.
    pub log: Option<PathBuf>,
}

/// Run the gateway in the foreground until SIGINT or SIGTERM. SIGHUP
/// reloads the config. On a terminal, a one-line live summary is kept on
/// stderr.
pub fn run(options: Options) -> Result<()> {
    let paths = StorePaths::resolve()?;
    let runtime = Runtime::resolve().context("unable to find the runtime directory")?;
    let state_dir = paths::state_dir().ok();
    let mut config = GatewayConfig::new(paths, runtime, state_dir);
    config.port = options.port;
    config.log = options.log;

    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .context("failed to start the async runtime")?
        .block_on(foreground(config))
}

async fn foreground(config: GatewayConfig) -> Result<()> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let log = config.log.clone();
    let gateway = Gateway::start(config).await?;
    let mut notices = gateway.notices();
    let live = std::io::stderr().is_terminal();
    let say = |text: &str| {
        // Clear the summary line first when there is one.
        let clear = if live { "\r\x1b[K" } else { "" };
        eprintln!("{clear}chainz serve: {text}");
    };

    let status = gateway.status();
    say(&format!(
        "listening on http://127.0.0.1:{} ({} chains, pid {})",
        gateway.port(),
        status.chains.len(),
        status.pid
    ));
    for warning in gateway.warnings() {
        say(&format!("warning: {warning}"));
    }
    if let Some(log) = &log {
        say(&format!("logging requests to {}", log.display()));
    }
    say(
        "`chainz exec` now routes through the gateway (--direct bypasses it); stop with Ctrl-C or `chainz serve stop`",
    );

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            _ = hangup.recv() => {
                // The outcome arrives as a notice.
                let _ = gateway.reload().await;
            }
            notice = notices.recv() => {
                if let Ok(notice) = notice {
                    say(&notice);
                }
            }
            _ = ticker.tick(), if live => {
                let s = gateway.summary();
                let status = gateway.status();
                eprint!(
                    "\r\x1b[K:{} │ {} chains │ up {} │ last 60s: {} req, {} err, {} failover, {} rate-limited, p50 {} ms",
                    status.port,
                    status.chains.len(),
                    chainz_core::fmt::uptime(status.uptime_secs),
                    s.requests,
                    s.errors,
                    s.failovers,
                    s.rate_limited,
                    s.p50_ms,
                );
                let _ = std::io::stderr().flush();
            }
        }
    }
    say("stopping (in-flight requests get up to 5 s)");
    gateway.shutdown().await;
    Ok(())
}
