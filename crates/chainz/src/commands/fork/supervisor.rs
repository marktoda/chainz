//! The `chainz fork` handlers: start (supervise), `ls`, `stop`.

use super::fund::{self, FundPlan};
use super::{process, proxy};
use crate::cli::{ForkArgs, ForkCommand};
use crate::commands::{chain_arg, open};
use crate::{net, ui};
use anyhow::{Context as _, Result, anyhow, bail};
use chainz_core::names::{self, RESERVED_FORK_NAMES};
use chainz_core::runtime::{ForkRecord, Runtime, ServeInfo};
use chainz_core::{Address, Chain, Chainz, Config};
use chainz_rpc::{Capabilities, HttpUpstream, RpcClient, Target, probe_capabilities};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Fork ports are allocated from here upward.
const FIRST_PORT: u16 = 8601;
/// Set in the re-exec'd background supervisor of `--detach`.
const SUPERVISOR_ENV: &str = "CHAINZ_FORK_SUPERVISOR";
/// How long anvil may take to come up (it fetches the fork block first).
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Chains anvil can't fork.
const UNFORKABLE: &[(u64, &str)] = &[(324, "zkSync Era"), (300, "zkSync Sepolia")];

/// `chainz fork …`.
pub fn run(cmd: Option<ForkCommand>, args: ForkArgs) -> Result<i32> {
    match cmd {
        None => start(args),
        Some(ForkCommand::Ls) => ls(),
        Some(ForkCommand::Stop { name, all }) => stop(name.as_deref(), all),
    }
}

fn runtime() -> Result<Runtime> {
    Runtime::resolve().context("unable to find the runtime directory")
}

// ------------------------------------------------------------------ rules

/// Reject chains anvil can't fork, before anything starts.
fn ensure_forkable(chain_name: &str, chain_id: u64) -> Result<()> {
    match UNFORKABLE.iter().find(|(id, _)| *id == chain_id) {
        Some((_, label)) => bail!(
            "'{chain_name}' can't be forked: anvil does not support {label} (chain {chain_id}), which is not EVM-equivalent"
        ),
        None => Ok(()),
    }
}

/// A fork name must be a valid chain name, not a `fork` subcommand, and
/// must not shadow a configured chain's name or alias or a live fork.
fn validate_name(config: &Config, live: &[String], name: &str) -> Result<()> {
    if RESERVED_FORK_NAMES.contains(&name) {
        bail!("a fork can't be named '{name}': it is a `chainz fork` subcommand");
    }
    if !names::is_valid_chain_name(name) {
        bail!(
            "invalid fork name '{name}': use lowercase letters, digits, '-' and '_', not digits only"
        );
    }
    for (chain_name, chain) in &config.chains {
        if chain_name.eq_ignore_ascii_case(name) {
            bail!("fork name '{name}' is taken by a configured chain; pick another with --name");
        }
        if chain.matches_alias(name) {
            bail!(
                "fork name '{name}' is an alias of configured chain '{chain_name}'; pick another with --name"
            );
        }
    }
    if live.iter().any(|l| l == name) {
        bail!(
            "fork '{name}' is already running; stop it with `chainz fork stop {name}` or pick another with --name"
        );
    }
    Ok(())
}

/// The first port from `start` upward that is not busy.
fn first_free_port(start: u16, mut busy: impl FnMut(u16) -> bool) -> Option<u16> {
    (start..=u16::MAX).find(|port| !busy(*port))
}

/// Whether something already listens on the loopback port.
fn port_busy(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_err()
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

// ----------------------------------------------------------------- output

/// `38112904` → `38,112,904`.
fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `0x0123…4567`.
fn short_address(address: &Address) -> String {
    let full = address.to_checksum();
    format!("{}…{}", &full[..6], &full[full.len() - 4..])
}

fn format_uptime(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        3600..86400 => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d {}h", secs / 86400, secs % 86400 / 3600),
    }
}

