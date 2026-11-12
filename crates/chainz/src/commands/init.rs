//! `init` and `chainz` with no command: first-run setup.
//!
//! `init` on a terminal composes the other commands' flows: chains (the
//! `add` flow), a signer (the `signer add` flow, which attaches it to
//! those chains), an Etherscan key (`update --explorer etherscan
//! --api-key`), and a default chain. Off a terminal it prints the quick
//! start.

use super::{chain, signer};
use crate::cli::{AddArgs, SignerAddArgs};
use crate::load::open;
use crate::prompt::{Ask, AskPrompter, Tty};
use crate::{ui, view};
use anyhow::Result;
use chainz_core::Chainz;
use chainz_core::model::{Backend, Config, Explorer};
use chainz_core::signer::{KeyBackend, SystemKeyBackend};
use chainz_core::tx::MissingExplorers;
use zeroize::Zeroizing;

/// `chainz` with no command: the quick start until a chain exists, then
/// the help.
pub fn no_command() -> Result<i32> {
    let chainz = open()?;
    if chainz.config().chains.is_empty() {
        print!("{}", view::no_chains());
    } else {
        crate::cli::command().print_help()?;
    }
    Ok(0)
}

/// `chainz init`.
pub fn run() -> Result<i32> {
    let ask = &mut Tty;
    if !ask.is_interactive() {
        print!("{}", view::QUICK_START);
        return Ok(0);
    }
    let mut chainz = open()?;
    println!("{}", ui::section("Chains"));
    add_chains(&mut chainz, ask, &mut chain::add)?;
    println!("{}", ui::section("Signer"));
    add_signer(&mut chainz, &SystemKeyBackend, ask)?;
    if !chainz.config().chains.is_empty() {
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
    }
    println!();
    println!("{}", closing(chainz.config()));
    Ok(0)
}

/// The last line: what to try next, given what the wizard left.
fn closing(config: &Config) -> String {
    match (config.chains.keys().next(), &config.default) {
        (None, _) => ui::warn(
            "No chains yet: add one with `chainz add CHAIN` (an ID, or a name on chainlist), or run `chainz init` again.",
        ),
        (Some(_), Some(_)) => {
            ui::success("Done. Try `chainz ls`, or `chainz exec -- cast block-number`.")
        }
        (Some(first), None) => ui::success(&format!(
            "Done. Try `chainz ls`, or `chainz exec {first} -- cast block-number`."
        )),
    }
}

/// How `init` adds one chain: `chain::add` (tests pass their own).
type AddChain<'a> = dyn FnMut(&mut Chainz, &AddArgs, &mut dyn Ask) -> Result<i32> + 'a;

/// Add chains from chainlist until the user is done. A chain that fails
/// to add is reported and the loop goes on; when chainlist is
/// unreachable, the chain can be given by ID and RPC URL instead.
fn add_chains(chainz: &mut Chainz, ask: &mut dyn Ask, add: &mut AddChain<'_>) -> Result<()> {
    let have = chainz.config().chains.len();
    if have > 0 {
        println!("{} configured", ui::count(have, "chain"));
    }
    let mut more = ask.confirm("Add a chain from chainlist?", have == 0)?;
    while more {
        let mut result = add(chainz, &AddArgs::default(), ask);
        if let Err(e) = &result
            && e.downcast_ref::<chain::ChainlistUnavailable>().is_some()
        {
            eprintln!("{}", ui::fail(&format!("{e:#}")));
            result = match ask.confirm("Add it by chain ID and RPC URL instead?", true)? {
                true => add_offline(chainz, ask, add),
                false => Ok(0),
            };
        }
        if let Err(e) = result {
            if ui::is_cancelled(&e) {
                return Err(e);
            }
            eprintln!("{}", ui::fail(&format!("{e:#}")));
        }
        more = ask.confirm("Add another chain?", false)?;
    }
    Ok(())
}

/// `add ID --url URL`, asked for.
fn add_offline(chainz: &mut Chainz, ask: &mut dyn Ask, add: &mut AddChain<'_>) -> Result<i32> {
    let id = ask.text("Chain ID", None)?;
    let url = ask.text("RPC URL", None)?;
    let args = AddArgs {
        chain: Some(id.trim().to_string()),
        urls: vec![url.trim().to_string()],
        ..AddArgs::default()
    };
    add(chainz, &args, ask)
}

