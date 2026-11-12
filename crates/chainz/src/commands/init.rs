//! `init` and `chainz` with no command: first-run setup.
//!
//! `init` on a terminal composes the other commands' flows: chains (the
//! `chain add` flow), a signer (the `signer add` flow, which attaches it to
//! those chains), an Etherscan key (`chain set --explorer etherscan
//! --api-key`), and a default chain. Off a terminal it prints the quick
//! start.

use super::{chain, signer};
use crate::cli::{ChainAddArgs, Opt, SignerAddArgs};
use crate::load::open;
use crate::prompt::{Ask, Tty, is_tty};
use crate::{ui, view};
use anyhow::Result;
use chainz_core::Chainz;
use chainz_core::model::Config;
use chainz_core::signer::SystemKeyBackend;
use chainz_core::tx::ChainEdit;
use clap::CommandFactory;
use zeroize::Zeroizing;

/// The built-in explorer `init` attaches.
const ETHERSCAN: &str = "etherscan";

/// `chainz` with no command: the quick start until a chain exists, then
/// the help.
pub fn no_command() -> Result<i32> {
    let chainz = open()?;
    if chainz.config().chains.is_empty() {
        println!("chainz: no chains configured yet.");
        print!("{}", view::QUICK_START);
    } else {
        Opt::command().print_help()?;
    }
    Ok(0)
}

/// `chainz init`.
pub fn run() -> Result<i32> {
    if !is_tty() {
        print!("{}", view::QUICK_START);
        return Ok(0);
    }
    let mut chainz = open()?;
    let ask = &mut Tty;
    println!("{}", ui::section("Chains"));
    add_chains(&mut chainz, ask)?;
    println!("{}", ui::section("Signer"));
    add_signer(&mut chainz, ask)?;
    println!("{}", ui::section("Explorer"));
    if let Some(key) = etherscan_key(chainz.config(), ask)? {
        set_etherscan(&mut chainz, key)?;
    }
    println!("{}", ui::section("Default chain"));
    if let Some(default) = pick_default(chainz.config(), ask)? {
        chainz.transact(|tx| {
            tx.set_default(Some(&default));
            Ok(())
        })?;
        println!("Default chain set to '{default}'");
    }
    println!();
    println!(
        "{}",
        ui::success("Done. Try `chainz ls`, or `chainz exec -- cast block-number`.")
    );
    Ok(0)
}

/// Add chains from chainlist until the user is done. A chain that fails
/// to add is reported and the loop goes on.
fn add_chains(chainz: &mut Chainz, ask: &mut dyn Ask) -> Result<()> {
    let have = chainz.config().chains.len();
    if have > 0 {
        println!("{have} chain(s) configured");
    }
    let mut more = ask.confirm("Add a chain from chainlist?", have == 0)?;
    while more {
        if let Err(e) = chain::add(chainz, &ChainAddArgs::default(), ask) {
            if ui::is_cancelled(&e) {
                return Err(e);
            }
            eprintln!("{}", ui::fail(&format!("{e:#}")));
        }
        more = ask.confirm("Add another chain?", false)?;
    }
    Ok(())
}

/// Add a first signer (attached to every chain without one), unless one
/// exists or the user skips it.
fn add_signer(chainz: &mut Chainz, ask: &mut dyn Ask) -> Result<()> {
    if !chainz.config().signers.is_empty() {
        let names: Vec<&str> = chainz.config().signers.keys().map(String::as_str).collect();
        println!("Signers: {}", names.join(", "));
        return Ok(());
    }
    if !ask.confirm("Add a signer (a private key in the OS keyring)?", true)? {
        println!(
            "{}",
            ui::dim("skipped; later: `chainz signer add <name>` (keystore and 1Password work too)")
        );
        return Ok(());
    }
    let name = ask.text("Signer name", Some("deployer"))?;
    let args = SignerAddArgs {
        name: name.trim().to_string(),
        ..SignerAddArgs::default()
    };
    signer::add(chainz, &SystemKeyBackend, &args).map(drop)
}

/// The Etherscan API key to store, if the user gives one. Asked only when
/// some chain has no explorer.
fn etherscan_key(config: &Config, ask: &mut dyn Ask) -> Result<Option<Zeroizing<String>>> {
    if !config.chains.values().any(|c| c.explorer.is_none()) {
        println!("Every chain has an explorer");
        return Ok(None);
    }
    if !ask.confirm(
        "Add an Etherscan API key (one key verifies on every Etherscan chain)?",
        false,
    )? {
        return Ok(None);
    }
    let key = ask.secret("Etherscan API key")?;
    Ok((!key.trim().is_empty()).then(|| Zeroizing::new(key.trim().to_string())))
}

