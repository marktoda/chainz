//! `signer …`: key references with cached addresses. Key material is only
//! read here (add, migrate) and by `exec --expose-key`.

use crate::args::parse_address;
use crate::cli::{SignerAddArgs, SignerCommand, SignerMigrateArgs, SignerRmArgs};
use crate::load::open;
use crate::prompt::{self, Consent, StdinTrim, Terminal, read_stdin_secret};
use crate::{ui, view};
use anyhow::{Context, Result, anyhow, bail};
use chainz_core::model::{Backend, Signer};
use chainz_core::signer::{self, KeyBackend, Prompter, Provision, SystemKeyBackend};
use chainz_core::tx::Users;
use chainz_core::{Address, Chainz, names, paths};
use serde::Serialize;
use zeroize::Zeroizing;

pub fn run(cmd: SignerCommand) -> Result<i32> {
    let backend = SystemKeyBackend;
    match cmd {
        SignerCommand::Add(args) => add(&mut open()?, &backend, &args),
        SignerCommand::Ls(args) => ls(args.json),
        SignerCommand::Rm(args) => rm(&backend, &args),
        SignerCommand::Migrate(args) => migrate(&backend, &args),
    }
}

fn check_name(chainz: &Chainz, name: &str) -> Result<()> {
    names::check_signer_name(name)?;
    if chainz.config().signers.contains_key(name) {
        bail!("signer '{name}' already exists; remove it first with `chainz signer rm {name}`");
    }
    Ok(())
}

fn read_key(
    terminal: &dyn Prompter,
    name: &str,
    stdin: bool,
    key: Option<String>,
) -> Result<Zeroizing<String>> {
    if let Some(key) = key {
        prompt::warn_secret_argument("a private key", "--stdin or the prompt");
        return Ok(Zeroizing::new(key));
    }
    if stdin {
        return read_stdin_secret("private key", StdinTrim::Whitespace);
    }
    if !terminal.is_interactive() {
        bail!("no key given: pipe it with --stdin");
    }
    terminal.secret(&format!("Private key for {name}: "))
}

/// `signer add`: store the key (or reference), then attach a first
/// signer to every chain that has none (unless `--no-attach`).
pub fn add(chainz: &mut Chainz, backend: &dyn KeyBackend, args: &SignerAddArgs) -> Result<i32> {
    let name = args.name.as_str();
    check_name(chainz, name)?;
    let first = chainz.config().signers.is_empty();
    let kind = args.backend.map(Backend::from).unwrap_or_else(|| {
        if args.path.is_some() {
            Backend::Keystore
        } else if args.vault.is_some() || args.item.is_some() {
            Backend::OnePassword
        } else {
            signer::safe_default(backend)
        }
    });
    let address = args
        .address
        .as_deref()
        .map(|a| parse_address(a, "--address"))
        .transpose()?;
    let provision = match kind {
        Backend::Keystore => {
            let path = args
                .path
                .clone()
                .ok_or_else(|| anyhow!("a keystore signer needs --path"))?;
            let derived = match signer::keystore_address(&paths::expand_tilde(&path)) {
                Ok(derived) => Some(derived),
                Err(e) => {
                    if address.is_none() {
                        eprintln!(
                            "{}",
                            ui::warn(&format!(
                                "no cached address for '{name}' ({e:#}); @wallet and ETH_FROM need one: pass --address"
                            ))
                        );
                    }
                    None
                }
            };
            let address = match (address, derived) {
                (Some(given), Some(derived)) if given != derived => {
                    bail!("--address does not match the keystore's address {derived}")
                }
                (given, derived) => given.or(derived),
            };
            Provision::reference(Signer::keystore(path, address))
        }
        Backend::OnePassword => {
            let (vault, item) = match (args.vault.clone(), args.item.clone()) {
                (Some(v), Some(i)) => (v, i),
                _ => bail!("a 1password signer needs --vault and --item"),
            };
            let address = match address {
                Some(a) => a,
                None => {
                    let key = backend.one_password_get(&vault, &item)?;
                    Address::from_private_key(&key)
                        .context("the 1Password item is not a private key")?
                }
            };
            Provision::reference(Signer::one_password(vault, item, Some(address)))
        }
        Backend::Keyring | Backend::Encrypted | Backend::Plaintext => {
            let key = read_key(&Terminal, name, args.stdin, args.key.clone())?;
            Address::from_private_key(&key)?;
            let provision = signer::provision(backend, &Terminal, name, &key, kind)?;
            if let Some(given) = address
                && Some(given) != provision.signer.address
            {
                let error = anyhow!("--address does not match the key's address");
                return Err(provision.abandon(backend, name, error));
            }
            provision
        }
    };
    chainz.install_signer(backend, name, &provision)?;
    report_added(name, &provision.signer);
    if kind == Backend::Plaintext {
        eprintln!(
            "{}",
            ui::warn(&format!("'{name}' is stored in plaintext in secrets.toml"))
        );
    }
    if first && !args.no_attach {
        let attached = chainz.transact(|tx| tx.attach_signer_where_missing(name))?;
        if !attached.is_empty() {
            println!(
                "Attached '{name}' to {} (chains without a signer; --no-attach to skip)",
                attached.join(", ")
            );
        }
    }
    Ok(0)
}