/// Add a first signer (attached to every chain without one), unless one
/// exists or the user skips it. A key that doesn't work (a typo, a
/// password mismatch) is reported and asked again.
fn add_signer(chainz: &mut Chainz, backend: &dyn KeyBackend, ask: &mut dyn Ask) -> Result<()> {
    if !chainz.config().signers.is_empty() {
        let names: Vec<&str> = chainz.config().signers.keys().map(String::as_str).collect();
        println!("Signers: {}", names.join(", "));
        return Ok(());
    }
    let storage = match chainz_core::signer::safe_default(backend) {
        Backend::Keyring => "in the OS keyring",
        _ => "encrypted in secrets.toml",
    };
    if !ask.confirm(&format!("Add a signer (a private key, {storage})?"), true)? {
        println!(
            "{}",
            ui::dim("skipped; later: `chainz signer add NAME` (keystore and 1Password work too)")
        );
        return Ok(());
    }
    let mut name = "deployer".to_string();
    loop {
        name = ask.text("Signer name", Some(&name))?.trim().to_string();
        let args = SignerAddArgs {
            name: name.clone(),
            ..SignerAddArgs::default()
        };
        match signer::add(chainz, backend, &AskPrompter::new(ask), &args) {
            Ok(_) => return Ok(()),
            Err(e) if ui::is_cancelled(&e) => return Err(e),
            Err(e) => {
                eprintln!("{}", ui::fail(&format!("{e:#}")));
                if !ask.confirm("Try again?", true)? {
                    return Ok(());
                }
            }
        }
    }
}

