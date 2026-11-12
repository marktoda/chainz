//! `chain set [CHAIN]` (also `update`): one patch to one chain, from flags
//! or, with none on a TTY, from the v1-style menu. Either way it is one
//! `ChainEdit` applied in one transaction.

use crate::args::parse_headers;
use crate::cli::{ChainSetArgs, usage_error};
use crate::load::open;
use crate::probe::check;
use crate::prompt::{self, Ask, StdinTrim, Tty, read_stdin_secret};
use crate::select::{Missing, pick_chain};
use crate::view;
use anyhow::Result;
use chainz_core::model::{Config, Explorer};
use chainz_core::resolve::{self, Policy};
use chainz_core::tx::{ChainEdit, EditReport};
use zeroize::Zeroizing;

/// `chain set`, or `update` (`command` is how it was invoked:
/// `["chain", "set"]` or `["update"]`).
pub fn set(args: &ChainSetArgs, command: &[&str]) -> Result<i32> {
    let mut chainz = open()?;
    // Usage and the chain first: stdin is read only for a valid command.
    let mut flags = build_edit(args, parse_headers(&args.headers)?, None);
    let menu_wanted = flags.is_empty() && !args.api_key_stdin && args.api_key.is_none();
    if !Tty.is_interactive() && (args.chain.is_none() || menu_wanted) {
        return Err(usage_error(
            command,
            "off a terminal, name a chain and at least one change (on a terminal, no flags open a menu)",
        ));
    }
    let name = match &args.chain {
        Some(query) => resolve::configured(chainz.config(), query, Policy::Prefix)?
            .0
            .to_string(),
        None => pick_chain(
            &chainz,
            &mut Tty,
            "Chain to change",
            &Missing {
                command,
                off_tty: "pass the chain to change",
            },
        )?,
    };
    flags.api_key = api_key(args)?;
    let edit = if menu_wanted {
        menu(&mut Tty, chainz.config(), &name)?
    } else {
        flags
    };
    if edit.is_empty() {
        println!("No changes");
        return Ok(0);
    }
    if !args.no_check {
        check_new_url(&chainz, &name, &edit)?;
    }
    let report = chainz.transact(|tx| tx.edit_chain(&name, &edit))?;
    print_report(&report);
    Ok(0)
}

/// The API key the flags give: read from stdin for `--api-key-stdin`.
fn api_key(args: &ChainSetArgs) -> Result<Option<Zeroizing<String>>> {
    Ok(if args.api_key_stdin {
        Some(read_stdin_secret("API key", StdinTrim::Whitespace)?)
    } else if let Some(key) = &args.api_key {
        prompt::warn_secret_argument("an API key", "--api-key-stdin");
        Some(Zeroizing::new(key.clone()))
    } else {
        None
    })
}

/// The patch the flags ask for (empty when there are none), once the
/// header and key values are read.
fn build_edit(
    args: &ChainSetArgs,
    headers: Vec<(String, String)>,
    api_key: Option<Zeroizing<String>>,
) -> ChainEdit {
    let either = |set: &Option<String>, clear: bool| match (set, clear) {
        (Some(value), _) => Some(Some(value.clone())),
        (None, true) => Some(None),
        (None, false) => None,
    };
    ChainEdit {
        signer: either(&args.signer, args.no_signer),
        rpc: args.rpc.clone(),
        headers,
        clear_headers: args.clear_headers,
        failover: match (args.failover.is_empty(), args.no_failover) {
            (false, _) => Some(args.failover.clone()),
            (true, true) => Some(Vec::new()),
            (true, false) => None,
        },
        explorer: either(&args.explorer, args.no_explorer),
        api_key,
        name: args.name.clone(),
        add_aliases: args.aliases.clone(),
        remove_aliases: args.rm_aliases.clone(),
    }
}

/// The endpoint a `--rpc` URL adds must answer with the chain's ID (with
/// the headers the edit gives it).
fn check_new_url(chainz: &chainz_core::Chainz, name: &str, edit: &ChainEdit) -> Result<()> {
    let chain = &chainz.config().chains[name];
    match edit.new_endpoint(name, chain)? {
        Some((ep, endpoint)) if !endpoint.is_websocket() => {
            check(chainz, name, chain.id, &ep, &endpoint)
        }
        _ => Ok(()),
    }
}

