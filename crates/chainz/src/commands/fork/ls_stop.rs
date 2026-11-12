//! `fork ls` and `fork stop`.

use super::process;
use super::start::anvil_target;
use crate::commands::runtime;
use crate::{net, view};
use anyhow::{Context as _, Result, bail};
use chainz_core::fmt::{group, uptime};
use chainz_core::runtime::ForkRecord;
use chainz_rpc::RpcClient;
use chainz_sys::now_secs;
use serde::Serialize;
use std::time::Duration;

/// How long `fork ls` waits for a fork's block height. Forks are local, but
/// one busy fetching fork state, or a loaded machine, can be slow.
const HEIGHT_TIMEOUT: Duration = Duration::from_secs(5);

/// One row of `fork ls --json`.
#[derive(Serialize)]
struct ForkRow<'a> {
    #[serde(flatten)]
    record: &'a ForkRecord,
    url: String,
    /// The fork's current block height, if it answered.
    block: Option<u64>,
    uptime_secs: u64,
}

pub fn ls(json: bool) -> Result<i32> {
    let runtime = runtime()?;
    process::reap(&runtime);
    let forks = runtime
        .list_forks()
        .context("unable to read the fork registry")?;
    if forks.is_empty() && !json {
        println!("no forks running (start one with `chainz fork CHAIN`)");
        return Ok(0);
    }
    let heights = net::block_on(heights(&forks));
    let now = now_secs();
    let rows: Vec<ForkRow> = forks
        .iter()
        .zip(heights)
        .map(|(record, block)| ForkRow {
            record,
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
                row.record.name.clone(),
                row.url.clone(),
                row.record.parent.clone(),
                row.block.map_or_else(|| "-".to_string(), group),
                uptime(row.uptime_secs),
                row.record.pid.to_string(),
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
async fn heights(forks: &[ForkRecord]) -> Vec<Option<u64>> {
    let client = RpcClient::with_timeout(HEIGHT_TIMEOUT);
    let mut set = tokio::task::JoinSet::new();
    for (index, fork) in forks.iter().enumerate() {
        let client = client.clone();
        let target = anvil_target(fork.chain_id, fork.port);
        set.spawn(async move {
            let height = match target {
                Ok(target) => client.block_number(&target).await.ok(),
                Err(_) => None,
            };
            (index, height)
        });
    }
    let mut heights = vec![None; forks.len()];
    while let Some(joined) = set.join_next().await {
        if let Ok((index, height)) = joined {
            heights[index] = height;
        }
    }
    heights
}

pub fn stop(name: Option<&str>, all: bool) -> Result<i32> {
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
