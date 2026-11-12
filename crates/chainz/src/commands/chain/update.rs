//! `update [CHAIN]`: one patch to one chain, from flags or, with none on a
//! TTY, from the v1-style menu. Either way it is one
//! `ChainEdit` applied in one transaction.

use crate::args::parse_headers;
use crate::cli::{UpdateArgs, usage_error};
use crate::load::open;
use crate::probe::check;
use crate::prompt::{self, Ask, Esc, StdinTrim, Tty, read_stdin_secret};
use crate::select::{Missing, pick_chain};
use crate::{ui, view};
use anyhow::Result;
use chainz_core::model::{Config, Explorer, RpcChoice};
use chainz_core::resolve::{self, Policy};
use chainz_core::secrets::Vars;
use chainz_core::tx::{ChainEdit, EditReport};
use zeroize::Zeroizing;

/// `update`.
pub fn update(args: &UpdateArgs) -> Result<i32> {
    let command: &[&str] = &["update"];
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
        menu(&mut Tty, chainz.config(), &chainz.secrets()?.vars, &name)?
    } else {
        flags
    };
    if edit.is_empty() {
        println!("No changes");
        return Ok(0);
    }
    if !args.no_check {
        check_new_urls(&chainz, &name, &edit)?;
    }
    let report = chainz.transact(|tx| tx.edit_chain(&name, &edit))?;
    print_report(&report);
    Ok(0)
}

