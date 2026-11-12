//! `chainz fork CHAIN`: start a fork and supervise it until it stops.

use super::fund::{self, FundPlan};
use super::{endpoint, process};
use crate::cli::ForkArgs;
use crate::commands::{chain_arg, open, runtime};
use crate::{net, sys, ui};
use anyhow::{Context as _, Result, anyhow, bail};
use chainz_core::Chainz;
use chainz_core::fmt::{group, short_address};
use chainz_core::forks;
use chainz_core::model::{Chain, Config};
use chainz_core::runtime::{ForkClaim, ForkRecord, LOOPBACK, Runtime, loopback_url};
use chainz_rpc::{HttpUpstream, RetryPolicy, RpcClient, Target};
use chainz_serve::PinnedProxy;
use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Fork ports are allocated from here upward.
const FIRST_PORT: u16 = 8601;
/// How long anvil may take to come up (it fetches the fork block first).
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// The first port from `start` upward that is not busy.
fn first_free_port(start: u16, mut busy: impl FnMut(u16) -> bool) -> Option<u16> {
    (start..=u16::MAX).find(|port| !busy(*port))
}

/// Whether something already listens on the loopback port.
fn port_busy(port: u16) -> bool {
    TcpListener::bind((LOOPBACK, port)).is_err()
}

/// Who and what to fund. `--no-fund` and a chain without a signer address
/// fund nothing; the latter is an error only when `--fund` was asked for.
fn plan_funding(
    config: &Config,
    chain_name: &str,
    chain: &Chain,
    args: &ForkArgs,
) -> Result<Option<FundPlan>> {
    if args.no_fund {
        return Ok(None);
    }
    let signer = chain.signer.as_ref().and_then(|name| {
        let address = config.signers.get(name)?.address?;
        Some((name.clone(), address))
    });
    let Some((signer, address)) = signer else {
        if args.fund.is_some() {
            bail!(
                "--fund: chain '{chain_name}' has no signer with a cached address to fund (pass --no-fund to fork without funding)"
            );
        }
        eprintln!(
            "{}",
            ui::warn(&format!(
                "chain '{chain_name}' has no signer with a cached address; nothing funded"
            ))
        );
        return Ok(None);
    };
    Ok(Some(FundPlan {
        signer,
        address,
        native_symbol: fund::native_symbol(chain.id),
        tokens: fund::plan_tokens(
            config,
            chain_name,
            chain.id,
            args.fund.as_deref().unwrap_or(""),
        )?,
    }))
}

fn summary_line(record: &ForkRecord, block: u64) -> String {
    format!(
        "{}  {}  block {}  chain {}",
        record.name,
        record.rpc_url(),
        group(block),
        record.chain_id
    )
}

pub(super) fn anvil_target(chain_id: u64, port: u16) -> Result<Target> {
    Target::new(chain_id, "anvil", &loopback_url(port), &[]).map_err(|e| anyhow!("{e}"))
}

pub fn start(args: ForkArgs) -> Result<i32> {
    let chainz = open()?;
    let runtime = runtime()?;
    let query = args
        .chain
        .as_deref()
        .ok_or_else(|| anyhow!("no chain given: chainz fork CHAIN"))?;
    let parent = chain_arg(&chainz, query)?;
    let config = chainz.config();
    let chain = &config.chains[&parent];
    forks::check_forkable(&parent, chain.id)?;

    process::reap(&runtime)?;
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| forks::default_fork_name(&parent));
    forks::check_fork_name(config, &name)?;
    // Said before anything starts; claiming the name below is the check
    // that holds when two forks race for it.
    if runtime.fork_live(&name).unwrap_or(false) {
        return Err(already_running(&name));
    }
    let funding = plan_funding(config, &parent, chain, &args)?;
    match args.port {
        Some(0) => bail!("--port 0 is not a usable port"),
        Some(port) if port_busy(port) => bail!("port {port} is already in use"),
        _ => {}
    }

    if args.detach {
        return detach(&runtime, &name);
    }
    let detached = chainz_sys::is_detached();

    process::install_signal_handlers();
    let claim = runtime
        .claim_fork(&name)
        .context("unable to claim the fork name")?
        .ok_or_else(|| already_running(&name))?;
    let mut supervisor = Supervisor { anvil: None, claim };
    let job = Job {
        chainz: &chainz,
        runtime: &runtime,
        parent: &parent,
        chain,
        name: &name,
        args: &args,
        funding: funding.as_ref(),
        detached,
    };
    let code = net::block_on(supervise(&mut supervisor, &job));
    drop(supervisor);
    code
}

