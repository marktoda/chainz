//! `doctor CHAIN --discover`: public endpoints from chainlist.

use crate::net;
use crate::probe::{self, probe_urls};
use crate::prompt::{Ask, Tty};
use crate::ui;
use anyhow::Result;
use chainz_core::Chainz;
use chainz_core::model::Endpoint;
use chainz_core::tx::EndpointRole;
use chainz_rpc::chainlist;

/// `doctor CHAIN --discover`: the public chainlist endpoints `chain`
/// doesn't have, probed and ranked; the ones picked (on a TTY), or every
/// one that answered (`yes`), are added as spares. Off a TTY without
/// `yes`, the ranked list and how to add them.
pub fn run(chainz: &mut Chainz, chain: &str, refresh: bool, yes: bool) -> Result<()> {
    let c = chainz.config().chains[chain].clone();
    println!("{}", ui::section(&format!("discover {chain}")));
    let registry = net::chainlist(&chainz_core::paths::cache_dir()?, refresh)?;
    let entry = registry.find(c.id)?;
    let vars = &chainz.secrets()?.vars;
    let fresh: Vec<String> = chainlist::public_rpcs(entry)
        .into_iter()
        .filter(|u| Endpoint::new(u.as_str()).is_http() && c.endpoint_at(u, vars).is_none())
        .collect();
    if fresh.is_empty() {
        println!("Every public chainlist RPC for '{chain}' is already configured");
        return Ok(());
    }
    let probed = probe_urls(c.id, &fresh);
    let (ranking, items) = probe::ranked_urls(&probed);
    let order = ranking.order;
    let answered: Vec<usize> = (0..order.len())
        .filter(|&p| probed[order[p]].1.is_ok())
        .collect();
    let picked = if yes {
        answered
    } else if Tty.is_interactive() {
        let defaults: Vec<bool> = (0..order.len()).map(|p| answered.contains(&p)).collect();
        Tty.multi_select("Endpoints to add", &items, &defaults)?
    } else {
        for item in &items {
            println!("{item}");
        }
        println!(
            "{}",
            ui::dim(&format!(
                "add them with `chainz doctor {chain} --discover --yes`, or one with `chainz update {chain} --add-rpc <url>`"
            ))
        );
        return Ok(());
    };
    if picked.is_empty() {
        println!("Nothing added");
        return Ok(());
    }
    let (added, moved) = chainz.transact(|tx| {
        let (mut added, mut moved) = (Vec::new(), Vec::new());
        for p in &picked {
            let url = &probed[order[*p]].0;
            let endpoint = Endpoint::new(url.as_str());
            let (ep, m) = tx.add_endpoint_url(chain, None, &endpoint, EndpointRole::Spare)?;
            added.push(ep);
            moved.extend(m);
        }
        Ok((added, moved))
    })?;
    println!(
        "Added {} to '{chain}': {}",
        ui::count(added.len(), "endpoint"),
        added.join(", ")
    );
    ui::report_moved(&moved);
    Ok(())
}
