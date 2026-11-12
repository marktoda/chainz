//! `chainz serve [status|stop|log]`: the local RPC gateway and its clients.
//!
//! The gateway itself lives in `chainz-serve`. Everything else here talks
//! to a running gateway through the runtime registry (`serve.lock`,
//! `serve.json`) and the control socket (`serve.sock`).

use super::runtime;
use crate::cli::{ServeArgs, ServeCommand};
use crate::{net, sys, ui, view};
use anyhow::{Context, Result, bail};
use chainz_core::paths;
use chainz_core::runtime::{LOOPBACK, Runtime, ServeInfo};
use chainz_rpc::control::{
    ControlClient, EVENTS_BUFFER, EventsOptions, MetricsRow, RecordStatus, RequestRecord, Status,
};
use serde::Serialize;
use std::io::Write;
use std::time::{Duration, Instant};

/// How long `--detach` waits for the background gateway to come up.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `serve stop` waits: the gateway drains for up to 5 s.
const STOP_TIMEOUT: Duration = Duration::from_secs(8);
/// Records `serve log -f` shows before following.
const FOLLOW_BACKLOG: usize = 10;

/// Route `serve` and its subcommands.
pub fn run(cmd: Option<ServeCommand>, args: ServeArgs) -> Result<i32> {
    match cmd {
        None => start(args),
        Some(ServeCommand::Status { json }) => status(json),
        Some(ServeCommand::Stop) => stop(),
        Some(ServeCommand::Log {
            follow,
            chain,
            method,
        }) => log(follow, chain, method),
    }
}