fn print_report(report: &EditReport) {
    println!("Updated chain '{}'", report.chain);
    if let Some(explorer) = &report.created_explorer {
        println!("  created [explorers.{explorer}]");
    }
    if let Some(endpoint) = &report.added_endpoint {
        println!("  added endpoint '{endpoint}'");
    }
    for change in &report.changes {
        println!("  {change}");
    }
    crate::ui::report_moved(&report.moved);
}

// ----------------------------------------------------------------- menu

/// The menu's entries, in order.
const MENU: [&str; 6] = ["RPC", "Signer", "Explorer", "Name", "Aliases", "Done"];

/// The v1-style menu over chain `name`: each pick edits one field of the
/// patch, and `Done` returns it (nothing is written until then).
pub(super) fn menu(ask: &mut dyn Ask, config: &Config, name: &str) -> Result<ChainEdit> {
    let chain = &config.chains[name];
    let mut edit = ChainEdit::default();
    loop {
        let shown = Shown::of(config, name, &edit);
        let items: Vec<String> = MENU
            .iter()
            .zip([
                shown.rpc.clone(),
                shown.signer.clone(),
                shown.explorer.clone(),
                shown.name.clone(),
                shown.aliases.clone(),
            ])
            .map(|(entry, now)| format!("{entry:<9} {now}"))
            .chain(std::iter::once(MENU[5].to_string()))
            .collect();
        match ask.select(&format!("Change {name}"), &items, 0)? {
            0 => {
                let mut options: Vec<String> = chain
                    .rpcs
                    .iter()
                    .map(|(ep, e)| {
                        format!(
                            "{} {ep}  {}",
                            view::marker(ep == &chain.rpc, chain.failover.contains(ep)),
                            chainz_core::redact::url(&e.url, false)
                        )
                    })
                    .collect();
                options.push("Enter a URL…".to_string());
                let pick = ask.select("Active RPC", &options, 0)?;
                edit.rpc = match chain.rpcs.get_index(pick) {
                    Some((ep, _)) if *ep == chain.rpc => None,
                    Some((ep, _)) => Some(ep.clone()),
                    None => match ask.text("RPC URL", None)?.trim() {
                        "" => edit.rpc.take(),
                        url => Some(url.to_string()),
                    },
                };
            }
            1 => {
                let mut options: Vec<String> = config
                    .signers
                    .iter()
                    .map(|(n, s)| match s.address {
                        Some(address) => format!("{n}  {address}"),
                        None => n.clone(),
                    })
                    .collect();
                options.push("No signer".to_string());
                let pick = ask.select("Signer", &options, 0)?;
                let signer = config.signers.get_index(pick).map(|(n, _)| n.clone());
                edit.signer = (signer != chain.signer).then_some(signer);
            }
            2 => {
                let mut names: Vec<String> = config.explorers.keys().cloned().collect();
                if !names.iter().any(|n| n == Explorer::ETHERSCAN) {
                    names.push(Explorer::ETHERSCAN.to_string());
                }
                let mut options = names.clone();
                options.push("No explorer".to_string());
                let pick = ask.select("Explorer", &options, 0)?;
                let explorer = names.get(pick).cloned();
                edit.api_key = None;
                if explorer.is_some()
                    && ask.confirm("Set its API key (stored in secrets.toml)?", false)?
                {
                    let key = ask.secret("API key")?;
                    edit.api_key = (!key.is_empty()).then_some(key);
                }
                edit.explorer =
                    (explorer != chain.explorer || edit.api_key.is_some()).then_some(explorer);
            }
            3 => {
                let new = ask.text("Chain name", Some(&shown.name))?;
                let new = new.trim();
                edit.name = (!new.is_empty() && new != name).then(|| new.to_string());
            }
            4 => {
                let current = shown.aliases_list(chain);
                let answer = ask.text("Aliases (comma-separated)", Some(&current.join(", ")))?;
                let wanted: Vec<String> = answer
                    .split(',')
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .collect();
                let has = |list: &[String], a: &str| list.iter().any(|x| x.eq_ignore_ascii_case(a));
                edit.add_aliases = wanted
                    .iter()
                    .filter(|a| !has(&chain.aliases, a))
                    .cloned()
                    .collect();
                edit.remove_aliases = chain
                    .aliases
                    .iter()
                    .filter(|a| !has(&wanted, a))
                    .cloned()
                    .collect();
            }
            _ => return Ok(edit),
        }
    }
}

