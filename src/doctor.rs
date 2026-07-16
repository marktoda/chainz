//! `chainz doctor`: config health checks and RPC repair.
//!
//! Failures (dangling key references, dead selected RPCs) make the command
//! exit nonzero; warnings (plaintext key storage) are informational only.
//! RPC failover deliberately lives here rather than in `exec`, which stays
//! network-free and fast.

use crate::{
    chain::{
        Rpc,
        rpc::{create_provider, test_rpc},
    },
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

    let checks: Vec<_> = chains
        .iter()
        .map(|c| {
            let url = chainz.config.globals.expand_rpc_url(&c.selected_rpc);
            let name = c.name.clone();
            let chain_id = c.chain_id;
            tokio::spawn(async move {
                let healthy = check_rpc(&url, chain_id).await.is_ok();
                (name, url, healthy)
            })
        })
        .collect();

    let mut failed = Vec::new();
    for handle in checks {
        let Ok((name, url, healthy)) = handle.await else {
            continue;
        };
        if healthy {
            println!("  {} {} ({})", "✓".bright_green(), name, url);
        } else {
            report.failures += 1;
            println!("  {} {} ({})", "✗".bright_red(), name, url);
            failed.push(name);
        }
    }
    failed
}

async fn fix_rpcs(chainz: &mut Chainz, failed: &[String], report: &mut Report) -> Result<()> {
    println!("\n{}", "Fixing RPCs".bright_blue().bold());
    let mut fixed_any = false;
    for name in failed {
        let chain = chainz.config.get_chain(name)?;
        let mut fixed = false;
        for candidate in &chain.rpc_urls {
            if *candidate == chain.selected_rpc {
                continue;
            }
            let url = chainz.config.globals.expand_rpc_url(candidate);
            if check_rpc(&url, chain.chain_id).await.is_ok() {
                let mut updated = chain.clone();
                updated.selected_rpc = candidate.clone();
                chainz.add_chain(updated)?;
                println!("  {} {}: switched to {}", "✓".bright_green(), name, url);
                report.failures = report.failures.saturating_sub(1);
                fixed = true;
                fixed_any = true;
                break;
            }
        }
        if !fixed {
            println!(
                "  {} {}: no healthy alternative among {} configured RPC(s)",
                "✗".bright_red(),
                name,
                chain.rpc_urls.len()
            );
        }
    }
    if fixed_any {
        chainz.save().await?;
    }
    Ok(())
}

async fn check_rpc(expanded_url: &str, chain_id: u64) -> Result<()> {
    let provider = create_provider(expanded_url).await?;
    let rpc = Rpc {
        rpc_url: expanded_url.to_string(),
        provider,
    };
    test_rpc(&rpc, chain_id).await
}