fn already_running(name: &str) -> anyhow::Error {
    anyhow!(
        "fork '{name}' is already running; stop it with `chainz fork stop {name}` or pick another with --name"
    )
}

/// `--detach`: run this command again as a background supervisor and
/// return once it is ready: it prints the summary, then closes the pipe.
/// Its stderr, and later its stdout, is the log.
fn detach(runtime: &Runtime, name: &str) -> Result<i32> {
    let log_path = process::log_path(name)?;
    let mut child = chainz_sys::launch_detached(&log_path, sys::LogMode::Truncate)
        .context("unable to start the background supervisor")?;
    let summary = child
        .wait_ready(None)
        .context("lost the background supervisor")?;
    print!("{summary}");
    let registered = runtime
        .fork(name)
        .ok()
        .flatten()
        .is_some_and(|record| record.pid == child.pid());
    if registered {
        eprintln!(
            "{}",
            ui::dim(&format!(
                "detached (pid {}); log {}; stop with `chainz fork stop {name}`",
                child.pid(),
                log_path.display()
            ))
        );
        return Ok(0);
    }
    let status = child.wait()?;
    bail!(
        "the fork did not start ({status}):\n{}",
        process::log_tail(&log_path, 0, 15)
    )
}

/// Everything the supervisor needs to start one fork.
struct Job<'a> {
    chainz: &'a Chainz,
    runtime: &'a Runtime,
    parent: &'a str,
    chain: &'a Chain,
    name: &'a str,
    args: &'a ForkArgs,
    funding: Option<&'a FundPlan>,
    detached: bool,
}

/// Owns the claim on the name and the anvil child. Dropping it stops
/// anvil, then releases the claim (which removes the fork's files), on
/// every exit path.
struct Supervisor {
    anvil: Option<Child>,
    claim: ForkClaim,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if let Some(mut anvil) = self.anvil.take() {
            process::stop_child(&mut anvil);
        }
    }
}

/// A fork that is up and registered.
struct Running {
    /// Keeps the proxy serving for the fork's whole life.
    _proxy: PinnedProxy,
    log_path: PathBuf,
}

