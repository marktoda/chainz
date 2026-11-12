//! `chain …`, `ls`, `use` and `update` (`chain set`).

mod set;

pub use set::set;

use crate::cli::{ChainAddArgs, ChainCommand, ChainRmArgs, ChainShowArgs, ListArgs, UseArgs};
use crate::load::open;
use crate::probe::{check, probe_urls, ranked_urls};
use crate::prompt::{Ask, Tty};
use crate::select::{Missing, pick_chain};
use crate::{net, ui, view};
use anyhow::{Context, Result, anyhow, bail};
use chainz_core::Chainz;
use chainz_core::labels::{LabelIndex, LabelSource};
use chainz_core::model::{Chain, Endpoint};
use chainz_core::names;
use chainz_core::resolve::{self, Policy};
use chainz_core::tx::{Attach, EndpointRole};
use chainz_rpc::ProbeResult;
use chainz_rpc::chainlist::{self, ChainlistEntry};

pub fn run(cmd: ChainCommand) -> Result<i32> {
    match cmd {
        ChainCommand::Ls(args) => ls(&args),
        ChainCommand::Add(args) => {
            let mut chainz = open()?;
            add(&mut chainz, &args, &mut crate::prompt::Tty)
        }
        ChainCommand::Show(args) => show(&args),
        ChainCommand::Set(args) => set(&args, &["chain", "set"]),
        ChainCommand::Rm(args) => rm(&args),
    }
}

/// `ls` / `chain ls`: config only (hot path, no network).
pub fn ls(args: &ListArgs) -> Result<i32> {
    let chainz = open()?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&view::chains(chainz.config(), args.reveal))?
        );
    } else {
        print!("{}", view::compact(chainz.config(), args.reveal));
    }
    Ok(0)
}

/// `use [CHAIN]`.
pub fn use_default(args: &UseArgs) -> Result<i32> {
    let mut chainz = open()?;
    if args.clear {
        chainz.transact(|tx| {
            tx.set_default(None);
            Ok(())
        })?;
        println!("Default chain cleared");
        return Ok(0);
    }
    let name = match &args.chain {
        Some(query) => resolve::configured(chainz.config(), query, Policy::Prefix)?
            .0
            .to_string(),
        None => pick_chain(
            &chainz,
            &mut Tty,
            "Default chain",
            &Missing {
                command: &["use"],
                off_tty: "pass the chain to make the default",
            },
        )?,
    };
    if chainz.config().default.as_deref() == Some(name.as_str()) {
        println!("No changes ('{name}' is already the default chain)");
        return Ok(0);
    }
    chainz.transact(|tx| {
        tx.set_default(Some(&name));
        Ok(())
    })?;
    println!("Default chain set to '{name}'");
    Ok(0)
}

fn show(args: &ChainShowArgs) -> Result<i32> {
    let chainz = open()?;
    let config = chainz.config();
    let (name, chain) = resolve::configured(config, &args.chain, Policy::Prefix)?;
    let v = view::ChainView::new(config, name, chain, args.reveal);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    let mut extra = Vec::new();
    if let Some(signer) = chain.signer.as_ref().and_then(|s| config.signers.get(s))
        && let Some(address) = signer.address
    {
        extra.push(("wallet".to_string(), address.to_string()));
    }
    let index = LabelIndex::new(config);
    let labels: Vec<String> = index
        .labels_on(chain.id)
        .into_iter()
        .filter(|l| !matches!(l.source, LabelSource::Signer))
        .map(|l| format!("@{}", l.name))
        .collect();
    if !labels.is_empty() {
        extra.push(("labels".to_string(), labels.join(" ")));
    }
    print!("{}", view::show(&v, &extra));
    Ok(0)
}