fn summary_line(record: &ForkRecord, block: u64) -> String {
    format!(
        "{}  {}  block {}  chain {}",
        record.name,
        record.rpc_url(),
        group_thousands(block),
        record.chain_id
    )
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn anvil_target(chain_id: u64, port: u16) -> Result<Target> {
    Target::new(chain_id, "anvil", &format!("http://127.0.0.1:{port}"), &[])
        .map_err(|e| anyhow!("{e}"))
}

// ------------------------------------------------------------------ start

fn start(args: ForkArgs) -> Result<i32> {
    let chainz = open()?;
    let runtime = runtime()?;
    let query = args
        .chain
        .as_deref()
        .ok_or_else(|| anyhow!("no chain given: chainz fork CHAIN"))?;
    let parent = chain_arg(&chainz, query)?;
    let config = chainz.config();
    let chain = &config.chains[&parent];
    ensure_forkable(&parent, chain.id)?;

    process::reap(&runtime);
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("{parent}-fork"));
    let live: Vec<String> = process::entries(&runtime)
        .into_iter()
        .map(|e| e.name)
        .collect();
    validate_name(config, &live, &name)?;
    let funding = plan_funding(config, &parent, chain, &args)?;
    match args.port {
        Some(0) => bail!("--port 0 is not a usable port"),
        Some(port) if port_busy(port) => bail!("port {port} is already in use"),
        _ => {}
    }

    let detached = std::env::var_os(SUPERVISOR_ENV).is_some();
    if args.detach && !detached {
        return detach(&runtime, &name);
    }

    process::install_signal_handlers();
    process::claim(&runtime, &name)?;
    let mut supervisor = Supervisor {
        runtime: &runtime,
        name: name.clone(),
        anvil: None,
    };
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

/// `--detach`: re-exec this command as a background supervisor in its own
/// session and return once it has registered the fork. The child's stdout
/// is a pipe it closes after printing the summary; its stderr is the log.
fn detach(runtime: &Runtime, name: &str) -> Result<i32> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let log_path = process::log_path(name)?;
    let log = process::open_log(&log_path, true)?;
    let exe = std::env::current_exe().context("unable to find the chainz binary")?;
    let mut command = Command::new(exe);
    command
        .args(std::env::args_os().skip(1))
        .env(SUPERVISOR_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    process::detach_session(&mut command);
    let mut child = command
        .spawn()
        .context("unable to start the background supervisor")?;
    let mut summary = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut summary);
    }
    print!("{summary}");
    let registered = runtime
        .read_fork(name)
        .ok()
        .flatten()
        .is_some_and(|record| record.pid == child.id());
    if registered {
        eprintln!(
            "{}",
            ui::dim(&format!(
                "detached (pid {}); log {}; stop with `chainz fork stop {name}`",
                child.id(),
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
/// anvil and removes the registry entry, on every exit path.
struct Supervisor<'a> {
    runtime: &'a Runtime,
    name: String,
    anvil: Option<Child>,
}

impl Drop for Supervisor<'_> {
    fn drop(&mut self) {
        if let Some(mut anvil) = self.anvil.take() {
            process::stop_child(&mut anvil);
        }
        process::release(self.runtime, &self.name);
    }
}

/// A fork that is up and registered.
struct Running {
    /// Keeps the proxy serving for the fork's whole life.
    _proxy: proxy::Proxy,
    log_path: std::path::PathBuf,
}

async fn supervise(supervisor: &mut Supervisor<'_>, job: &Job<'_>) -> Result<i32> {
    let running = tokio::select! {
        result = start_fork(supervisor, job) => result?,
        () = process::shutdown() => return Ok(130),
    };
    let me = std::process::id();
    loop {
        if process::shutdown_requested() {
            eprintln!("{}", ui::dim(&format!("stopped {}", job.name)));
            return Ok(0);
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
        // `fork stop` (or a reaper) removed the record: stand down.
        if let Ok(record) = job.runtime.read_fork(job.name)
            && record.is_none_or(|r| r.pid != me)
        {
            return Ok(0);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn start_fork(supervisor: &mut Supervisor<'_>, job: &Job<'_>) -> Result<Running> {
    let Job {
        runtime,
        parent,
        chain,
        name,
        args,
        ..
    } = *job;
    let client = RpcClient::new();
    let (endpoint, target) = choose_endpoint(job, &client).await?;

    // One upstream for the fork's whole life, always behind our own
    // proxy: the real endpoint, or a running gateway pinned to it. Anvil's
    // argv (world-readable in /proc) then only ever holds the proxy's
    // per-fork token, never the gateway's token or an endpoint secret.
    let (target, route) = match gateway(runtime) {
        Some(info) => {
            let url = info.chain_url(parent, Some(&endpoint));
            let target = Target::new(chain.id, endpoint.as_str(), &url, &[])
                .map_err(|e| anyhow!("gateway URL: {e}"))?;
            (target, "gateway")
        }
        None => (target, "proxy"),
    };
    let upstream = Arc::new(HttpUpstream::new(client.clone()));
    let proxy = proxy::start(upstream, target)
        .await
        .context("unable to start the fork proxy")?;
    let fork_url = proxy.url();

    let log_path = process::log_path(name)?;
    let log = process::open_log(&log_path, !job.detached)?;
    let chain_id = args.chain_id.unwrap_or(chain.id);
    let mut from = FIRST_PORT;
    let mut attempt = 0;
    let port = loop {
        attempt += 1;
        let port = match args.port {
            Some(port) => port,
            None => {
                let taken: Vec<u16> = process::entries(runtime)
                    .into_iter()
                    .filter(|e| e.name != name)
                    .filter_map(|e| e.port)
                    .collect();
                first_free_port(from, |p| taken.contains(&p) || port_busy(p))
                    .ok_or_else(|| anyhow!("no free port from {FIRST_PORT} upward"))?
            }
        };
        let log_start = log.metadata().map(|m| m.len()).unwrap_or(0);
        let spec = process::AnvilSpec {
            fork_url: &fork_url,
            port,
            block: args.block,
            chain_id: args.chain_id,
        };
        let anvil = process::spawn_anvil(&spec, &log)?;
        process::write_pids(
            runtime,
            name,
            &process::Pids {
                supervisor: std::process::id(),
                anvil: Some(anvil.id()),
                port: Some(port),
            },
        )?;
        let anvil = supervisor.anvil.insert(anvil);
        match wait_ready(anvil, chain_id, port, &log_path, log_start).await? {
            None => break port,
            Some(status) => {
                supervisor.anvil = None;
                // Lost a race for an auto-allocated port: take the next.
                if args.port.is_none() && attempt < 5 && port_busy(port) {
                    from = port.saturating_add(1);
                    continue;
                }
                bail!(
                    "anvil exited before the fork was ready ({status}):\n{}",
                    process::log_tail(&log_path, log_start, 12)
                );
            }
        }
    };

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

    let record = ForkRecord {
        name: name.to_string(),
        pid: std::process::id(),
        port,
        parent: parent.to_string(),
        chain_id,
        fork_block: args.block,
        started_at: now_secs(),
    };
    runtime
        .write_fork(&record)
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
        process::redirect_output(&log);
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

/// The gateway's discovery record, when one is running.
fn gateway(runtime: &Runtime) -> Option<ServeInfo> {
    if !runtime.serve_running() {
        return None;
    }
    runtime.read_serve_info().ok().flatten()
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
    let listening = format!("Listening on 127.0.0.1:{port}");
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

// --------------------------------------------------------------- endpoint

/// The one endpoint this fork uses for its whole life.
///
/// Without `--block`: the chain's active endpoint. With `--block`: an
/// archive-capable one (cached capability tags first, then a fresh probe),
/// preferring the active endpoint, then the failover list, then the rest.
/// The fork refuses to start when none qualifies.
async fn choose_endpoint(job: &Job<'_>, client: &RpcClient) -> Result<(String, Target)> {
    let Job {
        chainz,
        parent,
        chain,
        ..
    } = *job;
    let Some(block) = job.args.block else {
        let name = &chain.rpc;
        let endpoint = chain
            .rpcs
            .get(name)
            .ok_or_else(|| anyhow!("chain '{parent}' has no endpoint '{name}'"))?;
        if endpoint.is_websocket() {
            bail!(
                "chain '{parent}' is on websocket endpoint '{name}', which a fork can't use; activate an HTTP endpoint with `chainz rpc use {parent} NAME`"
            );
        }
        let target = net::target(chainz, parent, chain, name, endpoint)?;
        client
            .check_chain_id(&target, chain.id)
            .await
            .map_err(|e| anyhow!("can't fork '{parent}' from endpoint '{name}': {e}"))?;
        return Ok((name.clone(), target));
    };

    let mut candidates: Vec<(String, Target)> = Vec::new();
    for name in archive_order(chain) {
        let endpoint = &chain.rpcs[&name];
        if endpoint.is_websocket() {
            continue;
        }
        match net::target(chainz, parent, chain, &name, endpoint) {
            Ok(target) => candidates.push((name, target)),
            Err(e) => eprintln!("{}", ui::warn(&format!("{e:#}"))),
        }
    }
    let mut store = net::health_store();
    for (name, target) in &candidates {
        let cached = store
            .capabilities(chain.id, name)
            .is_some_and(|c| c.archive == Some(true));
        if cached && client.check_chain_id(target, chain.id).await.is_ok() {
            return Ok((name.clone(), target.clone()));
        }
    }

    eprintln!(
        "{}",
        ui::dim(&format!(
            "--block {block}: probing {} endpoint(s) of '{parent}' for archive state…",
            candidates.len()
        ))
    );
    let mut probes = tokio::task::JoinSet::new();
    for (index, (_, target)) in candidates.iter().enumerate() {
        let (client, target, id) = (client.clone(), target.clone(), chain.id);
        probes.spawn(async move {
            let capabilities = match client.check_chain_id(&target, id).await {
                Ok(()) => Some(probe_capabilities(&client, &target).await),
                Err(_) => None,
            };
            (index, capabilities)
        });
    }
    let mut probed: Vec<Option<Capabilities>> = vec![None; candidates.len()];
    while let Some(joined) = probes.join_next().await {
        if let Ok((index, capabilities)) = joined {
            probed[index] = capabilities;
        }
    }
    let mut chosen = None;
    for ((name, target), capabilities) in candidates.iter().zip(probed) {
        let Some(capabilities) = capabilities else {
            continue;
        };
        if chosen.is_none() && capabilities.archive == Some(true) {
            chosen = Some((name.clone(), target.clone()));
        }
        store.set_capabilities(chain.id, name, capabilities);
    }
    let _ = store.save();
    chosen.ok_or_else(|| {
        anyhow!(
            "--block {block} needs an archive endpoint, and none of the {} HTTP endpoint(s) of '{parent}' serves historical state; add one with `chainz rpc add {parent} URL`",
            candidates.len()
        )
    })
}

/// Endpoint names in preference order: active, failover list, the rest.
fn archive_order(chain: &Chain) -> Vec<String> {
    let mut order: Vec<String> = Vec::with_capacity(chain.rpcs.len());
    let preferred = std::iter::once(&chain.rpc).chain(&chain.failover);
    for name in preferred.chain(chain.rpcs.keys()) {
        if chain.rpcs.contains_key(name) && !order.contains(name) {
            order.push(name.clone());
        }
    }
    order
}

// -------------------------------------------------------------- ls / stop

fn ls() -> Result<i32> {
    let runtime = runtime()?;
    process::reap(&runtime);
    let forks = runtime
        .list_forks()
        .context("unable to read the fork registry")?;
    if forks.is_empty() {
        println!("no forks running (start one with `chainz fork CHAIN`)");
        return Ok(0);
    }
    let heights = net::block_on(async {
        let client = RpcClient::with_timeout(Duration::from_secs(2));
        let mut heights = Vec::with_capacity(forks.len());
        for fork in &forks {
            let height = match anvil_target(fork.chain_id, fork.port) {
                Ok(target) => client.block_number(&target).await.ok(),
                Err(_) => None,
            };
            heights.push(height);
        }
        heights
    });
    let now = now_secs();
    let mut rows = vec![["NAME", "URL", "PARENT", "BLOCK", "UPTIME", "PID"].map(String::from)];
    for (fork, height) in forks.iter().zip(heights) {
        rows.push([
            fork.name.clone(),
            fork.rpc_url(),
            fork.parent.clone(),
            height.map_or_else(|| "-".to_string(), group_thousands),
            format_uptime(now.saturating_sub(fork.started_at)),
            fork.pid.to_string(),
        ]);
    }
    print!("{}", table(&rows));
    Ok(0)
}

/// Left-aligned columns, two spaces apart.
fn table(rows: &[[String; 6]]) -> String {
    let mut widths = [0usize; 6];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

fn stop(name: Option<&str>, all: bool) -> Result<i32> {
    let runtime = runtime()?;
    process::reap(&runtime);
    let entries = process::entries(&runtime);
    let targets: Vec<process::Entry> = match (name, all) {
        (_, true) => entries,
        (Some(name), false) => {
            let live: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            match entries.iter().find(|e| e.name == name) {
                Some(entry) => vec![entry.clone()],
                None if live.is_empty() => bail!("no fork named '{name}' (no forks running)"),
                None => bail!("no fork named '{name}' (running: {})", live.join(", ")),
            }
        }
        (None, false) => bail!("which fork? chainz fork stop NAME, or --all"),
    };
    if targets.is_empty() {
        println!("no forks running");
        return Ok(0);
    }
    for entry in targets {
        process::stop_supervisor(entry.supervisor);
        if let Some(anvil) = entry.anvil {
            process::kill_anvil(anvil, entry.port);
        }
        process::forget(&runtime, &entry.name);
        println!("stopped {}", entry.name);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml_edit::de::from_str(
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
    fn name_rules() {
        let c = config();
        let live = vec!["base-fork".to_string()];
        let check = |name: &str| validate_name(&c, &live, name).map_err(|e| e.to_string());
        assert_eq!(check("eth-fork"), Ok(()));
        assert_eq!(check("scratch_2"), Ok(()));
        // Reserved subcommand names.
        assert!(check("ls").unwrap_err().contains("subcommand"));
        assert!(check("stop").unwrap_err().contains("subcommand"));
        // Configured chain names and aliases, case-insensitively.
        assert!(check("base").unwrap_err().contains("configured chain"));
        assert!(check("mainnet").unwrap_err().contains("alias of"));
        // A live fork.
        assert!(check("base-fork").unwrap_err().contains("already running"));
        // Shape: a chain ID must never resolve to a fork, and the name is
        // a file name in the registry.
        for bad in ["8453", "", "Base", "a b", "a/b", "../x", "a.b", "-x", "@x"] {
            assert!(check(bad).unwrap_err().contains("invalid"), "{bad:?}");
        }
    }

    #[test]
    fn unforkable_chains_are_rejected_up_front() {
        let err = ensure_forkable("zksync", 324).unwrap_err().to_string();
        assert!(err.contains("zkSync Era") && err.contains("324"), "{err}");
        assert!(ensure_forkable("base", 8453).is_ok());
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
    fn archive_candidates_prefer_active_then_failover() {
        let c = config();
        assert_eq!(archive_order(&c.chains["base"]), ["b", "c", "a"]);
    }

    #[test]
    fn output_formatting() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(38_112_904), "38,112,904");
        assert_eq!(format_uptime(5), "5s");
        assert_eq!(format_uptime(125), "2m 5s");
        assert_eq!(format_uptime(3780), "1h 3m");
        assert_eq!(format_uptime(180_000), "2d 2h");
        let address: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
            .parse()
            .unwrap();
        assert_eq!(short_address(&address), "0xf39F…2266");
        let record = ForkRecord {
            name: "base-fork".into(),
            pid: 1,
            port: 8601,
            parent: "base".into(),
            chain_id: 8453,
            fork_block: None,
            started_at: 0,
        };
        assert_eq!(
            summary_line(&record, 38_112_904),
            "base-fork  http://127.0.0.1:8601  block 38,112,904  chain 8453"
        );
        let rows = [
            ["NAME", "URL", "PARENT", "BLOCK", "UPTIME", "PID"].map(String::from),
            ["base-fork", "http://x", "base", "-", "5s", "42"].map(String::from),
        ];
        assert_eq!(
            table(&rows),
            "NAME       URL       PARENT  BLOCK  UPTIME  PID\nbase-fork  http://x  base    -      5s      42\n"
        );
    }
}
