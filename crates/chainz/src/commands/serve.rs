//! `chainz serve [status|stop|log]`: the local RPC gateway and its clients.
//!
//! The gateway itself lives in `chainz-serve`. Everything else here talks
//! to a running gateway through the runtime registry (`serve.lock`,
//! `serve.json`) and the control socket (`serve.sock`).

use crate::cli::{ServeArgs, ServeCommand, ServeLogArgs};
use crate::load::open;
use crate::{net, sys, ui, view};
use anyhow::{Context, Result, bail};
use chainz_core::StorePaths;
use chainz_core::paths;
use chainz_core::resolve::{self, Policy};
use chainz_core::runtime::{GatewayState, LOOPBACK, Runtime, ServeInfo};
use chainz_os::LogMode;
use chainz_rpc::control::{
    Backlog, ControlClient, EventsOptions, MetricsRow, MetricsTotals, MetricsWindow, RequestRecord,
    Status, Window,
};
use chainz_serve::Event;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long `--detach` waits for the background gateway to come up.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `serve stop` waits: the gateway's whole shutdown, plus time to
/// notice the signal and exit.
const STOP_TIMEOUT: Duration =
    chainz_serve::SHUTDOWN_BUDGET.saturating_add(Duration::from_millis(750));
/// Records `serve log -f` shows before following.
const FOLLOW_BACKLOG: usize = 10;

/// Route `serve` and its subcommands.
pub fn run(cmd: Option<ServeCommand>, args: ServeArgs) -> Result<i32> {
    match cmd {
        None => start(args),
        Some(ServeCommand::Status(args)) => status(args.json),
        Some(ServeCommand::Stop) => stop(),
        Some(ServeCommand::Log(args)) => log(&args),
    }
}

/// The gateway's state, read once.
fn gateway(runtime: &Runtime) -> Result<GatewayState> {
    runtime
        .gateway()
        .context("unable to read the running gateway's record")
}