fn report_added(name: &str, signer: &Signer) {
    let address = signer
        .address
        .map(|a| a.to_string())
        .unwrap_or_else(|| "no cached address".into());
    println!("Added signer '{name}' ({}) {address}", signer.backend);
}

#[derive(Serialize)]
struct SignerRow<'a> {
    name: &'a str,
    backend: &'static str,
    address: Option<String>,
    chains: Vec<&'a str>,
}

fn ls(json: bool) -> Result<i32> {
    let chainz = open()?;
    let config = chainz.config();
    let rows: Vec<SignerRow> = config
        .signers
        .iter()
        .map(|(name, s)| SignerRow {
            name,
            backend: s.backend.as_str(),
            address: s.address.map(|a| a.to_string()),
            chains: config.chains_using_signer(name),
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if rows.is_empty() {
        println!("No signers. Add one with `chainz signer add <name>`.");
        return Ok(0);
    }
    let cells: Vec<[String; 4]> = rows
        .into_iter()
        .map(|r| {
            [
                r.name.to_string(),
                r.backend.to_string(),
                r.address.unwrap_or_else(|| "-".into()),
                ui::dim(&view::or_dash(&r.chains.join(", "))),
            ]
        })
        .collect();
    print!(
        "{}",
        view::table(&["NAME", "BACKEND", "ADDRESS", "CHAINS"], &cells)
    );
    Ok(0)
}

fn rm(backend: &dyn KeyBackend, args: &SignerRmArgs) -> Result<i32> {
    let name = args.name.as_str();
    let mut chainz = open()?;
    let Some(record) = chainz.config().signers.get(name).cloned() else {
        bail!("signer '{name}' not found");
    };
    let users = chainz.config().chains_using_signer(name);
    if !users.is_empty() && !args.detach {
        bail!(
            "signer '{name}' is used by {}; pass --detach to detach it",
            users.join(", ")
        );
    }
    if let Some(material) = record.key_material() {
        let hint = format!(
            "`signer rm {name}` deletes {material}, which can't be undone; pass --yes to confirm"
        );
        prompt::confirm_or_yes(
            args.yes,
            &format!("Remove signer '{name}' and delete {material}? This can't be undone"),
            Consent::Destructive { hint: &hint },
        )?;
    }
    let users = if args.detach {
        Users::Detach
    } else {
        Users::Keep
    };
    let removed = chainz.remove_signer(backend, name, users)?;
    if let Err(e) = removed.external {
        let material = record.key_material().unwrap_or("its key material");
        eprintln!(
            "{}",
            ui::warn(&format!(
                "removed '{name}' from config but {material} remains: {e:#}"
            ))
        );
    }
    let suffix = if removed.detached > 0 {
        format!(" (detached from {} chain(s))", removed.detached)
    } else {
        String::new()
    };
    println!("Removed signer '{name}'{suffix}");
    Ok(0)
}

fn migrate(backend: &dyn KeyBackend, args: &SignerMigrateArgs) -> Result<i32> {
    let mut chainz = open()?;
    let target = args
        .to
        .map(Backend::from)
        .unwrap_or_else(|| signer::safe_default(backend));
    let all = args.all;
    let names: Vec<String> = match args.name.clone() {
        Some(n) => {
            if !chainz.config().signers.contains_key(&n) {
                bail!("signer '{n}' not found");
            }
            vec![n]
        }
        None => {
            let plain: Vec<String> = chainz
                .config()
                .signers
                .iter()
                .filter(|(_, s)| s.backend == Backend::Plaintext)
                .map(|(n, _)| n.clone())
                .collect();
            if plain.is_empty() {
                println!("No plaintext signers to migrate");
                return Ok(0);
            }
            if !all && plain.len() > 1 {
                bail!(
                    "{} plaintext signers ({}); pass a name or --all",
                    plain.len(),
                    plain.join(", ")
                );
            }
            plain
        }
    };
    if names.len() == 1 {
        migrate_one(backend, &mut chainz, &names[0], target)?;
        return Ok(0);
    }
    // Several: one failure doesn't stop the rest.
    let mut failed = Vec::new();
    for name in &names {
        if let Err(e) = migrate_one(backend, &mut chainz, name, target) {
            eprintln!("{}", ui::fail(&format!("'{name}': {e:#}")));
            failed.push(name.as_str());
        }
    }
    if !failed.is_empty() {
        bail!(
            "{} of {} signers not moved: {}",
            failed.len(),
            names.len(),
            failed.join(", ")
        );
    }
    Ok(0)
}

fn migrate_one(
    backend: &dyn KeyBackend,
    chainz: &mut Chainz,
    name: &str,
    target: Backend,
) -> Result<()> {
    let old = chainz.config().signers[name].clone();
    match old.backend {
        Backend::Keystore | Backend::OnePassword => {
            bail!(
                "signer '{name}' is a {} reference; chainz does not move it",
                old.backend
            )
        }
        b if b == target => {
            println!("'{name}' is already {target}");
            return Ok(());
        }
        _ => {}
    }
    let key = signer::unlock(backend, &Terminal, name, &old, chainz.secrets()?)?;
    let provision = signer::provision(backend, &Terminal, name, &key, target)?;
    if let Some(warning) = chainz.install_signer(backend, name, &provision)? {
        eprintln!("{}", ui::warn(&warning));
    }
    println!("Moved '{name}' from {} to {target}", old.backend);
    Ok(())
}
