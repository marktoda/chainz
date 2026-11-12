//! The chain verbs: `ls`, `use`, `add`, `update` and `rm`.

mod update;

pub use update::update;

use crate::cli::{AddArgs, ListArgs, RmArgs, UseArgs};
use crate::load::{health_store, open};
use crate::probe::{check, probe_urls, ranked_urls};
use crate::prompt::{Ask, Removal, Tty};
use crate::{net, ui, view};
use anyhow::{Context, Result, anyhow, bail};
use chainz_core::Chainz;
use chainz_core::labels::{LabelIndex, LabelSource};
use chainz_core::model::{Chain, Endpoint};
use chainz_core::names;
use chainz_core::resolve::{self, Policy};
use chainz_core::tx::{Attach, EndpointRole};
use chainz_rpc::ProbeResult;
use chainz_rpc::chainlist::{self, ChainlistEntry, ChainlistError, Registry};

/// `ls [CHAIN]`: config only (hot path, no network). The table reads no
/// more than the config; one chain in full and `--json` add the recorded
/// endpoint health.
pub fn ls(args: &ListArgs) -> Result<i32> {
    let chainz = open()?;
    if let Some(query) = &args.chain {
        return show(&chainz, query, args);
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&view::chains(
                chainz.config(),
                args.reveal,
                &health_store()
            ))?
        );
    } else {
        print!("{}", view::compact(chainz.config(), args.reveal));
    }
    Ok(0)
}

/// `use [CHAIN]`: with no chain, show the default (never a picker).
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
    let Some(query) = &args.chain else {
        print!(
            "{}",
            view::default_chain(chainz.config().default.as_deref())
        );
        return Ok(0);
    };
    let name = resolve::configured(chainz.config(), query, Policy::Prefix)?
        .0
        .to_string();
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

/// `ls CHAIN`: one chain in full.
fn show(chainz: &Chainz, query: &str, args: &ListArgs) -> Result<i32> {
    let config = chainz.config();
    let (name, chain) = resolve::configured(config, query, Policy::Prefix)?;
    let v = view::ChainView::new(config, name, chain, args.reveal, &health_store());
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
    print!("{}", view::show(&v, &extra, chainz_os::now_ms()));
    Ok(0)
}

/// The chainlist entry `query` names: a built-in short name (`arb`) first,
/// then [`Registry::lookup`].
fn find_in_chainlist<'a>(registry: &'a Registry, query: &str) -> Result<&'a ChainlistEntry> {
    if let Some(entry) = names::builtin_chain_id(query).and_then(|id| registry.find(id).ok()) {
        return Ok(entry);
    }
    registry.lookup(query).map_err(|e| {
        let hint = match e {
            ChainlistError::NotFound(_) => "pass --name and --url to add it by hand",
            _ => "pass a chain ID",
        };
        anyhow!("{e}; {hint}")
    })
}

/// Where `add` gets the chain from, decided from the arguments
/// alone (before any network access).
enum ChainSource {
    /// `--url`s with a chain ID or a built-in short name (`arb`): no
    /// chainlist needed (its cached entry, if any, only names the chain).
    /// With `--url`, `add` never goes online for the chain.
    Offline { id: u64 },
    /// Look the chain up on chainlist (a picker when no chain is given).
    Chainlist,
}

impl ChainSource {
    fn of(args: &AddArgs, ask: &dyn Ask) -> Result<Self> {
        match &args.chain {
            Some(query) if !args.urls.is_empty() => query
                .parse()
                .ok()
                .or_else(|| names::builtin_chain_id(query))
                .map(|id| ChainSource::Offline { id })
                .ok_or_else(|| {
                    crate::cli::usage_error(
                        &["add"],
                        &format!(
                            "'{query}' is not a chain ID or a built-in chain name; with --url, pass the chain ID (and --name {query} to name it)"
                        ),
                    )
                }),
            Some(_) => Ok(ChainSource::Chainlist),
            None if ask.is_interactive() => Ok(ChainSource::Chainlist),
            None => Err(crate::cli::usage_error(
                &["add"],
                "off a terminal, name the chain to add (its ID or name)",
            )),
        }
    }
}

/// `add` needed chainlist and could not get it (an error's context:
/// `init` then offers the offline path, a chain ID and an RPC URL).
#[derive(Debug)]
pub struct ChainlistUnavailable;

impl std::fmt::Display for ChainlistUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("chainlist is unavailable")
    }
}

/// The chain `add` found.
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
fn find_chain(args: &AddArgs, source: ChainSource, ask: &mut dyn Ask) -> Result<Found> {
    if let ChainSource::Offline { id } = source {
        let cached = chainlist::read_cache(&chainz_core::paths::cache_dir()?)
            .and_then(|all| all.into_iter().find(|e| e.chain_id == id));
        return Ok(Found::Offline { id, cached });
    }
    let registry = net::chainlist(args.refresh).context(ChainlistUnavailable)?;
    let entry = match &args.chain {
        Some(q) => find_in_chainlist(&registry, q)?.clone(),
        None => {
            let items: Vec<String> = registry
                .entries
                .iter()
                .map(|e| format!("{} ({})", e.name, e.chain_id))
                .collect();
            registry.entries[ask.select("Chain to add", &items, 0)?].clone()
        }
    };
    Ok(Found::Listed(entry))
}

/// `add`: find the chain, pick its endpoints, and insert it with the
/// default signer and explorer attached (unless `--no-signer`).
pub fn add(chainz: &mut Chainz, args: &AddArgs, ask: &mut dyn Ask) -> Result<i32> {
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
            // Websockets are added too, but not probed.
            let http = chainlist::public_http_rpcs(entry);
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
            for url in &chainlist::public_rpcs(entry) {
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
    println!(
        "Added chain '{short}' ({id}) with {}; active: {active}",
        ui::count(count, "endpoint")
    );
    if let Some(signer) = &attached.signer {
        println!("  signer: {signer} (the only signer; --no-signer to skip)");
    }
    if let Some(explorer) = &attached.explorer {
        println!("  explorer: {explorer}");
    }
    if let Some(gap) = &attached.explorer_skipped {
        println!("  explorer: none ({gap})");
    }
    ui::report_moved(&moved);
    if let Some(why) = &attached.signer_skipped {
        super::signer::warn_signerless(std::slice::from_ref(&short), why);
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

pub fn rm(args: &RmArgs) -> Result<i32> {
    let mut chainz = open()?;
    let name = resolve::configured(chainz.config(), &args.chain, Policy::Exact)?
        .0
        .to_string();
    let going = chainz.preview(|tx| tx.remove_chain(&name))?;
    let mut with = vec![ui::count(going.endpoints, "endpoint")];
    if !going.labels.is_empty() {
        with.push(format!("labels {}", at_labels(&going.labels)));
    }
    if going.was_default {
        with.push("it is the default chain".to_string());
    }
    crate::prompt::confirm_removal(
        &mut Tty,
        args.yes,
        &Removal::of(
            &format!("rm {}", args.chain),
            &format!("chain '{name}' ({})", with.join("; ")),
        ),
    )?;
    let removed = chainz.transact(|tx| tx.remove_chain(&name))?;
    println!("Removed chain '{name}'");
    if !removed.labels.is_empty() {
        println!("  dropped labels: {}", at_labels(&removed.labels));
    }
    if removed.was_default {
        println!("  default cleared; set one with `chainz use CHAIN`");
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