/// The running gateway's record, or `None` when no gateway holds the lock.
fn running(runtime: &Runtime) -> Result<Option<ServeInfo>> {
    if !runtime.serve_running() {
        return Ok(None);
    }
    // The lock is taken a moment before `serve.json` is written.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(info) = runtime.read_serve_info()? {
            return Ok(Some(info));
        }
        if !runtime.serve_running() {
            return Ok(None);
        }
        if Instant::now() > deadline {
            bail!("a gateway holds serve.lock but has not published serve.json");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn start(args: ServeArgs) -> Result<i32> {
    // Loads (and on first run migrates) the config, so errors surface here
    // rather than in a background log.
    let chainz = super::open()?;
    if chainz.config().chains.is_empty() {
        bail!("no chains configured; add one with `chainz chain add`");
    }
    drop(chainz);
    if let Some(info) = running(&runtime()?)? {
        bail!(
            "chainz serve is already running on port {} (pid {}); stop it with `chainz serve stop`",
            info.port,
            info.pid
        );
    }
    if args.detach {
        return detach(&args);
    }
    chainz_serve::run(chainz_serve::Options {
        port: args.port,
        log: args.log,
    })?;
    Ok(0)
}

/// Start the gateway as a background process, logging to the state dir,
/// and wait until it is serving.
fn detach(args: &ServeArgs) -> Result<i32> {
    use std::process::{Command, Stdio};
    let runtime = runtime()?;
    let state = paths::state_dir().context("unable to find the state directory")?;
    chainz_sys::create_private_dir(&state)?;
    let log_path = state.join("serve.log");
    let log = sys::open_log(&log_path, sys::LogMode::Append)?;

    let exe = std::env::current_exe().context("can't find the chainz executable")?;
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .arg("--port")
        .arg(args.port.to_string());
    if let Some(file) = &args.log {
        // The child keeps our working directory, so a relative path holds.
        command.arg("--log").arg(file);
    }
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Its own session: the terminal's Ctrl-C and hangup don't reach it.
    sys::detach(&mut command);
    let mut child = command.spawn().context("failed to start the gateway")?;

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(exit) = child.try_wait()? {
            bail!(
                "the gateway exited during startup ({exit}); see {}",
                log_path.display()
            );
        }
        if runtime.serve_running()
            && let Some(info) = runtime.read_serve_info()?
            && info.pid == child.id()
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
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!(
                "the gateway did not come up within {} s; see {}",
                START_TIMEOUT.as_secs(),
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stop() -> Result<i32> {
    let runtime = runtime()?;
    let Some(info) = running(&runtime)? else {
        println!("chainz serve: not running");
        return Ok(0);
    };
    terminate(info.pid)?;
    let deadline = Instant::now() + STOP_TIMEOUT;
    while runtime.serve_running() {
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
}

fn status(json: bool) -> Result<i32> {
    let runtime = runtime()?;
    let Some(info) = running(&runtime)? else {
        if json {
            let stopped = StatusJson {
                running: false,
                status: None,
                last_60s: None,
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
    let minute = metrics
        .windows
        .iter()
        .find(|w| w.window_secs == 60)
        .map(|w| w.rows.as_slice())
        .unwrap_or_default();
    if json {
        let running = StatusJson {
            running: true,
            status: Some(&status),
            last_60s: Some(minute),
        };
        println!("{}", serde_json::to_string_pretty(&running)?);
    } else {
        print!("{}", render_status(&status, minute));
    }
    Ok(0)
}

/// Totals over a set of metric rows.
#[derive(Default)]
struct Totals {
    requests: u64,
    errors: u64,
    rate_limited: u64,
    failovers: u64,
    weighted_p50: u64,
}

impl Totals {
    fn add(&mut self, row: &MetricsRow) {
        self.requests += row.requests;
        self.errors += row.errors.values().sum::<u64>();
        self.rate_limited += row.rate_limited;
        self.failovers += row.failovers;
        self.weighted_p50 += u64::from(row.p50_ms) * row.requests;
    }

    /// The request-weighted mean of the per-method medians.
    fn p50(&self) -> String {
        match self.requests {
            0 => "-".to_string(),
            n => format!("{}ms", self.weighted_p50 / n),
        }
    }
}

fn render_status(status: &Status, minute: &[MetricsRow]) -> String {
    use std::fmt::Write;
    let mut all = Totals::default();
    minute.iter().for_each(|row| all.add(row));
    let mut out = String::new();
    let _ = writeln!(
        out,
        "chainz serve: running on {LOOPBACK}:{} (pid {}, v{}, up {})",
        status.port,
        status.pid,
        status.version,
        chainz_core::fmt::uptime(status.uptime_secs)
    );
    let _ = writeln!(
        out,
        "last 60s: {} upstream requests, {} errors, {} failovers, {} rate-limited",
        all.requests, all.errors, all.failovers, all.rate_limited
    );
    let header = [
        "CHAIN", "ID", "ACTIVE", "FAILOVER", "HEAD", "REQ", "ERR", "429", "P50",
    ];
    let mut rows: Vec<[String; 9]> = Vec::with_capacity(status.chains.len());
    for chain in &status.chains {
        let mut totals = Totals::default();
        minute
            .iter()
            .filter(|row| row.chain == chain.chain)
            .for_each(|row| totals.add(row));
        let dash = |s: String| if s.is_empty() { "-".to_string() } else { s };
        rows.push([
            chain.chain.clone(),
            chain.chain_id.to_string(),
            chain.active.clone(),
            dash(chain.failover.join(",")),
            dash(chain.head.map(|h| h.to_string()).unwrap_or_default()),
            totals.requests.to_string(),
            totals.errors.to_string(),
            totals.rate_limited.to_string(),
            totals.p50(),
        ]);
    }
    out.push('\n');
    out.push_str(&view::table(&header, &rows));
    out
}

fn log(follow: bool, chain: Option<String>, method: Option<String>) -> Result<i32> {
    let runtime = runtime()?;
    if running(&runtime)?.is_none() {
        bail!("chainz serve is not running");
    }
    // Records carry the chain's short name: accept an alias or ID too.
    let chain = chain.map(|query| {
        super::open()
            .ok()
            .and_then(|chainz| super::chain_arg(&chainz, &query).ok())
            .unwrap_or(query)
    });
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
            .events_with(EventsOptions {
                backlog: EVENTS_BUFFER,
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
        // Reconnect with a backlog and skip what was already shown, so no
        // record falls between the two requests.
        let mut events = client
            .events_with(EventsOptions {
                backlog: EVENTS_BUFFER,
                follow: true,
            })
            .await?;
        while let Some(record) = events.recv().await {
            if record.id <= last_id || !matches(&record) {
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
    let outcome = match &record.status {
        RecordStatus::Ok => "ok".to_string(),
        RecordStatus::Error {
            class,
            code: Some(code),
        } => format!("{class}({code})"),
        RecordStatus::Error { class, code: None } => class.to_string(),
    };
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
        "{time} {} {}{params} → {}{attempts} {}ms {outcome} {}B",
        record.chain, record.method, record.upstream, record.latency_ms, record.response_bytes
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chainz_rpc::ErrorClass;
    use chainz_rpc::control::ChainStatus;
    use std::collections::BTreeMap;

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
        let row = |method: &str, requests: u64, p50_ms: u32| MetricsRow {
            chain: "eth".into(),
            endpoint: "infura".into(),
            method: method.into(),
            requests,
            errors: BTreeMap::from([(ErrorClass::Timeout, 1)]),
            p50_ms,
            p95_ms: 0,
            rate_limited: 2,
            failovers: 1,
            bytes: 0,
        };
        let text = render_status(
            &status,
            &[row("eth_call", 3, 40), row("eth_getLogs", 1, 80)],
        );
        let expected = "\
chainz serve: running on 127.0.0.1:18545 (pid 42, v2.0.0, up 2m05s)
last 60s: 4 upstream requests, 2 errors, 2 failovers, 4 rate-limited

CHAIN  ID    ACTIVE  FAILOVER          HEAD      REQ  ERR  429  P50
eth    1     infura  llama,publicnode  23481220  4    2    4    50ms
base   8453  infura  -                 -         0    0    0    -
";
        assert_eq!(text, expected);
    }
}