/// Start the fork, then wait. A stop signal exits with 128 + its number
/// (130 for Ctrl-C) whether it arrives while starting or once running.
async fn supervise(supervisor: &mut Supervisor, job: &Job<'_>) -> Result<i32> {
    let running = tokio::select! {
        result = start_fork(supervisor, job) => result?,
        code = process::shutdown() => return Ok(code),
    };
    loop {
        if let Some(code) = process::shutdown_requested() {
            eprintln!("{}", ui::dim(&format!("stopped {}", job.name)));
            return Ok(code);
        }
        if let Some(anvil) = supervisor.anvil.as_mut()
            && let Some(status) = anvil.try_wait()?
        {
            eprintln!(
                "{}",
                ui::fail(&format!(
                    "anvil exited ({status}); {} is gone. Log: {}",
                    job.name,
                    running.log_path.display()
                ))
            );
            return Ok(1);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn start_fork(supervisor: &mut Supervisor, job: &Job<'_>) -> Result<Running> {
    let Job {
        chainz,
        runtime,
        parent,
        chain,
        name,
        args,
        ..
    } = *job;
    let client = RpcClient::new();
    let (endpoint, target) = endpoint::choose(chainz, parent, chain, args.block, &client).await?;

    // One upstream for the fork's whole life, always behind our own
    // proxy: the real endpoint, or a running gateway pinned to it. Anvil's
    // argv (world-readable in /proc) then only ever holds the proxy's
    // per-fork token, never the gateway's token or an endpoint secret.
    // Retries stay on that endpoint: mixing nodes would corrupt the fork.
    let (target, route) = endpoint::route(runtime, parent, chain.id, &endpoint, target)?;
    let upstream = Arc::new(HttpUpstream::new(client.clone()));
    let proxy = PinnedProxy::start(upstream, target, RetryPolicy::same_endpoint_transient())
        .await
        .context("unable to start the fork proxy")?;

    let log_path = process::log_path(name)?;
    // Detached, `detach` already emptied the log before starting us.
    let mode = if job.detached {
        sys::LogMode::Append
    } else {
        sys::LogMode::Truncate
    };
    let log = sys::open_log(&log_path, mode)?;
    let chain_id = args.chain_id.unwrap_or(chain.id);
    let mut record = spawn_on_free_port(supervisor, job, &proxy.url(), &log, &log_path).await?;
    let port = record.port;

    // Deals walk upstream state through the proxy: allow them time.
    let anvil_client = RpcClient::with_timeout(Duration::from_secs(90));
    let anvil = anvil_target(chain_id, port)?;
    let block = anvil_client
        .block_number(&anvil)
        .await
        .map_err(|e| anyhow!("the fork does not answer eth_blockNumber: {e}"))?;
    let report = match job.funding {
        Some(plan) => Some((plan, fund::fund(&anvil_client, &anvil, plan).await)),
        None => None,
    };

    record.ready = true;
    supervisor
        .claim
        .register(&record)
        .context("unable to register the fork")?;

    println!("{}", summary_line(&record, block));
    if let Some((plan, report)) = &report {
        if !report.funded.is_empty() {
            println!(
                "funded {} ({}): {}",
                short_address(&plan.address),
                plan.signer,
                report.funded.join(", ")
            );
        }
        for (what, why) in &report.failed {
            println!("{}", ui::warn(&format!("could not fund {what}: {why}")));
        }
    }
    let notice = format!(
        "upstream {parent}/{endpoint} (pinned, via {route}); log {}",
        log_path.display()
    );
    if job.detached {
        // Closing the stdout pipe releases the waiting `--detach` parent.
        chainz_sys::detached_ready().context("unable to redirect output to the log")?;
        eprintln!("chainz fork: {name} up; {notice}");
    } else {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        eprintln!("{}", ui::dim(&format!("{notice}; ctrl-c to stop")));
    }
    Ok(Running {
        _proxy: proxy,
        log_path,
    })
}

/// Spawn anvil on `--port`, or on the first free port from
/// [`FIRST_PORT`] up, and wait until it serves. Another process can take an
/// auto-allocated port between the check and anvil's bind (parallel forks
/// do): then the next port is tried, up to five times. Registers each
/// anvil (not yet ready) as it is spawned, and returns the record.
async fn spawn_on_free_port(
    supervisor: &mut Supervisor,
    job: &Job<'_>,
    fork_url: &str,
    log: &File,
    log_path: &Path,
) -> Result<ForkRecord> {
    let Job {
        runtime,
        parent,
        chain,
        name,
        args,
        ..
    } = *job;
    let chain_id = args.chain_id.unwrap_or(chain.id);
    let mut from = FIRST_PORT;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let port = match args.port {
            Some(port) => port,
            None => {
                let taken: Vec<u16> = runtime
                    .forks()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| e.live && e.name != name)
                    .filter_map(|e| e.record.ok().flatten().map(|r| r.port))
                    .collect();
                first_free_port(from, |p| taken.contains(&p) || port_busy(p))
                    .ok_or_else(|| anyhow!("no free port from {FIRST_PORT} upward"))?
            }
        };
        let log_start = log.metadata().map(|m| m.len()).unwrap_or(0);
        let spec = process::AnvilSpec {
            fork_url,
            port,
            block: args.block,
            chain_id: args.chain_id,
        };
        let anvil = process::spawn_anvil(&spec, log)?;
        let record = ForkRecord {
            format: ForkRecord::FORMAT,
            name: name.to_string(),
            pid: std::process::id(),
            anvil_pid: anvil.id(),
            port,
            parent: parent.to_string(),
            chain_id,
            fork_block: args.block,
            started_at: chainz_sys::now_secs(),
            ready: false,
        };
        supervisor.anvil = Some(anvil);
        supervisor
            .claim
            .register(&record)
            .context("unable to register the fork")?;
        let anvil = supervisor.anvil.as_mut().expect("just spawned");
        match wait_ready(anvil, chain_id, port, log_path, log_start).await? {
            None => return Ok(record),
            Some(status) => {
                supervisor.anvil = None;
                // Lost a race for an auto-allocated port (the winner may
                // already have let go of it again): take the next.
                let lost_port =
                    port_busy(port) || log_contains(log_path, log_start, "Address already in use");
                if args.port.is_none() && attempt < 5 && lost_port {
                    from = port.saturating_add(1);
                    continue;
                }
                bail!(
                    "anvil exited before the fork was ready ({status}):\n{}",
                    process::log_tail(log_path, log_start, 12)
                );
            }
        }
    }
}

/// Wait until our anvil serves `eth_chainId` on `port`. `Ok(Some(status))`
/// means anvil exited first.
async fn wait_ready(
    anvil: &mut Child,
    chain_id: u64,
    port: u16,
    log_path: &Path,
    log_start: u64,
) -> Result<Option<ExitStatus>> {
    let client = RpcClient::with_timeout(Duration::from_secs(2));
    let target = anvil_target(chain_id, port)?;
    let listening = format!("Listening on {LOOPBACK}:{port}");
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut first_answer: Option<Instant> = None;
    loop {
        if let Some(status) = anvil.try_wait()? {
            return Ok(Some(status));
        }
        // An answer alone could come from another process that won the
        // port; anvil's own "Listening on" line says the listener is ours.
        // Should that line ever change, a few seconds of answers with our
        // anvil still alive is accepted instead.
        let announced = log_contains(log_path, log_start, &listening);
        if announced || first_answer.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
            match client.chain_id(&target).await {
                Ok(actual) if actual == chain_id => return Ok(None),
                Ok(actual) => {
                    bail!("the fork on port {port} reports chain ID {actual}, expected {chain_id}")
                }
                Err(_) => {}
            }
        } else if first_answer.is_none() && client.chain_id(&target).await.is_ok() {
            first_answer = Some(Instant::now());
        }
        if Instant::now() > deadline {
            bail!(
                "anvil did not come up within {}s:\n{}",
                READY_TIMEOUT.as_secs(),
                process::log_tail(log_path, log_start, 12)
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn log_contains(path: &Path, from: u64, needle: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(from)).is_ok()
        && file.read_to_end(&mut bytes).is_ok()
        && String::from_utf8_lossy(&bytes).contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::parse(
            r#"
version = 2
[signers.default]
backend = "keyring"
address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
[signers.cold]
backend = "keyring"
[chains.eth]
id = 1
aliases = ["Ethereum Mainnet", "mainnet"]
rpc = "a"
rpcs.a = "https://a"
[chains.base]
id = 8453
signer = "default"
rpc = "b"
failover = ["c", "missing"]
rpcs.a = "https://a"
rpcs.b = "https://b"
rpcs.c = "https://c"
[chains.zksync]
id = 324
signer = "cold"
rpc = "a"
rpcs.a = "https://a"
"#,
        )
        .unwrap()
    }

    fn fork_args(fund: Option<&str>, no_fund: bool) -> ForkArgs {
        ForkArgs {
            chain: Some("base".into()),
            block: None,
            port: None,
            name: None,
            chain_id: None,
            fund: fund.map(String::from),
            no_fund,
            detach: false,
        }
    }

    #[test]
    fn ports_are_allocated_upward_skipping_busy_ones() {
        assert_eq!(first_free_port(FIRST_PORT, |_| false), Some(8601));
        assert_eq!(
            first_free_port(FIRST_PORT, |p| [8601, 8602, 8604].contains(&p)),
            Some(8603)
        );
        assert_eq!(first_free_port(u16::MAX, |_| true), None);
        assert_eq!(first_free_port(u16::MAX, |_| false), Some(u16::MAX));
        // A port with a listener is busy; once released it is free again.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_busy(port));
        assert_eq!(
            first_free_port(port, port_busy).map(|p| p > port),
            Some(true)
        );
        drop(listener);
        assert!(!port_busy(port));
    }

    #[test]
    fn funding_defaults_to_the_chain_signer_native_only() {
        let c = config();
        let plan = plan_funding(&c, "base", &c.chains["base"], &fork_args(None, false))
            .unwrap()
            .unwrap();
        assert_eq!(plan.signer, "default");
        assert_eq!(plan.native_symbol, "ETH");
        assert!(plan.tokens.is_empty());
        let plan = plan_funding(
            &c,
            "base",
            &c.chains["base"],
            &fork_args(Some("usdc=10000,weth=5"), false),
        )
        .unwrap()
        .unwrap();
        let symbols: Vec<&str> = plan.tokens.iter().map(|t| t.symbol.as_str()).collect();
        assert_eq!(symbols, ["USDC", "WETH"]);
        // --no-fund.
        assert!(
            plan_funding(&c, "base", &c.chains["base"], &fork_args(None, true))
                .unwrap()
                .is_none()
        );
        // No signer (eth) or no cached address (zksync): nothing by
        // default, an error when funding was asked for.
        for chain in ["eth", "zksync"] {
            assert!(
                plan_funding(&c, chain, &c.chains[chain], &fork_args(None, false))
                    .unwrap()
                    .is_none()
            );
            let err = plan_funding(&c, chain, &c.chains[chain], &fork_args(Some(""), false))
                .unwrap_err()
                .to_string();
            assert!(err.contains("no signer with a cached address"), "{err}");
        }
    }

    #[test]
    fn summary() {
        let record = ForkRecord {
            format: ForkRecord::FORMAT,
            name: "base-fork".into(),
            pid: 1,
            anvil_pid: 2,
            port: 8601,
            parent: "base".into(),
            chain_id: 8453,
            fork_block: None,
            started_at: 0,
            ready: true,
        };
        assert_eq!(
            summary_line(&record, 38_112_904),
            "base-fork  http://127.0.0.1:8601  block 38,112,904  chain 8453"
        );
    }
}
