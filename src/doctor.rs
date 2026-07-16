//! `chainz doctor`: config health checks and RPC repair.
//!
//! Failures (dangling key references, dead selected RPCs) make the command
//! exit nonzero; warnings (plaintext key storage) are informational only.
//! RPC failover deliberately lives here rather than in `exec`, which stays
//! network-free and fast.

use crate::{
    chain::rpc::{check_url, check_urls},
    config::Chainz,
    key::KeyType,
};
use anyhow::Result;
use colored::*;

pub struct Report {
    pub failures: usize,
    pub warnings: usize,
}

pub async fn run(chainz: &mut Chainz, fix: bool) -> Result<Report> {
    let mut report = Report {
        failures: 0,
        warnings: 0,
    };

    check_keys(chainz, &mut report);
    check_key_references(chainz, &mut report);
    let failed_chains = check_rpc_health(chainz, &mut report).await;

    if fix && !failed_chains.is_empty() {
        fix_rpcs(chainz, &failed_chains, &mut report).await?;
    }

    println!();
    match (report.failures, report.warnings) {
        (0, 0) => println!("{} no issues found", "✓".bright_green()),
        (f, w) => {
            println!(
                "{} {} failure(s), {} warning(s){}",
                if f > 0 {
                    "✗".bright_red().to_string()
                } else {
                    "⚠".bright_yellow().to_string()
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

fn check_keys(chainz: &Chainz, report: &mut Report) {
    println!("\n{}", "Keys".bright_blue().bold());
    let keys = chainz.list_keys();
    if keys.is_empty() {
        println!("  no keys configured");
    }
    for (name, key) in keys {
        if let KeyType::PrivateKey { .. } = key.kind {
            report.warnings += 1;
            println!(
                "  {} '{}' is stored as a plaintext private key — consider re-adding it with --type encrypted or --type keyring",
                "⚠".bright_yellow(),
                name
            );
        } else {
            println!("  {} {}", "✓".bright_green(), key);
        }
    }
}

fn check_key_references(chainz: &Chainz, report: &mut Report) {
    println!("\n{}", "Key references".bright_blue().bold());
    let mut ok = true;
    for chain in chainz.list_chains() {
        if chainz.get_key(&chain.key_name).is_err() {
            report.failures += 1;
            ok = false;
            println!(
                "  {} chain '{}' references missing key '{}'",
                "✗".bright_red(),
                chain.name,
                chain.key_name
            );
        }
    }
    if ok {
        println!(
            "  {} all chains reference existing keys",
            "✓".bright_green()
        );
    }
}

/// Concurrently health-check every chain's selected RPC.
/// Returns the names of chains whose RPC failed.
async fn check_rpc_health(chainz: &Chainz, report: &mut Report) -> Vec<String> {
    println!("\n{}", "RPC health".bright_blue().bold());
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
            let url = chainz.config.globals.expand_rpc_url(&c.selected_rpc);
            let chain_id = c.chain_id;
            let name = c.name.clone();
            tokio::spawn(async move { (name, healthy(&url, chain_id).await, url) })
        })
        .collect();

    let mut failed = Vec::new();
    for handle in checks {
        let Ok((name, is_healthy, url)) = handle.await else {
            continue;
        };
        if is_healthy {
            println!("  {} {} ({})", "✓".bright_green(), name, url);
        } else {
            report.failures += 1;
            println!("  {} {} ({})", "✗".bright_red(), name, url);
            failed.push(name);
        }
    }
    failed
}

async fn healthy(expanded_url: &str, chain_id: u64) -> bool {
    check_url(expanded_url, chain_id).await.is_ok()
}

async fn fix_rpcs(chainz: &mut Chainz, failed: &[String], report: &mut Report) -> Result<()> {
    println!("\n{}", "Fixing RPCs".bright_blue().bold());
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
                    "  {} {}: switched to {}",
                    "✓".bright_green(),
                    name,
                    expanded[i]
                );
                report.failures = report.failures.saturating_sub(1);
                fixed_any = true;
            }
            None => println!(
                "  {} {}: no healthy alternative among {} configured RPC(s)",
                "✗".bright_red(),
                name,
                chain.rpc_urls.len()
            ),
        }
    }
    if fixed_any {
        chainz.save().await?;
    }
    Ok(())
}