fn find_in_chainlist<'a>(entries: &'a [ChainlistEntry], query: &str) -> Result<&'a ChainlistEntry> {
    if let Ok(id) = query.parse::<u64>() {
        return entries.iter().find(|e| e.chain_id == id).ok_or_else(|| {
            anyhow!("chain ID {id} is not on chainlist; pass --name and --url to add it by hand")
        });
    }
    let q = query.to_ascii_lowercase();
    if let Some(id) = names::chain_id_for_short_name(query)
        && let Some(e) = entries.iter().find(|e| e.chain_id == id)
    {
        return Ok(e);
    }
    if let Some(e) = entries
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(query) || e.short_name.eq_ignore_ascii_case(query))
    {
        return Ok(e);
    }
    let matches: Vec<&ChainlistEntry> = entries
        .iter()
        .filter(|e| e.name.to_ascii_lowercase().contains(&q))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!("'{query}' is not on chainlist; pass a chain ID"),
        many => bail!(
            "'{query}' matches {} chainlist entries ({}…); pass a chain ID",
            many.len(),
            many.iter()
                .take(5)
                .map(|e| format!("{} {}", e.chain_id, e.name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Where `chain add` gets the chain from, decided from the arguments
/// alone (before any network access).
enum ChainSource {
    /// `--url`s with a chain ID or a built-in short name (`arb`): no
    /// chainlist needed (its cached entry, if any, only names the chain).
    /// With `--url`, `chain add` never goes online for the chain.
    Offline { id: u64 },
    /// Look the chain up on chainlist (a picker when no chain is given).
    Chainlist,
}

impl ChainSource {
    fn of(args: &ChainAddArgs, ask: &dyn Ask) -> Result<Self> {
        match &args.chain {
            Some(query) if !args.urls.is_empty() => query
                .parse()
                .ok()
                .or_else(|| names::chain_id_for_short_name(query))
                .map(|id| ChainSource::Offline { id })
                .ok_or_else(|| {
                    crate::cli::usage_error(
                        &["chain", "add"],
                        &format!(
                            "'{query}' is not a chain ID or a built-in short name; with --url, pass the chain ID (and --name {query} to name it)"
                        ),
                    )
                }),
            Some(_) => Ok(ChainSource::Chainlist),
            None if ask.is_interactive() => Ok(ChainSource::Chainlist),
            None => Err(crate::cli::usage_error(
                &["chain", "add"],
                "off a terminal, name the chain to add (its ID or name)",
            )),
        }
    }
}

/// `chain add` needed chainlist and could not get it (an error's context:
/// `init` then offers the offline path, a chain ID and an RPC URL).
#[derive(Debug)]
pub struct ChainlistUnavailable;

impl std::fmt::Display for ChainlistUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("chainlist is unavailable")
    }
}

/// The chain `chain add` found.
enum Found {
    /// By ID, with its chainlist entry when the cache has one.
    Offline {
        id: u64,
        cached: Option<ChainlistEntry>,
    },
    /// On chainlist.
    Listed(ChainlistEntry),
}

impl Found {
    fn id(&self) -> u64 {
        match self {
            Found::Offline { id, .. } => *id,
            Found::Listed(entry) => entry.chain_id,
        }
    }

    fn entry(&self) -> Option<&ChainlistEntry> {
        match self {
            Found::Offline { cached, .. } => cached.as_ref(),
            Found::Listed(entry) => Some(entry),
        }
    }
}

/// Look the chain up as `source` says.
fn find_chain(args: &ChainAddArgs, source: ChainSource, ask: &mut dyn Ask) -> Result<Found> {
    let cache = chainz_core::paths::cache_dir()?;
    if let ChainSource::Offline { id } = source {
        let cached = chainlist::read_cache(&cache)
            .and_then(|all| all.into_iter().find(|e| e.chain_id == id));
        return Ok(Found::Offline { id, cached });
    }
    let registry = net::chainlist(&cache, args.refresh).context(ChainlistUnavailable)?;
    let all = registry.entries;
    let entry = match &args.chain {
        Some(q) => find_in_chainlist(&all, q)?.clone(),
        None => {
            let items: Vec<String> = all
                .iter()
                .map(|e| format!("{} ({})", e.name, e.chain_id))
                .collect();
            all[ask.select("Chain to add", &items, 0)?].clone()
        }
    };
    Ok(Found::Listed(entry))
}

/// `chain add`: find the chain, pick its endpoints, and insert it with the
/// default signer and explorer attached (unless `--no-signer`).
pub fn add(chainz: &mut Chainz, args: &ChainAddArgs, ask: &mut dyn Ask) -> Result<i32> {
    let found = find_chain(args, ChainSource::of(args, ask)?, ask)?;
    let id = found.id();
    let short = args
        .name
        .clone()
        .or_else(|| names::short_name(id).map(str::to_string))
        .or_else(|| found.entry().map(|e| names::slugify(&e.name)))
        .ok_or_else(|| anyhow!("chain {id} has no built-in short name; pass --name"))?;
    chainz
        .config()
        .check_new_chain(&short, id, args.signer.as_deref())?;

    // The URLs, active first; a URL listed twice is added once.
    let mut urls: Vec<String> = Vec::new();
    let vars = &chainz.secrets()?.vars;
    let push = |urls: &mut Vec<String>, url: &str| {
        if !urls.iter().any(|u| Endpoint::same_url(u, url, vars)) {
            urls.push(url.to_string());
        }
    };
    match &found {
        Found::Listed(entry) if args.urls.is_empty() => {
            // Only URLs that make valid endpoints; websockets are kept but
            // not probed.
            let public: Vec<String> = chainlist::public_rpcs(entry)
                .into_iter()
                .filter(|u| Endpoint::check_url(u).is_ok())
                .collect();
            let http: Vec<String> = public
                .iter()
                .filter(|u| Endpoint::new(u.as_str()).is_http())
                .cloned()
                .collect();
            if http.is_empty() {
                bail!(
                    "chainlist lists no public HTTP RPC for {}; pass --url URL",
                    entry.name
                );
            }
            eprintln!(
                "{}",
                ui::dim(&format!("probing {} public RPCs…", http.len()))
            );
            let probed = probe_urls(id, &http);
            let chosen = choose_active(&probed, &entry.name, ask)?;
            push(&mut urls, chosen);
            for url in &public {
                push(&mut urls, url);
            }
        }
        _ => {
            for url in &args.urls {
                Endpoint::check_url(url)?;
                push(&mut urls, url);
            }
            let endpoint = Endpoint::new(urls[0].as_str());
            if !args.no_check && !endpoint.is_websocket() {
                check(
                    chainz,
                    &short,
                    id,
                    &names::endpoint_name_for_url(&urls[0]),
                    &endpoint,
                )?;
            }
        }
    }

    let aliases: Vec<String> = found
        .entry()
        .map(|e| e.name.clone())
        .filter(|n| !n.eq_ignore_ascii_case(&short))
        .into_iter()
        // Only a name no configured chain has (as its name or an alias,
        // in any case): the collision validation would refuse.
        .filter(|alias| matches!(resolve::exact(chainz.config(), alias), Ok(None)))
        .collect();
    let attach = if args.no_signer {
        Attach::ExplorerOnly
    } else {
        Attach::Defaults
    };
    let count = urls.len();
    let (active, moved, attached) = chainz.transact(|tx| {
        let new = Chain {
            id,
            aliases,
            signer: args.signer.clone(),
            explorer: None,
            rpc: String::new(),
            failover: vec![],
            rpcs: Default::default(),
        };
        let attached = tx.insert_chain(&short, &new, attach)?;
        let (mut active, mut moved) = (String::new(), Vec::new());
        for (i, url) in urls.iter().enumerate() {
            let role = match i {
                0 => EndpointRole::Active,
                _ => EndpointRole::Spare,
            };
            let (ep, m) = tx.add_endpoint_url(&short, None, &Endpoint::new(url.as_str()), role)?;
            if i == 0 {
                active = ep;
            }
            moved.extend(m);
        }
        Ok((active, moved, attached))
    })?;
    println!("Added chain '{short}' ({id}) with {count} endpoint(s); active: {active}");
    if let Some(signer) = &attached.signer {
        println!("  signer: {signer} (the only signer; --no-signer to skip)");
    }
    if let Some(explorer) = &attached.explorer {
        println!("  explorer: {explorer}");
    }
    ui::report_moved(&moved);
    let signers: Vec<&str> = chainz.config().signers.keys().map(String::as_str).collect();
    if attach == Attach::Defaults && args.signer.is_none() && signers.len() > 1 {
        super::signer::warn_signerless(
            &[short.clone()],
            &format!(
                "{} signers to choose from: {}",
                signers.len(),
                signers.join(", ")
            ),
        );
    }
    Ok(0)
}

/// The probed URL to make active: the fastest healthy one, or the user's
/// pick from the ranked list on a TTY.
fn choose_active<'a>(
    probed: &'a [(String, ProbeResult)],
    chain: &str,
    ask: &mut dyn Ask,
) -> Result<&'a str> {
    let (ranking, items) = ranked_urls(probed);
    let Some(best) = ranking.best().filter(|&i| probed[i].1.is_ok()) else {
        bail!("no public RPC for {chain} answered; pass --url URL");
    };
    if !ask.is_interactive() {
        return Ok(&probed[best].0);
    }
    Ok(&probed[ranking.order[ask.select("Active RPC", &items, 0)?]].0)
}