/// What the menu shows for each field: the chain's value, or the pending
/// one.
struct Shown {
    rpc: String,
    signer: String,
    explorer: String,
    name: String,
    aliases: String,
    pending_aliases: Option<Vec<String>>,
}

impl Shown {
    fn of(config: &Config, name: &str, edit: &ChainEdit) -> Self {
        let chain = &config.chains[name];
        let or_dash = |v: Option<&str>| v.unwrap_or("-").to_string();
        let pending_aliases = (!edit.add_aliases.is_empty() || !edit.remove_aliases.is_empty())
            .then(|| {
                chain
                    .aliases
                    .iter()
                    .filter(|a| !edit.remove_aliases.contains(a))
                    .chain(&edit.add_aliases)
                    .cloned()
                    .collect::<Vec<_>>()
            });
        let aliases = pending_aliases.as_ref().unwrap_or(&chain.aliases);
        Shown {
            rpc: match edit.new_endpoint(name, chain) {
                Ok(Some((_, endpoint))) => chainz_core::redact::url(&endpoint.url, false),
                _ => match edit.rpc.as_deref() {
                    Some(rpc) if rpc.contains("://") => match chain.endpoint_at(rpc) {
                        Some(endpoint) => endpoint.to_string(),
                        None => chainz_core::redact::url(rpc, false),
                    },
                    Some(endpoint) => endpoint.to_string(),
                    None => chain.rpc.clone(),
                },
            },
            signer: match &edit.signer {
                Some(signer) => or_dash(signer.as_deref()),
                None => or_dash(chain.signer.as_deref()),
            },
            explorer: match &edit.explorer {
                Some(explorer) => or_dash(explorer.as_deref()),
                None => or_dash(chain.explorer.as_deref()),
            },
            name: edit.name.clone().unwrap_or_else(|| name.to_string()),
            aliases: if aliases.is_empty() {
                "-".to_string()
            } else {
                aliases.join(", ")
            },
            pending_aliases,
        }
    }