/// The patches that attach `etherscan` (with `key`) to every chain that
/// has no explorer. The key is stored once, with the first.
fn etherscan_edits(config: &Config, key: Zeroizing<String>) -> Vec<(String, ChainEdit)> {
    let mut key = Some(key);
    config
        .chains
        .iter()
        .filter(|(_, c)| c.explorer.is_none())
        .map(|(name, _)| {
            let edit = ChainEdit {
                explorer: Some(Some(ETHERSCAN.to_string())),
                api_key: key.take(),
                ..ChainEdit::default()
            };
            (name.clone(), edit)
        })
        .collect()
}

fn set_etherscan(chainz: &mut Chainz, key: Zeroizing<String>) -> Result<()> {
    let edits = etherscan_edits(chainz.config(), key);
    let reports = chainz.transact(|tx| {
        edits
            .iter()
            .map(|(name, edit)| tx.edit_chain(name, edit))
            .collect::<Result<Vec<_>>>()
    })?;
    let chains: Vec<&str> = reports.iter().map(|r| r.chain.as_str()).collect();
    println!("Attached etherscan to {}", chains.join(", "));
    if let Some(var) = reports.iter().find_map(|r| r.api_key_var.as_deref()) {
        println!("  the key is in ${{{var}}} (secrets.toml)");
    }
    Ok(())
}

/// The default chain to set: none when one is set already or there are no
/// chains; with one chain, that chain.
fn pick_default(config: &Config, ask: &mut dyn Ask) -> Result<Option<String>> {
    if let Some(default) = &config.default {
        println!("Default chain: {default}");
        return Ok(None);
    }
    let names: Vec<String> = config.chains.keys().cloned().collect();
    match names.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(only.clone())),
        _ => {
            let mut items = names.clone();
            items.push("No default".to_string());
            let pick = ask.select("Default chain", &items, 0)?;
            Ok(names.get(pick).cloned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    /// Answers every question from a list, in order.
    struct Script(Vec<&'static str>);

    impl Script {
        fn next(&mut self) -> &'static str {
            assert!(!self.0.is_empty(), "out of scripted answers");
            self.0.remove(0)
        }
    }

    impl Ask for Script {
        fn select(&mut self, _: &str, _: &[String], _: usize) -> Result<usize> {
            Ok(self.next().parse()?)
        }
        fn text(&mut self, _: &str, _: Option<&str>) -> Result<String> {
            Ok(self.next().to_string())
        }
        fn secret(&mut self, _: &str) -> Result<Zeroizing<String>> {
            Ok(Zeroizing::new(self.next().to_string()))
        }
        fn confirm(&mut self, _: &str, _: bool) -> Result<bool> {
            match self.next() {
                "y" => Ok(true),
                "n" => Ok(false),
                other => bail!("not y/n: {other}"),
            }
        }
    }

    fn config(extra: &str) -> Config {
        Config::parse(&format!(
            r#"
version = 2
{extra}
[chains.eth]
id = 1
rpc = "a"
rpcs = {{ a = "https://eth.example" }}
[chains.base]
id = 8453
explorer = "blockscout"
rpc = "a"
rpcs = {{ a = "https://base.example" }}
[chains.op]
id = 10
rpc = "a"
rpcs = {{ a = "https://op.example" }}
[explorers.blockscout]
api = "https://blockscout.example/api"
"#
        ))
        .unwrap()
    }

    #[test]
    fn etherscan_goes_on_chains_without_an_explorer_with_one_key() {
        let config = config("");
        let mut script = Script(vec!["y", " KEY "]);
        let key = etherscan_key(&config, &mut script).unwrap().unwrap();
        assert_eq!(key.as_str(), "KEY");
        let edits = etherscan_edits(&config, key);
        let names: Vec<&str> = edits.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["eth", "op"]);
        assert!(edits[0].1.api_key.is_some());
        assert!(edits[1].1.api_key.is_none());
        for (_, edit) in &edits {
            assert_eq!(edit.explorer, Some(Some(ETHERSCAN.to_string())));
        }
        // Declined, or an empty key: nothing.
        assert!(
            etherscan_key(&config, &mut Script(vec!["n"]))
                .unwrap()
                .is_none()
        );
        assert!(
            etherscan_key(&config, &mut Script(vec!["y", "  "]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn default_chain_is_asked_only_when_there_is_a_choice() {
        let mut script = Script(vec!["2"]);
        assert_eq!(
            pick_default(&config(""), &mut script).unwrap().as_deref(),
            Some("op")
        );
        assert_eq!(
            pick_default(&config(""), &mut Script(vec!["3"])).unwrap(),
            None
        );
        assert_eq!(
            pick_default(&config("default = \"base\""), &mut Script(vec![])).unwrap(),
            None
        );
    }

    /// The README's quick start is this one.
    #[test]
    fn readme_has_the_quick_start() {
        let readme = include_str!("../../../../README.md");
        assert!(
            readme.contains(view::QUICK_START),
            "README.md must contain view::QUICK_START verbatim"
        );
    }
}
