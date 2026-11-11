//! `signer …`: key references with cached addresses. Key material is only
//! read here (add, migrate) and by `exec --expose-key`.

use super::open;
use crate::cli::{BackendArg, MigrateTarget, SignerCommand};
use crate::prompt::{StdinTrim, read_stdin_secret};
use anyhow::{Context, Result, anyhow, bail};
use chainz_core::model::{Backend, Signer};
use chainz_core::signer::{self, KeyBackend, SystemKeyBackend};
use chainz_core::{Address, Chainz, edit, names, paths};
use serde::Serialize;
use zeroize::Zeroizing;

pub fn run(cmd: SignerCommand) -> Result<i32> {
    let backend = SystemKeyBackend;
    match cmd {
        SignerCommand::Add {
            name,
            stdin,
            key,
            backend: kind,
            path,
            vault,
            item,
            address,
        } => add(
            &backend, &name, stdin, key, kind, path, vault, item, address,
        ),
        SignerCommand::Ls { json } => ls(json),
        SignerCommand::Rm { name, force } => rm(&backend, &name, force),
        SignerCommand::Migrate { name, all, to } => migrate(&backend, name, all, to),
    }
}

fn backend_of(arg: BackendArg) -> Backend {
    match arg {
        BackendArg::Keyring => Backend::Keyring,
        BackendArg::Encrypted => Backend::Encrypted,
        BackendArg::OnePassword => Backend::OnePassword,
        BackendArg::Keystore => Backend::Keystore,
        BackendArg::Plaintext => Backend::Plaintext,
    }
}

fn check_name(chainz: &Chainz, name: &str) -> Result<()> {
    if !names::is_valid_label_name(name) {
        bail!("signer name '{name}' must match [a-z0-9][a-z0-9_-]*");
    }
    if names::CONTEXT_TOKENS.contains(&name) {
        bail!("'{name}' is a reserved token name (@{name}); pick another signer name");
    }
    if chainz.config().signers.contains_key(name) {
        bail!("signer '{name}' already exists; remove it first with `chainz signer rm {name}`");
    }
    Ok(())
}

fn parse_address(raw: &str) -> Result<Address> {
    raw.parse().map_err(|e| anyhow!("invalid --address: {e}"))
}

fn read_key(
    backend: &dyn KeyBackend,
    name: &str,
    stdin: bool,
    key: Option<String>,
) -> Result<Zeroizing<String>> {
    if let Some(key) = key {
        return Ok(Zeroizing::new(key));
    }
    if stdin {
        return read_stdin_secret("private key", StdinTrim::Whitespace);
    }
    if !backend.is_interactive() {
        bail!("no key given: pipe it with --stdin");
    }
    backend.prompt_secret(&format!("Private key for {name}: "))
}