/// The Etherscan API key to store, if the user gives one. Asked only when
/// some chain Etherscan covers has no explorer.
fn etherscan_key(config: &Config, ask: &mut dyn Ask) -> Result<Option<Zeroizing<String>>> {
    let MissingExplorers {
        covered,
        skipped: not,
    } = MissingExplorers::of(config);
    if !not.is_empty() {
        println!(
            "{}",
            ui::dim(&format!(
                "{} doesn't cover {}; skipped",
                Explorer::ETHERSCAN,
                not.join(", ")
            ))
        );
    }
    if covered.is_empty() {
        if not.is_empty() {
            return Ok(None);
        }
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

fn set_etherscan(chainz: &mut Chainz, key: Zeroizing<String>) -> Result<()> {
    let attached = chainz.transact(|tx| tx.attach_explorer_where_missing(key))?;
    println!(
        "Attached etherscan to {}",
        attached.chains.covered.join(", ")
    );
    if let Some(var) = &attached.api_key_var {
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

    use crate::prompt::script::{Answer::*, Script};

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
[chains.local]
id = 31337
rpc = "a"
rpcs = {{ a = "http://127.0.0.1:8545" }}
[explorers.blockscout]
api = "https://blockscout.example/api"
"#
        ))
        .unwrap()
    }

    #[test]
    fn the_etherscan_key_is_asked_once_and_trimmed() {
        let config = config("");
        let mut script = Script::new([Yes(true), Text(" KEY ")]);
        let key = etherscan_key(&config, &mut script).unwrap().unwrap();
        assert_eq!(key.as_str(), "KEY");
        // Declined, or an empty key: nothing.
        assert!(
            etherscan_key(&config, &mut Script::new([Yes(false)]))
                .unwrap()
                .is_none()
        );
        assert!(
            etherscan_key(&config, &mut Script::new([Yes(true), Text("  ")]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn default_chain_is_asked_only_when_there_is_a_choice() {
        let mut script = Script::new([Pick(2)]);
        assert_eq!(
            pick_default(&config(""), &mut script).unwrap().as_deref(),
            Some("op")
        );
        assert_eq!(
            pick_default(&config(""), &mut Script::new([Pick(4)])).unwrap(),
            None
        );
        assert_eq!(
            pick_default(&config("default = \"base\""), &mut Script::new([])).unwrap(),
            None
        );
    }

    /// No keyring: the safe default is an encrypted key.
    struct NoKeyring;

    impl KeyBackend for NoKeyring {
        fn keyring_available(&self) -> bool {
            false
        }
        fn keyring_get(&self, _: &str, _: &str) -> Result<Zeroizing<String>> {
            bail!("no keyring")
        }
        fn keyring_set(&self, _: &str, _: &str, _: &str) -> Result<bool> {
            bail!("no keyring")
        }
        fn keyring_delete(&self, _: &str, _: &str) -> Result<()> {
            bail!("no keyring")
        }
        fn one_password_get(&self, _: &str, _: &str) -> Result<Zeroizing<String>> {
            bail!("no op")
        }
    }

    #[test]
    fn the_signer_step_asks_everything_through_ask() {
        let dir = tempfile::tempdir().unwrap();
        let paths = chainz_core::StorePaths::in_dir(dir.path().join("chainz"));
        let mut chainz = Chainz::open_at(paths, chainz_core::LoadMode::Strict).unwrap();
        // Name, key, then the encryption password twice: no terminal.
        let key = "0x4c0883a69102937d6231471b5dbb6204fe512961708279f22a82e1d1b1c2f8f2";
        let mut script = Script::new([Yes(true), Text(" ops "), Text(key), Text("pw"), Text("pw")]);
        add_signer(&mut chainz, &NoKeyring, &mut script).unwrap();
        assert!(script.done(), "unasked answers left");
        assert_eq!(chainz.config().signers["ops"].backend, Backend::Encrypted);
        // Declined: nothing is asked after.
        let dir = tempfile::tempdir().unwrap();
        let paths = chainz_core::StorePaths::in_dir(dir.path().join("chainz"));
        let mut chainz = Chainz::open_at(paths, chainz_core::LoadMode::Strict).unwrap();
        add_signer(&mut chainz, &NoKeyring, &mut Script::new([Yes(false)])).unwrap();
        assert!(chainz.config().signers.is_empty());
    }

    #[test]
    fn a_bad_key_is_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let paths = chainz_core::StorePaths::in_dir(dir.path().join("chainz"));
        let mut chainz = Chainz::open_at(paths, chainz_core::LoadMode::Strict).unwrap();
        let key = "0x4c0883a69102937d6231471b5dbb6204fe512961708279f22a82e1d1b1c2f8f2";
        let mut script = Script::new([
            Yes(true),
            Text("ops"),
            Text("0xnotakey"),
            Yes(true), // try again
            Text("ops"),
            Text(key),
            Text("pw"),
            Text("pw"),
        ]);
        add_signer(&mut chainz, &NoKeyring, &mut script).unwrap();
        assert!(script.done());
        assert!(chainz.config().signers.contains_key("ops"));
        // Declining to retry leaves the wizard going, with no signer.
        let dir = tempfile::tempdir().unwrap();
        let paths = chainz_core::StorePaths::in_dir(dir.path().join("chainz"));
        let mut chainz = Chainz::open_at(paths, chainz_core::LoadMode::Strict).unwrap();
        let mut script = Script::new([Yes(true), Text("ops"), Text("bad"), Yes(false)]);
        add_signer(&mut chainz, &NoKeyring, &mut script).unwrap();
        assert!(chainz.config().signers.is_empty());
    }

    #[test]
    fn unreachable_chainlist_offers_the_chain_id_and_url() {
        let dir = tempfile::tempdir().unwrap();
        let paths = chainz_core::StorePaths::in_dir(dir.path().join("chainz"));
        let mut chainz = Chainz::open_at(paths, chainz_core::LoadMode::Strict).unwrap();
        let mut seen: Vec<(Option<String>, Vec<String>)> = Vec::new();
        let mut add = |_: &mut Chainz, args: &AddArgs, _: &mut dyn Ask| -> Result<i32> {
            seen.push((args.chain.clone(), args.urls.clone()));
            match args.chain {
                None => Err(anyhow::anyhow!("offline").context(chain::ChainlistUnavailable)),
                Some(_) => Ok(0),
            }
        };
        let mut script = Script::new([
            Yes(true), // add a chain
            Yes(true), // by ID and URL
            Text(" 8453 "),
            Text("http://127.0.0.1:8545"),
            Yes(false), // no more
        ]);
        add_chains(&mut chainz, &mut script, &mut add).unwrap();
        assert!(script.done());
        assert_eq!(
            seen,
            [
                (None, vec![]),
                (Some("8453".into()), vec!["http://127.0.0.1:8545".into()])
            ]
        );
    }

    #[test]
    fn the_closing_line_fits_what_was_set_up() {
        let none = Config::parse("version = 2\n").unwrap();
        let line = closing(&none);
        assert!(
            !line.contains("Done") && line.contains("chainz add"),
            "{line}"
        );
        let line = closing(&config(""));
        assert!(
            line.contains("Done") && line.contains("exec eth --"),
            "{line}"
        );
        let line = closing(&config("default = \"base\""));
        assert!(
            line.contains("`chainz exec -- cast block-number`"),
            "{line}"
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
