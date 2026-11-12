//! `rpc …`: endpoints, probing, ranking and discovery (via chainz-rpc).

use crate::args::parse_headers;
use crate::cli::{RpcAddArgs, RpcCommand, RpcRmArgs};
use crate::load::open;
use crate::probe::check;
use crate::prompt::{self, Tty};
use crate::ui;
use anyhow::{Result, bail};
use chainz_core::model::Endpoint;
use chainz_core::resolve::{self, Policy};
use chainz_core::tx::EndpointRole;

pub fn run(cmd: RpcCommand) -> Result<i32> {
    match cmd {
        RpcCommand::Add(args) => add(&args),
        RpcCommand::Rm(args) => rm(&args),
    }
}

fn add(args: &RpcAddArgs) -> Result<i32> {
    let mut chainz = open()?;
    let (chain, c) = resolve::configured(chainz.config(), &args.chain, Policy::Prefix)?;
    let (chain, c) = (chain.to_string(), c.clone());
    let endpoint = Endpoint {
        url: args.url.clone(),
        headers: parse_headers(&args.headers)?.into_iter().collect(),
    };
    let ep = c.new_endpoint(
        &chain,
        &args.url,
        args.name.as_deref(),
        &chainz.secrets()?.vars,
    )?;
    if !args.no_check && !endpoint.is_websocket() {
        check(&chainz, &chain, c.id, &ep, &endpoint)?;
    }
    let role = if args.activate {
        EndpointRole::Active
    } else if args.failover {
        EndpointRole::Failover
    } else {
        EndpointRole::Spare
    };
    let (ep, moved) =
        chainz.transact(|tx| tx.add_endpoint_url(&chain, Some(&ep), &endpoint, role))?;
    println!(
        "Added endpoint '{ep}' to '{chain}'{}",
        if args.activate { " (active)" } else { "" }
    );
    ui::report_moved(&moved);
    Ok(0)
}

fn rm(args: &RpcRmArgs) -> Result<i32> {
    let mut chainz = open()?;
    let (chain, c) = resolve::configured(chainz.config(), &args.chain, Policy::Prefix)?;
    let name = &args.name;
    if c.rpc == *name {
        bail!(
            "'{name}' is the active endpoint of '{chain}'; switch first with `chainz update {chain} --rpc <name>`"
        );
    }
    if !c.rpcs.contains_key(name) {
        bail!("chain '{chain}' has no endpoint '{name}'");
    }
    let chain = chain.to_string();
    prompt::confirm_removal(
        &mut Tty,
        args.yes,
        &format!("rpc rm {} {name}", args.chain),
        &format!("endpoint '{name}' from '{chain}'"),
    )?;
    let failover = c.failover.clone();
    chainz.transact(|tx| tx.remove_endpoint(&chain, name))?;
    println!("Removed endpoint '{name}' from '{chain}'");
    if failover.contains(name) {
        let rest: Vec<&str> = failover
            .iter()
            .filter(|f| *f != name)
            .map(String::as_str)
            .collect();
        println!(
            "  also removed from failover (now: {})",
            if rest.is_empty() {
                "none".to_string()
            } else {
                rest.join(", ")
            }
        );
    }
    Ok(0)
}