#[allow(clippy::too_many_arguments)]
fn add(
    backend: &dyn KeyBackend,
    name: &str,
    stdin: bool,
    key: Option<String>,
    kind: Option<BackendArg>,
    path: Option<String>,
    vault: Option<String>,
    item: Option<String>,
    address: Option<String>,
) -> Result<i32> {
    let mut chainz = open()?;
    check_name(&chainz, name)?;
    let kind = kind.map(backend_of).unwrap_or_else(|| {
        if path.is_some() {
            Backend::Keystore
        } else if vault.is_some() || item.is_some() {
            Backend::OnePassword
        } else {
            signer::safe_default(backend)
        }
    });
    let address = address.as_deref().map(parse_address).transpose()?;
    match kind {
        Backend::Keystore => {
            let path = path.ok_or_else(|| anyhow!("a keystore signer needs --path"))?;
            let derived = signer::keystore_address(&paths::expand_tilde(&path)).ok();
            let address = match (address, derived) {
                (Some(given), Some(derived)) if given != derived => {
                    bail!("--address does not match the keystore's address {derived}")
                }
                (given, derived) => given.or(derived),
            };
            let record = Signer {
                backend: Backend::Keystore,
                address,
                service: None,
                username: None,
                path: Some(path),
                vault: None,
                item: None,
            };
            chainz.transact(|tx| edit::set_signer(&mut tx.doc, name, &record))?;
            report_added(name, &record);
        }
        Backend::OnePassword => {
            let (vault, item) = match (vault, item) {
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
            let record = Signer {
                backend: Backend::OnePassword,
                address: Some(address),
                service: None,
                username: None,
                path: None,
                vault: Some(vault),
                item: Some(item),
            };
            chainz.transact(|tx| edit::set_signer(&mut tx.doc, name, &record))?;
            report_added(name, &record);
        }
        Backend::Keyring | Backend::Encrypted | Backend::Plaintext => {
            let key = read_key(backend, name, stdin, key)?;
            chainz_core::address::validate_private_key(&key)?;
            let provision = signer::provision(backend, name, &key, kind, None)?;
            if let Some(given) = address
                && Some(given) != provision.signer.address
            {
                signer::rollback(backend, name, &provision)?;
                bail!("--address does not match the key's address");
            }
            let result = chainz.transact(|tx| {
                edit::set_signer(&mut tx.doc, name, &provision.signer)?;
                if let Some(secret) = &provision.secret {
                    edit::set_signer_secret(&mut tx.secrets, name, secret)?;
                }
                Ok(())
            });
            if let Err(e) = result {
                signer::rollback(backend, name, &provision)?;
                return Err(e);
            }
            report_added(name, &provision.signer);
            if kind == Backend::Plaintext {
                eprintln!("chainz: warning: '{name}' is stored in plaintext in secrets.toml");
            }
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
    for r in rows {
        let chains = if r.chains.is_empty() {
            "-".to_string()
        } else {
            r.chains.join(", ")
        };
        println!(
            "{:<16} {:<10} {:<42} {}",
            r.name,
            r.backend,
            r.address.as_deref().unwrap_or("-"),
            crate::ui::dim(&chains)
        );
    }
    Ok(0)
}

fn rm(backend: &dyn KeyBackend, name: &str, force: bool) -> Result<i32> {
    let mut chainz = open()?;
    let Some(record) = chainz.config().signers.get(name).cloned() else {
        bail!("signer '{name}' not found");
    };
    let users = chainz.config().chains_using_signer(name);
    if !users.is_empty() && !force {
        bail!(
            "signer '{name}' is used by {}; pass --force to detach it",
            users.join(", ")
        );
    }
    let detached = chainz.transact(|tx| {
        let detached = edit::remove_signer(&mut tx.doc, name, force)?;
        edit::remove_signer_secret(&mut tx.secrets, name);
        Ok(detached)
    })?;
    if let Err(e) = signer::delete_external(backend, name, &record) {
        eprintln!(
            "chainz: warning: removed '{name}' from config but its keyring entry remains: {e:#}"
        );
    }
    let suffix = if detached > 0 {
        format!(" (detached from {detached} chain(s))")
    } else {
        String::new()
    };
    println!("Removed signer '{name}'{suffix}");
    Ok(0)
}

fn target_of(backend: &dyn KeyBackend, to: Option<MigrateTarget>) -> Backend {
    match to {
        Some(MigrateTarget::Keyring) => Backend::Keyring,
        Some(MigrateTarget::Encrypted) => Backend::Encrypted,
        None => signer::safe_default(backend),
    }
}

fn migrate(
    backend: &dyn KeyBackend,
    name: Option<String>,
    all: bool,
    to: Option<MigrateTarget>,
) -> Result<i32> {
    let mut chainz = open()?;
    let target = target_of(backend, to);
    let names: Vec<String> = match (name, all) {
        (Some(n), _) => {
            if !chainz.config().signers.contains_key(&n) {
                bail!("signer '{n}' not found");
            }
            vec![n]
        }
        (None, _) => {
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
    for name in names {
        let old = chainz.config().signers[&name].clone();
        match old.backend {
            Backend::Keystore | Backend::OnePassword => {
                bail!(
                    "signer '{name}' is a {} reference; chainz does not move it",
                    old.backend
                )
            }
            b if b == target => {
                println!("'{name}' is already {target}");
                continue;
            }
            _ => {}
        }
        let key = signer::unlock(backend, &name, &old, chainz.secrets()?)?;
        // A fresh keyring entry name never clobbers the entry still in use.
        let fresh = (target == Backend::Keyring && old.backend == Backend::Keyring)
            .then(|| format!("{name}-v2"));
        let provision = signer::provision(backend, &name, &key, target, fresh.as_deref())?;
        let result = chainz.transact(|tx| {
            edit::set_signer(&mut tx.doc, &name, &provision.signer)?;
            edit::remove_signer_secret(&mut tx.secrets, &name);
            if let Some(secret) = &provision.secret {
                edit::set_signer_secret(&mut tx.secrets, &name, secret)?;
            }
            Ok(())
        });
        if let Err(e) = result {
            signer::rollback(backend, &name, &provision)?;
            return Err(e);
        }
        if old.backend == Backend::Keyring
            && old.keyring_entry(&name) != provision.signer.keyring_entry(&name)
            && let Err(e) = signer::delete_external(backend, &name, &old)
        {
            eprintln!("chainz: warning: the old keyring entry for '{name}' remains: {e:#}");
        }
        println!("Moved '{name}' from {} to {target}", old.backend);
    }
    Ok(0)
}
