//! `chainz doctor`: config health checks and RPC repair.
//!
//! Failures (dangling key references, dead selected RPCs) make the command
//! exit nonzero; warnings (plaintext key storage) are informational only.
//! RPC failover deliberately lives here rather than in `exec`, which stays
//! network-free and fast.

use crate::{chain::rpc::check_urls, config::Chainz, key::KeyType, ui};
use anyhow::Result;
use console::style;
use std::io::IsTerminal;

pub struct Report {
    pub failures: usize,
    pub warnings: usize,
}

pub async fn run(chainz: &mut Chainz, fix: bool) -> Result<Report> {
    let mut report = Report {
        failures: 0,
        warnings: 0,
    };

    check_config_invariants(chainz, &mut report);
    let plaintext_keys = check_keys(chainz, &mut report);
    if fix && plaintext_keys > 0 && std::io::stdin().is_terminal() {
        let migrate = dialoguer::Confirm::new()
            .with_prompt("Migrate plaintext keys to safe storage now?")
            .default(true)
            .interact()?;
        if migrate {
            let migrated = crate::key::migrate_plaintext_keys(chainz).await?;
            report.warnings = report.warnings.saturating_sub(migrated);
        }
    }
    check_key_references(chainz, &mut report);
    let failed_chains = check_rpc_health(chainz, &mut report).await;

    if fix && !failed_chains.is_empty() {
        fix_rpcs(chainz, &failed_chains, &mut report).await?;
    }

    println!();
    match (report.failures, report.warnings) {
        (0, 0) => println!("{}", ui::success("no issues found")),
        (f, w) => {
            println!(
                "{} {} failure(s), {} warning(s){}",
                if f > 0 {
                    style("✗").red().to_string()
                } else {
                    style("⚠").yellow().to_string()
                },
                f,
                w,
                if f > 0 && !fix {
                    " — run with --fix to attempt RPC repairs"
                } else {
                    ""
                }
            );
        }
    }
    Ok(report)
}

fn check_config_invariants(chainz: &Chainz, report: &mut Report) {
    println!("{}", ui::section("Configuration"));
    match chainz.config.validate() {
        Ok(()) => println!("  {}", ui::success("configuration invariants hold")),
        Err(error) => {
            report.failures += 1;
            println!("  {}", ui::fail(&format!("{:#}", error)));
        }
    }
}

fn check_keys(chainz: &Chainz, report: &mut Report) -> usize {
    println!("{}", ui::section("Keys"));
    let keys = chainz.list_keys();
    if keys.is_empty() {
        println!("  no keys configured");
    }
    let mut plaintext = 0;
    for (name, key) in keys {
        if let KeyType::PrivateKey { .. } = key.kind {
            report.warnings += 1;
            plaintext += 1;
            println!(
                "  {}",
                ui::warn(&format!(
                    "'{}' is stored as a plaintext private key — migrate with `chainz key migrate {}`",
                    name, name
                ))
            );
        } else {
            println!("  {}", ui::success(&key.to_string()));
        }
    }
    plaintext
}

fn check_key_references(chainz: &Chainz, report: &mut Report) {
    println!("{}", ui::section("Key references"));
    let mut ok = true;
    for chain in chainz.list_chains() {
        if chainz.get_key(&chain.key_name).is_err() {
            report.failures += 1;
            ok = false;
            println!(
                "  {}",
                ui::fail(&format!(
                    "chain '{}' references missing key '{}'",
                    chain.name, chain.key_name
                ))
            );
        }
    }
    if ok {
        println!("  {}", ui::success("all chains reference existing keys"));
    }
}

/// Concurrently health-check every chain's selected RPC.
/// Returns the names of chains whose RPC failed.
async fn check_rpc_health(chainz: &Chainz, report: &mut Report) -> Vec<String> {
    println!("{}", ui::section("RPC health"));
    let chains = chainz.list_chains();
    if chains.is_empty() {
        println!("  no chains configured");
        return vec![];
    }

    // The chain-id assertion inside check_urls means chains with different
    // ids can't share one batch; check per chain, but all chains in parallel.
    let checks: Vec<_> = chains
        .iter()
        .map(|c| {
            let expanded = chainz.config.globals.expand_rpc_url(&c.selected_rpc);
            let raw = c.selected_rpc.clone();
            let chain_id = c.chain_id;
            let name = c.name.clone();
            tokio::spawn(async move {
                let (healthy, latency) = crate::chain::rpc::probe(&expanded, chain_id).await;
                (name, healthy, raw, latency)
            })
        })
        .collect();

    let mut failed = Vec::new();
    for handle in checks {
        let Ok((name, is_healthy, raw_url, latency)) = handle.await else {
            continue;
        };
        if is_healthy {
            println!(
                "  {}",
                ui::success(&format!(
                    "{} ({}) {}ms",
                    name,
                    crate::variables::redact_url(&raw_url),
                    latency.as_millis()
                ))
            );
        } else {
            report.failures += 1;
            println!(
                "  {}",
                ui::fail(&format!(
                    "{} ({})",
                    name,
                    crate::variables::redact_url(&raw_url)
                ))
            );
            failed.push(name);
        }
    }
    failed
}

async fn fix_rpcs(chainz: &mut Chainz, failed: &[String], report: &mut Report) -> Result<()> {
    println!("{}", ui::section("Fixing RPCs"));
    let mut fixed_any = false;
    for name in failed {
        let chain = chainz.config.get_chain(name)?;
        // Probe all alternatives concurrently (chainlist chains can carry
        // dozens of RPCs; sequential 10s timeouts would stall for minutes),
        // then prefer the first healthy one in configured order.
        let candidates: Vec<&String> = chain
            .rpc_urls
            .iter()
            .filter(|url| **url != chain.selected_rpc)
            .collect();
        let expanded: Vec<String> = candidates
            .iter()
            .map(|url| chainz.config.globals.expand_rpc_url(url))
            .collect();
        let health = check_urls(&expanded, chain.chain_id).await;

        match health.iter().position(|h| *h) {
            Some(i) => {
                chainz.set_selected_rpc(name, candidates[i].clone())?;
                println!(
                    "  {}",
                    ui::success(&format!(
                        "{}: switched to {}",
                        name,
                        crate::variables::redact_url(candidates[i])
                    ))
                );
                report.failures = report.failures.saturating_sub(1);
                fixed_any = true;
            }
            None => println!(
                "  {}",
                ui::fail(&format!(
                    "{}: no healthy alternative among {} configured RPC(s)",
                    name,
                    chain.rpc_urls.len()
                ))
            ),
        }
    }
    if fixed_any {
        chainz.save().await?;
    }
    Ok(())
}