/// The API key the flags give: read from stdin for `--api-key-stdin`.
fn api_key(args: &UpdateArgs) -> Result<Option<Zeroizing<String>>> {
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
    args: &UpdateArgs,
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
        add_rpcs: args
            .add_rpcs
            .iter()
            .map(|arg| ChainEdit::parse_add_rpc(arg))
            .collect(),
        remove_rpcs: args.rm_rpcs.clone(),
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

/// Every endpoint the edit adds (a `--rpc` URL, `--add-rpc`) must answer
/// with the chain's ID (with the headers the edit gives it); websockets
/// are not checked.
fn check_new_urls(chainz: &chainz_core::Chainz, name: &str, edit: &ChainEdit) -> Result<()> {
    let chain = &chainz.config().chains[name];
    for (ep, endpoint, _) in edit.added_endpoints(name, chain, &chainz.secrets()?.vars)? {
        if !endpoint.is_websocket() {
            check(chainz, name, chain.id, &ep, &endpoint)?;
        }
    }
    Ok(())
}

fn print_report(report: &EditReport) {
    if report.is_empty() {
        println!("No changes ('{}' already has these settings)", report.chain);
        return;
    }
    println!("Updated chain '{}'", report.chain);
    if let Some(explorer) = &report.created_explorer {
        println!("  created [explorers.{explorer}]");
    }
    for change in &report.changes {
        println!("  {change}");
    }
    crate::ui::report_moved(&report.moved);
    for warning in &report.warnings {
        eprintln!("{}", crate::ui::warn(warning));
    }
}

// ----------------------------------------------------------------- menu

/// The menu's entries, in order.
const MENU: [&str; 6] = ["RPC", "Signer", "Explorer", "Name", "Aliases", "Done"];

/// The index of `Done` in [`MENU`].
const DONE: usize = MENU.len() - 1;

/// The v1-style menu over chain `name`: each pick edits one field of the
/// patch, and `Done` returns it (nothing is written until then).
///
/// Every picker starts on the field's value (pending, else current), so
/// Enter keeps it. Esc in a picker goes back to the menu with that field
/// unchanged; Esc in the menu leaves it, after asking when edits are
/// pending. After an edit the cursor is on `Done`.
/// `vars` (`secrets.toml`'s `[vars]`) match a typed URL to an endpoint.
pub(super) fn menu(
    ask: &mut dyn Ask,
    config: &Config,
    vars: &Vars,
    name: &str,
) -> Result<ChainEdit> {
    let chain = &config.chains[name];
    let mut edit = ChainEdit::default();
    let mut cursor = 0;
    loop {
        let shown = Shown::of(config, vars, name, &edit);
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
            .chain(std::iter::once(MENU[DONE].to_string()))
            .collect();
        let Some(pick) = ask.choose(&format!("Change {name}"), &items, cursor, Esc::Exit)? else {
            let pending = pending_fields(&edit);
            if pending.is_empty()
                || ask.confirm(
                    &format!(
                        "Discard {} pending change{} ({})?",
                        pending.len(),
                        if pending.len() == 1 { "" } else { "s" },
                        pending.join(", ")
                    ),
                    false,
                )?
            {
                return Err(ui::cancelled());
            }
            continue;
        };
        if pick == DONE {
            return Ok(edit);
        }
        let before = edit.clone();
        match pick {
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
                let current = match edit.rpc_choice(name, chain, vars) {
                    Ok(Some(RpcChoice::Existing(ep))) => chain.rpcs.get_index_of(&ep),
                    Ok(Some(RpcChoice::New(..))) => Some(chain.rpcs.len()),
                    _ => chain.rpcs.get_index_of(&chain.rpc),
                };
                let Some(pick) =
                    ask.choose("Active RPC", &options, current.unwrap_or(0), Esc::Back)?
                else {
                    cursor = pick;
                    continue;
                };
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
                let current = edit.signer.as_ref().unwrap_or(&chain.signer);
                let current = current
                    .as_ref()
                    .and_then(|s| config.signers.get_index_of(s))
                    .unwrap_or(config.signers.len());
                let Some(pick) = ask.choose("Signer", &options, current, Esc::Back)? else {
                    cursor = pick;
                    continue;
                };
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
                let current = edit.explorer.as_ref().unwrap_or(&chain.explorer);
                let current = current
                    .as_ref()
                    .and_then(|e| names.iter().position(|n| n == e))
                    .unwrap_or(names.len());
                let Some(pick) = ask.choose("Explorer", &options, current, Esc::Back)? else {
                    cursor = pick;
                    continue;
                };
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
            _ => {
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
        }
        cursor = if edit == before { pick } else { DONE };
    }
}

/// The menu fields `edit` changes, for "discard N pending changes (…)?".
fn pending_fields(edit: &ChainEdit) -> Vec<&'static str> {
    [
        ("RPC", edit.rpc.is_some()),
        ("signer", edit.signer.is_some()),
        (
            "explorer",
            edit.explorer.is_some() || edit.api_key.is_some(),
        ),
        ("name", edit.name.is_some()),
        (
            "aliases",
            !edit.add_aliases.is_empty() || !edit.remove_aliases.is_empty(),
        ),
    ]
    .into_iter()
    .filter_map(|(field, pending)| pending.then_some(field))
    .collect()
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
    fn of(config: &Config, vars: &Vars, name: &str, edit: &ChainEdit) -> Self {
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
            rpc: match edit.rpc_choice(name, chain, vars) {
                Ok(None) => chain.rpc.clone(),
                Ok(Some(RpcChoice::Existing(endpoint))) => endpoint,
                Ok(Some(RpcChoice::New(_, endpoint))) => {
                    chainz_core::redact::url(&endpoint.url, false)
                }
                // Neither an endpoint nor a usable URL (`Done` reports
                // why): shown redacted, as it may hold a key.
                Err(_) => chainz_core::redact::url(edit.rpc.as_deref().unwrap_or_default(), false),
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
    use crate::prompt::script::{Answer, Script};

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
        let edit = menu(&mut Script::new([Pick(5)]), &config(), &Vars::new(), "eth").unwrap();
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
        let edit = menu(&mut script, &config(), &Vars::new(), "eth").unwrap();
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
        assert!(
            menu(&mut script, &config(), &Vars::new(), "eth")
                .unwrap()
                .is_empty()
        );
    }

    /// The picker defaults are the current values: Enter in each changes
    /// nothing (it used to start on the first item, so Enter switched).
    #[test]
    fn enter_in_every_picker_keeps_the_current_value() {
        let config = Config::parse(
            r#"
version = 2
[chains.eth]
id = 1
signer = "ops"
explorer = "blockscout"
rpc = "infura"
[chains.eth.rpcs]
llama = "https://eth.llamarpc.com"
infura = "https://mainnet.infura.io/v3/${INFURA_KEY}"
[explorers.blockscout]
api = "https://blockscout.example/api"
[signers.deployer]
backend = "keystore"
path = "/k"
[signers.ops]
backend = "keystore"
path = "/o"
"#,
        )
        .unwrap();
        let mut script = Script::new([
            Pick(0),
            Answer::Enter, // Active RPC
            Pick(1),
            Answer::Enter, // Signer
            Pick(2),
            Answer::Enter, // Explorer
            Yes(false),    // no API key
            Pick(5),
        ]);
        assert!(
            menu(&mut script, &config, &Vars::new(), "eth")
                .unwrap()
                .is_empty()
        );
        // Menu, RPC (infura), menu, Signer (ops), menu, Explorer
        // (blockscout), menu.
        assert_eq!(script.defaults, [0, 1, 0, 1, 1, 0, 2]);
        // No signer, no explorer: the "No …" entries.
        let mut script = Script::new([Pick(1), Answer::Enter, Pick(2), Answer::Enter, Pick(5)]);
        let mut bare = config.clone();
        let eth = bare.chains.get_mut("eth").unwrap();
        eth.signer = None;
        eth.explorer = None;
        assert!(
            menu(&mut script, &bare, &Vars::new(), "eth")
                .unwrap()
                .is_empty()
        );
        // (etherscan is offered after blockscout.)
        assert_eq!(script.defaults, [0, 2, 1, 2, 2]);
    }

    #[test]
    fn esc_in_a_picker_goes_back_and_keeps_pending_edits() {
        let mut script = Script::new([
            Pick(3),
            Text("ethereum"), // Name
            Pick(1),
            Answer::Esc, // Signer: back
            Pick(0),
            Pick(1),       // infura
            Answer::Enter, // the cursor is on Done
        ]);
        let edit = menu(&mut script, &config(), &Vars::new(), "eth").unwrap();
        assert_eq!(edit.name.as_deref(), Some("ethereum"));
        assert_eq!(edit.rpc.as_deref(), Some("infura"));
        assert_eq!(edit.signer, None);
        // After the rename: Done; after Esc in Signer: Signer; after the
        // RPC pick: Done.
        assert_eq!(script.defaults, [0, 5, 0, 1, 0, 5]);
    }

    #[test]
    fn esc_in_the_menu_asks_before_discarding() {
        // Nothing pending: Esc leaves at once.
        let err = menu(
            &mut Script::new([Answer::Esc]),
            &config(),
            &Vars::new(),
            "eth",
        )
        .unwrap_err();
        assert!(ui::is_cancelled(&err));
        // Pending: asked; declining goes back to the menu.
        let mut script = Script::new([Pick(3), Text("ethereum"), Answer::Esc, Yes(false), Pick(5)]);
        let edit = menu(&mut script, &config(), &Vars::new(), "eth").unwrap();
        assert_eq!(edit.name.as_deref(), Some("ethereum"));
        assert_eq!(script.asked[3], "Discard 1 pending change (name)?");
        // Accepting discards them.
        let mut script = Script::new([Pick(3), Text("ethereum"), Answer::Esc, Yes(true)]);
        let err = menu(&mut script, &config(), &Vars::new(), "eth").unwrap_err();
        assert!(ui::is_cancelled(&err));
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
        let edit = menu(&mut script, &config(), &Vars::new(), "eth").unwrap();
        assert_eq!(edit.rpc.as_deref(), Some("https://rpc.example.org"));
        assert_eq!(edit.signer, Some(None));
    }

    #[test]
    fn flags_build_the_same_patch() {
        let args = UpdateArgs {
            chain: Some("eth".into()),
            signer: Some("ops".into()),
            rpc: Some("infura".into()),
            failover: vec!["llama".into()],
            no_explorer: true,
            name: Some("ethereum".into()),
            aliases: vec!["l1".into()],
            rm_aliases: vec!["mainnet".into()],
            clear_headers: true,
            ..UpdateArgs::default()
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

        let none = build_edit(&UpdateArgs::default(), Vec::new(), None);
        assert!(none.is_empty());
        let clear = UpdateArgs {
            no_signer: true,
            no_failover: true,
            ..UpdateArgs::default()
        };
        let edit = build_edit(&clear, Vec::new(), None);
        assert_eq!(edit.signer, Some(None));
        assert_eq!(edit.failover, Some(Vec::new()));
    }
}
