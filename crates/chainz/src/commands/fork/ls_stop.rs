//! `fork ls` and `fork stop`.

use super::process;
use super::start::anvil_target;
use crate::{net, ui, view};
use anyhow::{Context as _, Result, bail};
use chainz_core::fmt::{group, uptime};
use chainz_core::runtime::{ForkEntry, ForkRecord, Runtime};
use chainz_rpc::RpcClient;
use chainz_sys::now_secs;
use serde::Serialize;
use std::time::Duration;

/// How long `fork ls` waits for a fork's block height. Forks are local, but
/// one busy fetching fork state, or a loaded machine, can be slow.
const HEIGHT_TIMEOUT: Duration = Duration::from_secs(5);

/// One row of `fork ls --json`: the command's own contract, not the
/// registry's record format.
#[derive(Serialize)]
struct ForkRow<'a> {
    name: &'a str,
    pid: u32,
    port: u16,
    parent: &'a str,
    chain_id: u64,
    fork_block: Option<u64>,
    started_at: u64,
    url: String,
    /// The fork's current block height, if it answered.
    block: Option<u64>,
    uptime_secs: u64,
}

pub fn ls(json: bool) -> Result<i32> {
    let runtime = Runtime::resolve()?;
    process::reap(&runtime)?;
    let entries = runtime
        .forks()
        .context("unable to read the fork registry")?;
    for entry in &entries {
        if let (true, Err(e)) = (entry.live, &entry.record) {
            eprintln!("{}", ui::warn(&format!("fork '{}': {e}", entry.name)));
        }
    }
    let forks: Vec<&ForkRecord> = entries.iter().filter_map(|e| e.ready()).collect();
    if forks.is_empty() && !json {
        println!("no forks running (start one with `chainz fork CHAIN`)");
        return Ok(0);
    }
    let heights = net::block_on(heights(&forks))?;
    let now = now_secs();
    let rows: Vec<ForkRow> = forks
        .iter()
        .zip(heights)
        .map(|(record, block)| ForkRow {
            name: &record.name,
            pid: record.pid,
            port: record.port,
            parent: &record.parent,
            chain_id: record.chain_id,
            fork_block: record.fork_block,
            started_at: record.started_at,
            url: record.rpc_url(),
            block,
            uptime_secs: now.saturating_sub(record.started_at),
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|row| {
            [
                row.name.to_string(),
                row.url.clone(),
                row.parent.to_string(),
                row.block.map_or_else(|| "-".to_string(), group),
                uptime(row.uptime_secs),
                row.pid.to_string(),
            ]
        })
        .collect();
    print!(
        "{}",
        view::table(&["NAME", "URL", "PARENT", "BLOCK", "UPTIME", "PID"], &cells)
    );
    Ok(0)
}

/// Every fork's block height, asked concurrently.
async fn heights(forks: &[&ForkRecord]) -> Result<Vec<Option<u64>>> {
    let client = RpcClient::with_timeout(HEIGHT_TIMEOUT);
    let asks = forks.iter().map(|fork| {
        let client = client.clone();
        let target = anvil_target(fork.chain_id, fork.port);
        async move {
            match target {
                Ok(target) => client.block_number(&target).await.ok(),
                Err(_) => None,
            }
        }
    });
    net::join_indexed(asks.collect()).await
}

pub fn stop(name: Option<&str>, all: bool) -> Result<i32> {
    let runtime = Runtime::resolve()?;
    process::reap(&runtime)?;
    let entries = runtime
        .forks()
        .context("unable to read the fork registry")?;
    let targets: Vec<ForkEntry> = match (name, all) {
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
        process::stop(&runtime, &entry)?;
        println!("stopped {}", entry.name);
    }
    Ok(0)
}
