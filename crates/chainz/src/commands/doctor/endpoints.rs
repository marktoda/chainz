//! `doctor`'s endpoint section: the active endpoint of every chain, or,
//! for one chain, every endpoint ranked.

use super::{Finding, Groups, Report};
use crate::net;
use crate::probe::{self, Depth, Repair, health_problem};
use crate::prompt::{self, Consent, Tty};
use crate::ui;
use anyhow::Result;
use chainz_core::Chainz;

/// `doctor` with no chain: probe the active endpoint of every chain;
/// with `fix`, switch a failing one to a healthy endpoint.
pub(super) fn rpc_health(chainz: &mut Chainz, fix: bool, report: &mut Report) -> Result<()> {
    let chains: Vec<String> = chainz.config().chains.keys().cloned().collect();
    if chains.is_empty() {
        report.ok("no chains");
        return Ok(());
    }
    let mut store = crate::load::health_store();
    let probed = net::block_on(probe::probe_chains(
        chainz,
        &chains,
        Depth::Health,
        &mut store,
    ))?;
    let mut repairs: Vec<(String, Repair)> = Vec::new();
    // Failing active endpoints, one line per endpoint name and problem;
    // whether a healthy endpoint could replace each.
    let mut failing: Groups<(Finding, Option<Repair>)> = Groups::new();
    for p in &probed {
        let c = &chainz.config().chains[&p.chain];
        unprobed(p, report);
        let Some(active) = p.index_of(&c.rpc) else {
            if c.active().is_some_and(|e| e.is_websocket()) {
                report.note(&format!(
                    "{}: active endpoint '{}' is a websocket (not probed)",
                    p.chain, c.rpc
                ));
            }
            continue;
        };
        let ranking = p.ranking(&store);
        let Some(problem) = health_problem(&ranking.health[active]) else {
            report.ok(&format!(
                "{}: {} {}ms",
                p.chain,
                c.rpc,
                p.probes[active].result.latency.as_millis()
            ));
            continue;
        };
        let finding = Finding {
            on: p.chain.clone(),
            alone: format!("{}: active endpoint '{}' {problem}", p.chain, c.rpc),
        };
        let repair = Repair::propose(&p.chain, c, p, &ranking);
        failing
            .entry((c.rpc.clone(), problem))
            .or_default()
            .push((finding, repair));
    }
    for ((ep, problem), group) in failing {
        let (findings, proposed): (Vec<Finding>, Vec<Option<Repair>>) = group.into_iter().unzip();
        report.problems(&findings, |places| {
            format!("active endpoint '{ep}' {problem} on {places}")
        });
        let proposed: Vec<Repair> = proposed.into_iter().flatten().collect();
        if proposed.len() < findings.len() {
            // Nothing configured can replace it: the next step is finding
            // other endpoints, as `doctor CHAIN` says.
            let chain = match findings.as_slice() {
                [one] => one.on.as_str(),
                _ => "CHAIN",
            };
            println!(
                "  {}",
                ui::dim(&format!(
                    "check your network, or find public endpoints with `chainz doctor {chain} --discover`"
                ))
            );
        }
        if fix {
            repairs.extend(proposed.into_iter().map(|r| (ep.clone(), r)));
            continue;
        }
        report.would_switch += proposed.len();
        match (findings.len(), proposed.as_slice()) {
            (_, []) => {}
            (1, [repair]) => {
                println!("  {}", ui::dim(&format!("'{}' is healthy", repair.active)));
            }
            (n, _) => println!(
                "  {}",
                ui::dim(&format!(
                    "a healthy endpoint can replace it on {} of {n}",
                    proposed.len()
                ))
            ),
        }
    }
    if !repairs.is_empty() {
        // Only the active endpoint: failover lists are left to
        // `doctor CHAIN --fix`, which ranks every endpoint.
        chainz.transact(|tx| {
            for (_, repair) in &repairs {
                repair.apply(tx, false)?;
            }
            Ok(())
        })?;
        for (old, repair) in repairs {
            report.fixed(&format!(
                "{}: active {old} → {}",
                repair.chain, repair.active
            ));
        }
    }
    Ok(())
}

/// Endpoints that could not be probed (a `${VAR}` set nowhere, say): a
/// problem each, since every command that uses them fails the same way.
fn unprobed(probes: &probe::ChainProbes, report: &mut Report) {
    for (_, why) in &probes.unprobed {
        report.problem(&format!("not probed: {why}"));
    }
}

/// `doctor CHAIN`: probe, rank and tag every endpoint of `chain`. A chain
/// with no endpoint that answered is a problem, and so is a failing
/// active endpoint. With `fix`, the best endpoint becomes active and the
/// proposed failover list is written when confirmed (or `yes`).
pub(super) fn endpoints(
    chainz: &mut Chainz,
    chain: &str,
    fix: bool,
    yes: bool,
    report: &mut Report,
) -> Result<()> {
    let mut store = crate::load::health_store();
    let probed = net::block_on(probe::probe_chains(
        chainz,
        &[chain.to_string()],
        Depth::Capabilities,
        &mut store,
    ))?;
    let p = &probed[0];
    let c = &chainz.config().chains[chain];
    let (ok, probed_total) = probe::answered(p);
    let total = probed_total + p.unprobed.len();
    println!(
        "{}  {}",
        ui::section(&format!("endpoints of {chain}")),
        ui::dim(&format!("({ok} of {total} answered)"))
    );
    unprobed(p, report);
    if p.probes.is_empty() {
        if p.unprobed.is_empty() {
            report.note(&format!("{chain} has no HTTP endpoint to probe"));
        }
        return Ok(());
    }
    let ranking = p.ranking(&store);
    probe::print_ranked(c, p, &ranking, &store);
    let failing = p
        .index_of(&c.rpc)
        .and_then(|active| health_problem(&ranking.health[active]));
    if let Some(problem) = &failing {
        report.problem(&format!("{chain}: active endpoint '{}' {problem}", c.rpc));
    }
    let Some(repair) = Repair::propose(chain, c, p, &ranking) else {
        report.problem(&format!(
            "no healthy endpoint on {chain}; check your network, or `chainz doctor {chain} --discover`"
        ));
        return Ok(());
    };
    if !fix {
        if repair.active != c.rpc {
            if failing.is_some() {
                report.would_switch += 1;
            }
            println!(
                "  {}",
                ui::dim(&format!("best: {} (--fix activates it)", repair.active))
            );
        }
        return Ok(());
    }
    println!(
        "{chain}: active → {}; proposed failover: {}",
        repair.active,
        if repair.failover.is_empty() {
            "-".into()
        } else {
            repair.failover.join(", ")
        }
    );
    let write_failover = !repair.failover.is_empty()
        && repair.failover != c.failover
        && prompt::confirm_or_yes(
            &mut Tty,
            yes,
            "Write the proposed failover list?",
            Consent::Proposal {
                note: "failover list not written (pass --yes, or confirm on a TTY)",
            },
        )?;
    let old = c.rpc.clone();
    chainz.transact(|tx| repair.apply(tx, write_failover))?;
    if failing.is_some() {
        report.fixed(&format!("{chain}: active {old} → {}", repair.active));
    }
    Ok(())
}