    fn aliases_list(&self, chain: &chainz_core::model::Chain) -> Vec<String> {
        self.pending_aliases
            .clone()
            .unwrap_or_else(|| chain.aliases.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted answers, in order; running out is a test failure.
    #[derive(Default)]
    struct Script {
        answers: VecDeque<Answer>,
        asked: Vec<String>,
    }

    enum Answer {
        Pick(usize),
        Text(&'static str),
        Yes(bool),
    }

    impl Script {
        fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
            Script {
                answers: answers.into_iter().collect(),
                asked: Vec::new(),
            }
        }

        fn next(&mut self, message: &str) -> Answer {
            self.asked.push(message.to_string());
            self.answers
                .pop_front()
                .unwrap_or_else(|| panic!("no scripted answer for '{message}'"))
        }
    }

    impl Ask for Script {
        fn is_interactive(&self) -> bool {
            true
        }
        fn multi_select(&mut self, message: &str, _: &[String], _: &[bool]) -> Result<Vec<usize>> {
            panic!("'{message}' is not scripted")
        }
        fn select(&mut self, message: &str, items: &[String], _: usize) -> Result<usize> {
            match self.next(message) {
                Answer::Pick(i) => {
                    assert!(i < items.len(), "'{message}' has no item {i}: {items:?}");
                    Ok(i)
                }
                _ => panic!("'{message}' is a select"),
            }
        }
        fn text(&mut self, message: &str, _: Option<&str>) -> Result<String> {
            match self.next(message) {
                Answer::Text(t) => Ok(t.to_string()),
                _ => panic!("'{message}' is a text"),
            }
        }
        fn secret(&mut self, message: &str) -> Result<Zeroizing<String>> {
            match self.next(message) {
                Answer::Text(t) => Ok(Zeroizing::new(t.to_string())),
                _ => panic!("'{message}' is a secret"),
            }
        }
        fn confirm(&mut self, message: &str, _: bool) -> Result<bool> {
            match self.next(message) {
                Answer::Yes(yes) => Ok(yes),
                _ => panic!("'{message}' is a confirm"),
            }
        }
    }

    fn config() -> Config {
        Config::parse(
            r#"
version = 2
[chains.eth]
id = 1
aliases = ["mainnet"]
signer = "deployer"
rpc = "llama"
[chains.eth.rpcs]
llama = "https://eth.llamarpc.com"
infura = "https://mainnet.infura.io/v3/${INFURA_KEY}"
[signers.deployer]
backend = "keystore"
path = "/k"
[signers.ops]
backend = "keystore"
path = "/o"
"#,
        )
        .unwrap()
    }

    use Answer::{Pick, Text, Yes};

    #[test]
    fn done_right_away_changes_nothing() {
        let edit = menu(&mut Script::new([Pick(5)]), &config(), "eth").unwrap();
        assert!(edit.is_empty());
    }

    #[test]
    fn the_menu_fills_one_patch() {
        let mut script = Script::new([
            Pick(0),          // RPC
            Pick(1),          // infura
            Pick(1),          // Signer
            Pick(1),          // ops
            Pick(2),          // Explorer
            Pick(0),          // etherscan (built in)
            Yes(true),        // set its key
            Text("KEY123"),   // the key
            Pick(3),          // Name
            Text("ethereum"), // new name
            Pick(4),          // Aliases
            Text("main, l1"), // mainnet dropped, two added
            Pick(5),          // Done
        ]);
        let edit = menu(&mut script, &config(), "eth").unwrap();
        assert_eq!(edit.rpc.as_deref(), Some("infura"));
        assert_eq!(edit.signer, Some(Some("ops".to_string())));
        assert_eq!(edit.explorer, Some(Some("etherscan".to_string())));
        assert_eq!(edit.api_key.as_deref().map(|k| k.as_str()), Some("KEY123"));
        assert_eq!(edit.name.as_deref(), Some("ethereum"));
        assert_eq!(edit.add_aliases, ["main", "l1"]);
        assert_eq!(edit.remove_aliases, ["mainnet"]);
        assert_eq!(script.asked.len(), 13);
    }

    #[test]
    fn picking_the_current_values_changes_nothing() {
        let mut script = Script::new([
            Pick(0),
            Pick(0), // llama, already active
            Pick(1),
            Pick(0), // deployer, already attached
            Pick(3),
            Text("eth"),
            Pick(4),
            Text("mainnet"),
            Pick(5),
        ]);
        assert!(menu(&mut script, &config(), "eth").unwrap().is_empty());
    }

    #[test]
    fn a_url_and_no_signer() {
        let mut script = Script::new([
            Pick(0),
            Pick(2), // Enter a URL…
            Text("https://rpc.example.org"),
            Pick(1),
            Pick(2), // No signer
            Pick(5),
        ]);
        let edit = menu(&mut script, &config(), "eth").unwrap();
        assert_eq!(edit.rpc.as_deref(), Some("https://rpc.example.org"));
        assert_eq!(edit.signer, Some(None));
    }

    #[test]
    fn flags_build_the_same_patch() {
        let args = ChainSetArgs {
            chain: Some("eth".into()),
            signer: Some("ops".into()),
            rpc: Some("infura".into()),
            failover: vec!["llama".into()],
            no_explorer: true,
            name: Some("ethereum".into()),
            aliases: vec!["l1".into()],
            rm_aliases: vec!["mainnet".into()],
            clear_headers: true,
            ..ChainSetArgs::default()
        };
        let headers = vec![("x-team".to_string(), "${TEAM}".to_string())];
        let edit = build_edit(&args, headers.clone(), None);
        assert_eq!(edit.signer, Some(Some("ops".to_string())));
        assert_eq!(edit.rpc.as_deref(), Some("infura"));
        assert_eq!(edit.failover, Some(vec!["llama".to_string()]));
        assert_eq!(edit.explorer, Some(None));
        assert_eq!(edit.headers, headers);
        assert!(edit.clear_headers);
        assert_eq!(edit.name.as_deref(), Some("ethereum"));
        assert_eq!(edit.add_aliases, ["l1"]);
        assert_eq!(edit.remove_aliases, ["mainnet"]);

        let none = build_edit(&ChainSetArgs::default(), Vec::new(), None);
        assert!(none.is_empty());
        let clear = ChainSetArgs {
            no_signer: true,
            no_failover: true,
            ..ChainSetArgs::default()
        };
        let edit = build_edit(&clear, Vec::new(), None);
        assert_eq!(edit.signer, Some(None));
        assert_eq!(edit.failover, Some(Vec::new()));
    }
}