fn rm(args: &ChainRmArgs) -> Result<i32> {
    let mut chainz = open()?;
    let name = resolve::configured(chainz.config(), &args.chain, Policy::Exact)?
        .0
        .to_string();
    let going = chainz.preview(|tx| tx.remove_chain(&name))?;
    let mut with = vec![format!("{} endpoint(s)", going.endpoints)];
    if !going.labels.is_empty() {
        with.push(format!("labels {}", at_labels(&going.labels)));
    }
    if going.was_default {
        with.push("it is the default chain".to_string());
    }
    crate::prompt::confirm_removal(
        &mut Tty,
        args.yes,
        &format!("chain rm {}", args.chain),
        &format!("chain '{name}' ({})", with.join("; ")),
    )?;
    let removed = chainz.transact(|tx| tx.remove_chain(&name))?;
    println!("Removed chain '{name}'");
    if !removed.labels.is_empty() {
        println!("  dropped labels: {}", at_labels(&removed.labels));
    }
    if removed.was_default {
        println!("  default cleared; set one with `chainz use <chain>`");
    }
    Ok(0)
}

/// `@a, @b`.
fn at_labels(labels: &[String]) -> String {
    labels
        .iter()
        .map(|l| format!("@{l}"))
        .collect::<Vec<_>>()
        .join(", ")
}