/// The running gateway's record, or `None` when no gateway holds the lock.
fn running(runtime: &Runtime) -> Result<Option<ServeInfo>> {
    // The lock is taken a moment before `serve.json` is written.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match gateway(runtime)? {
            GatewayState::Down => return Ok(None),
            GatewayState::Up(info) => return Ok(Some(info)),
            GatewayState::Starting if Instant::now() > deadline => {
                bail!("a gateway holds serve.lock but has not published serve.json")
            }
            GatewayState::Starting => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn start(args: ServeArgs) -> Result<i32> {
    preflight()?;
    if args.detach {
        return detach();
    }
    let mut foreground = Foreground {
        live: std::io::stderr().is_terminal(),
        log: args.log.clone(),
    };
    let options = chainz_serve::Options {
        port: args.port,
        log: args.log,
        ticks: foreground.live,
    };
    chainz_serve::run(
        StorePaths::resolve()?,
        Runtime::resolve()?,
        paths::state_dir().ok(),
        options,
        |event| foreground.show(event),
    )?;
    Ok(0)
}

/// What must hold before a gateway starts, checked here so the errors reach
/// the terminal rather than a background log: the config loads (migrating
/// v1 on first run) and has chains, and no gateway runs.
fn preflight() -> Result<()> {
    let chainz = open()?;
    if chainz.config().chains.is_empty() {
        bail!("{}", chainz_core::resolve::NO_CHAINS);
    }
    if let Some(info) = running(&Runtime::resolve()?)? {
        bail!(chainz_serve::AlreadyRunning(Some(info)));
    }
    Ok(())
}

/// The foreground gateway's terminal (or, detached, its log): notices on
/// stderr, and on a terminal a live one-line summary.
struct Foreground {
    live: bool,
    log: Option<PathBuf>,
}

impl Foreground {
    fn say(&self, text: &str) {
        // Clear the summary line first when there is one.
        let clear = if self.live { "\r\x1b[K" } else { "" };
        eprintln!("{clear}chainz serve: {text}");
    }

    fn show(&mut self, event: Event<'_>) -> Result<()> {
        match event {
            Event::Started(gateway) => {
                let status = gateway.status();
                self.say(&format!(
                    "listening on http://{LOOPBACK}:{} ({} chains, pid {})",
                    gateway.port(),
                    status.chains.len(),
                    status.pid
                ));
                for warning in gateway.warnings() {
                    self.say(&format!("warning: {warning}"));
                }
                if let Some(log) = &self.log {
                    self.say(&format!("logging requests to {}", log.display()));
                }
                self.say(
                    "`chainz exec` now routes through the gateway (--direct bypasses it); stop with Ctrl-C or `chainz serve stop`",
                );
                // `serve --detach` waits for this: serve.json is out.
                sys::detached_ready().context("unable to redirect output to the log")?;
            }
            Event::Notice(notice) => self.say(notice),
            Event::Tick(gateway) => {
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
            Event::Stopping { drain } => self.say(&format!(
                "stopping (in-flight requests get up to {} s)",
                drain.as_secs()
            )),
        }
        Ok(())
    }
}

fn detach() -> Result<i32> {
    let runtime = Runtime::resolve()?;
    let log_path = paths::serve_log().context("unable to find the state directory")?;
    // The same command line in the background, ready once serve.json is
    // out; the child keeps our working directory, so a relative --log holds.
    let mut child =
        sys::launch_detached(&log_path, LogMode::Append).context("failed to start the gateway")?;
    match child.wait_ready(Some(START_TIMEOUT)) {
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            let _ = child.kill();
            bail!(
                "the gateway did not come up within {} s; see {}",
                START_TIMEOUT.as_secs(),
                log_path.display()
            );
        }
        ready => {
            ready.context("lost the background gateway")?;
        }
    }
    if let GatewayState::Up(info) = gateway(&runtime)?
        && info.pid == child.pid()
    {
        println!(
            "{}",
            ui::success(&format!(
                "chainz serve: running on {LOOPBACK}:{} (pid {})",
                info.port, info.pid
            ))
        );
        println!("{}", ui::dim(&format!("log: {}", log_path.display())));
        return Ok(0);
    }
    let exit = child.wait()?;
    bail!(
        "the gateway exited during startup ({exit}); see {}",
        log_path.display()
    )
}

fn stop() -> Result<i32> {
    let runtime = Runtime::resolve()?;
    let Some(info) = running(&runtime)? else {
        println!("chainz serve: not running");
        return Ok(0);
    };
    terminate(info.pid)?;
    let deadline = Instant::now() + STOP_TIMEOUT;
    while gateway(&runtime)? != GatewayState::Down {
        if Instant::now() > deadline {
            bail!(
                "the gateway (pid {}) did not stop within {} s",
                info.pid,
                STOP_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("{}", ui::success("chainz serve: stopped"));
    Ok(0)
}

/// SIGTERM: the gateway drains in-flight requests, then exits. The PID
/// comes from `serve.json` while `serve.lock` is held, so it is the
/// gateway's.
fn terminate(pid: u32) -> Result<()> {
    sys::signal_process(pid, libc::SIGTERM).context("failed to signal the gateway")
}

/// `serve status --json`.
#[derive(Serialize)]
struct StatusJson<'a> {
    running: bool,
    #[serde(flatten)]
    status: Option<&'a Status>,
    /// Upstream metrics over the last 60 s.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_60s: Option<&'a [MetricsRow]>,
    /// Their totals, overall and per chain (absent from a 2.0 gateway).
    #[serde(skip_serializing_if = "Option::is_none")]
    totals_60s: Option<&'a MetricsTotals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chain_totals_60s: Option<&'a BTreeMap<String, MetricsTotals>>,
}

fn status(json: bool) -> Result<i32> {
    let runtime = Runtime::resolve()?;
    let Some(info) = running(&runtime)? else {
        if json {
            let stopped = StatusJson {
                running: false,
                status: None,
                last_60s: None,
                totals_60s: None,
                chain_totals_60s: None,
            };
            println!("{}", serde_json::to_string_pretty(&stopped)?);
        } else {
            println!("chainz serve: not running");
        }
        return Ok(1);
    };
    let client = ControlClient::new(runtime.serve_sock_path());
    let (status, metrics) = net::block_on(async {
        let status = client.status().await?;
        let metrics = client.metrics().await?;
        Ok::<_, std::io::Error>((status, metrics))
    })
    .with_context(|| {
        format!(
            "the gateway (pid {}) is not answering on its control socket",
            info.pid
        )
    })?;
    let minute = metrics.window(Window::Short);
    if json {
        let running = StatusJson {
            running: true,
            status: Some(&status),
            last_60s: Some(minute.map(|w| w.rows.as_slice()).unwrap_or_default()),
            totals_60s: minute.and_then(|w| w.totals.as_ref()),
            chain_totals_60s: minute.filter(|w| w.totals.is_some()).map(|w| &w.chains),
        };
        println!("{}", serde_json::to_string_pretty(&running)?);
    } else {
        print!("{}", render_status(&status, minute));
    }
    Ok(0)
}

/// One row's numbers: `-` where an older gateway sent no totals, and for
/// the median of no requests.
fn totals_cells(totals: Option<MetricsTotals>) -> [String; 4] {
    let Some(t) = totals else {
        return ["-", "-", "-", "-"].map(String::from);
    };
    let p50 = match t.requests {
        0 => "-".to_string(),
        _ => format!("{}ms", t.p50_ms),
    };
    [
        t.requests.to_string(),
        t.errors.to_string(),
        t.rate_limited.to_string(),
        p50,
    ]
}

fn render_status(status: &Status, minute: Option<&MetricsWindow>) -> String {
    use std::fmt::Write;
    let totals = minute.and_then(|w| w.totals);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "chainz serve: running on {LOOPBACK}:{} (pid {}, v{}, up {})",
        status.port,
        status.pid,
        status.version,
        chainz_core::fmt::uptime(status.uptime_secs)
    );
    let count =
        |n: fn(&MetricsTotals) -> u64| totals.map_or("-".to_string(), |t| n(&t).to_string());
    let _ = writeln!(
        out,
        "last 60s: {} upstream requests, {} errors, {} failovers, {} rate-limited",
        count(|t| t.requests),
        count(|t| t.errors),
        count(|t| t.failovers),
        count(|t| t.rate_limited)
    );
    let header = [
        "CHAIN", "ID", "ACTIVE", "FAILOVER", "HEAD", "REQ", "ERR", "429", "P50",
    ];
    let mut rows: Vec<[String; 9]> = Vec::with_capacity(status.chains.len());
    for chain in &status.chains {
        let chain_totals = minute
            .filter(|_| totals.is_some())
            .map(|w| w.chains.get(&chain.chain).copied().unwrap_or_default());
        let [requests, errors, rate_limited, p50] = totals_cells(chain_totals);
        let dash = |s: String| if s.is_empty() { "-".to_string() } else { s };
        rows.push([
            chain.chain.clone(),
            chain.chain_id.to_string(),
            chain.active.clone(),
            dash(chain.failover.join(",")),
            dash(chain.head.map(|h| h.to_string()).unwrap_or_default()),
            requests,
            errors,
            rate_limited,
            p50,
        ]);
    }
    out.push('\n');
    out.push_str(&view::table(&header, &rows));
    out
}

fn log(args: &ServeLogArgs) -> Result<i32> {
    let (follow, method) = (args.follow, &args.method);
    let runtime = Runtime::resolve()?;
    if running(&runtime)?.is_none() {
        bail!("chainz serve is not running");
    }
    // Records carry the chain's short name: accept an alias or ID too (a
    // chain no longer configured still filters by the name given).
    let chain = match &args.chain {
        Some(query) => {
            let chainz = open()?;
            Some(
                resolve::configured(chainz.config(), query, Policy::Prefix)
                    .map_or_else(|_| query.clone(), |(name, _)| name.to_string()),
            )
        }
        None => None,
    };
    let matches = |record: &RequestRecord| {
        chain
            .as_ref()
            .is_none_or(|c| record.chain.eq_ignore_ascii_case(c))
            && method
                .as_ref()
                .is_none_or(|m| record.method.eq_ignore_ascii_case(m))
    };
    let client = ControlClient::new(runtime.serve_sock_path());
    net::block_on(async {
        // Filtering happens here, so ask for the whole buffer.
        let mut events = client
            .events(EventsOptions {
                backlog: Backlog::All,
                after_id: None,
                follow: false,
            })
            .await?;
        let mut backlog = Vec::new();
        let mut last_id = 0;
        while let Some(record) = events.recv().await {
            last_id = record.id;
            if matches(&record) {
                backlog.push(record);
            }
        }
        let skip = if follow {
            backlog.len().saturating_sub(FOLLOW_BACKLOG)
        } else {
            0
        };
        let mut out = std::io::stdout().lock();
        for record in &backlog[skip..] {
            if writeln!(out, "{}", render_record(record)).is_err() {
                return Ok(()); // closed pipe
            }
        }
        if !follow {
            return Ok(());
        }
        let _ = out.flush();
        // Pick up after the last record seen, so none falls between the
        // two requests.
        let mut events = client
            .events(EventsOptions {
                backlog: Backlog::All,
                after_id: Some(last_id),
                follow: true,
            })
            .await?;
        while let Some(record) = events.recv().await {
            if !matches(&record) {
                continue;
            }
            if writeln!(out, "{}", render_record(&record)).is_err() || out.flush().is_err() {
                return Ok(());
            }
        }
        Ok::<_, std::io::Error>(())
    })
    .context("lost the gateway's control socket")?;
    Ok(0)
}

/// `19:02:11.123Z base eth_call @usdc.balanceOf latest → infura 12ms ok 66B`.
fn render_record(record: &RequestRecord) -> String {
    let secs = record.at_ms / 1000 % 86_400;
    let time = format!(
        "{:02}:{:02}:{:02}.{:03}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60,
        record.at_ms % 1000
    );
    let attempts = match record.attempts {
        0 | 1 => String::new(),
        n => format!(" ({n} attempts)"),
    };
    let params = if record.params_summary.is_empty() {
        String::new()
    } else {
        format!(" {}", record.params_summary)
    };
    format!(
        "{time} {} {}{params} → {}{attempts} {}ms {} {}B",
        record.chain,
        record.method,
        record.upstream,
        record.latency_ms,
        record.status,
        record.response_bytes
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chainz_rpc::ErrorClass;
    use chainz_rpc::control::{ChainStatus, RecordStatus};

    fn record() -> RequestRecord {
        RequestRecord {
            id: 7,
            at_ms: 1_794_423_731_123, // 19:02:11.123 UTC
            chain: "base".into(),
            method: "eth_call".into(),
            params_summary: "@usdc.balanceOf latest".into(),
            upstream: "infura".into(),
            attempts: 1,
            latency_ms: 12,
            status: RecordStatus::Ok,
            response_bytes: 66,
        }
    }

    #[test]
    fn record_lines() {
        assert_eq!(
            render_record(&record()),
            "19:02:11.123Z base eth_call @usdc.balanceOf latest → infura 12ms ok 66B"
        );
        let failed = RequestRecord {
            params_summary: String::new(),
            upstream: "llama".into(),
            attempts: 3,
            status: RecordStatus::Error {
                class: ErrorClass::Rpc,
                code: Some(3),
            },
            ..record()
        };
        assert_eq!(
            render_record(&failed),
            "19:02:11.123Z base eth_call → llama (3 attempts) 12ms rpc(3) 66B"
        );
    }

    #[test]
    fn status_table() {
        console::set_colors_enabled(false);
        let status = Status {
            version: "2.0.0".into(),
            pid: 42,
            port: 18545,
            uptime_secs: 125,
            chains: vec![
                ChainStatus {
                    chain: "eth".into(),
                    chain_id: 1,
                    active: "infura".into(),
                    failover: vec!["llama".into(), "publicnode".into()],
                    head: Some(23_481_220),
                },
                ChainStatus {
                    chain: "base".into(),
                    chain_id: 8453,
                    active: "infura".into(),
                    failover: vec![],
                    head: None,
                },
            ],
        };
        let eth = MetricsTotals {
            requests: 4,
            errors: 2,
            rate_limited: 4,
            failovers: 2,
            p50_ms: 40,
            p95_ms: 80,
            bytes: 0,
        };
        let window = MetricsWindow {
            window_secs: 60,
            rows: vec![],
            totals: Some(eth),
            chains: BTreeMap::from([("eth".to_string(), eth)]),
        };
        let text = render_status(&status, Some(&window));
        let expected = "\
chainz serve: running on 127.0.0.1:18545 (pid 42, v2.0.0, up 2m05s)
last 60s: 4 upstream requests, 2 errors, 2 failovers, 4 rate-limited

CHAIN  ID    ACTIVE  FAILOVER          HEAD      REQ  ERR  429  P50
eth    1     infura  llama,publicnode  23481220  4    2    4    40ms
base   8453  infura  -                 -         0    0    0    -
";
        assert_eq!(text, expected);
        // A 2.0 gateway sends no totals.
        let old = MetricsWindow {
            totals: None,
            chains: BTreeMap::new(),
            ..window
        };
        let text = render_status(&status, Some(&old));
        assert!(
            text.contains("last 60s: - upstream requests, - errors"),
            "{text}"
        );
        assert!(
            text.contains("eth    1     infura  llama,publicnode  23481220  -    -    -    -"),
            "{text}"
        );
    }
}
