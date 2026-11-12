//! `signer …`: key references with cached addresses. Key material is only
//! read here (add, migrate) and by `exec --expose-key`.

use super::{open, parse_address};
use crate::cli::{BackendArg, MigrateTarget, SignerCommand};
use crate::prompt::{self, StdinTrim, read_stdin_secret};
use crate::ui;
use anyhow::{Context, Result, anyhow, bail};
use chainz_core::model::{Backend, Signer};
use chainz_core::signer::{self, KeyBackend, Provision, SystemKeyBackend};
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
        SignerCommand::Rm { name, force, yes } => rm(&backend, &name, force, yes),
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
    names::check_signer_name(name)?;
    if chainz.config().signers.contains_key(name) {
        bail!("signer '{name}' already exists; remove it first with `chainz signer rm {name}`");
    }
    Ok(())
}

/// `original`, with a failed rollback of the backend entry attached as
/// context (the original error stays the cause).
fn with_rollback(original: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => original,
        Err(e) => original.context(format!(
            "rolling back the new key entry also failed ({e:#}); remove it by hand"
        )),
    }
}

/// Undo `provision` after `original` went wrong.
fn roll_back(
    backend: &dyn KeyBackend,
    name: &str,
    provision: &Provision,
    original: anyhow::Error,
) -> anyhow::Error {
    with_rollback(original, signer::rollback(backend, name, provision))
}

fn read_key(
    backend: &dyn KeyBackend,
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
    let address = address
        .as_deref()
        .map(|a| parse_address(a, "--address"))
        .transpose()?;
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
                let error = anyhow!("--address does not match the key's address");
                return Err(roll_back(backend, name, &provision, error));
            }
            let result = chainz.transact(|tx| {
                edit::set_signer(&mut tx.doc, name, &provision.signer)?;
                if let Some(secret) = &provision.secret {
                    edit::set_signer_secret(&mut tx.secrets, name, secret)?;
                }
                Ok(())
            });
            if let Err(e) = result {
                return Err(roll_back(backend, name, &provision, e));
            }
            report_added(name, &provision.signer);
            if kind == Backend::Plaintext {
                eprintln!(
                    "{}",
                    ui::warn(&format!("'{name}' is stored in plaintext in secrets.toml"))
                );
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
            ui::dim(&chains)
        );
    }
    Ok(0)
}

/// What `signer rm` deletes besides the config entry, if anything.
fn key_material(record: &Signer) -> Option<&'static str> {
    match record.backend {
        Backend::Keyring => Some("its OS keyring entry"),
        Backend::Encrypted => Some("its encrypted key in secrets.toml"),
        Backend::Plaintext => Some("its key in secrets.toml"),
        // A reference: the keystore file or 1Password item stays.
        Backend::Keystore | Backend::OnePassword => None,
    }
}

fn rm(backend: &dyn KeyBackend, name: &str, force: bool, yes: bool) -> Result<i32> {
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
    if let Some(material) = key_material(&record) {
        prompt::confirm_or_yes(
            yes,
            &format!("Remove signer '{name}' and delete {material}? This can't be undone"),
            &format!(
                "`signer rm {name}` deletes {material}, which can't be undone; pass --yes to confirm"
            ),
        )?;
    }
    let detached = chainz.transact(|tx| {
        let detached = edit::remove_signer(&mut tx.doc, name, force)?;
        edit::remove_signer_secret(&mut tx.secrets, name);
        Ok(detached)
    })?;
    if let Err(e) = signer::delete_external(backend, name, &record) {
        eprintln!(
            "{}",
            ui::warn(&format!(
                "removed '{name}' from config but its keyring entry remains: {e:#}"
            ))
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
    let key = signer::unlock(backend, name, &old, chainz.secrets()?)?;
    // A fresh keyring entry name never clobbers the entry still in use.
    let fresh = (target == Backend::Keyring && old.backend == Backend::Keyring)
        .then(|| format!("{name}-v2"));
    let provision = signer::provision(backend, name, &key, target, fresh.as_deref())?;
    let result = chainz.transact(|tx| {
        edit::set_signer(&mut tx.doc, name, &provision.signer)?;
        edit::remove_signer_secret(&mut tx.secrets, name);
        if let Some(secret) = &provision.secret {
            edit::set_signer_secret(&mut tx.secrets, name, secret)?;
        }
        Ok(())
    });
    if let Err(e) = result {
        return Err(roll_back(backend, name, &provision, e));
    }
    if old.backend == Backend::Keyring
        && old.keyring_entry(name) != provision.signer.keyring_entry(name)
        && let Err(e) = signer::delete_external(backend, name, &old)
    {
        eprintln!(
            "{}",
            ui::warn(&format!(
                "the old keyring entry for '{name}' remains: {e:#}"
            ))
        );
    }
    println!("Moved '{name}' from {} to {target}", old.backend);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_rollback_is_context_on_the_original_error() {
        let original = || anyhow!("config write failed");
        assert_eq!(
            format!("{:#}", with_rollback(original(), Ok(()))),
            "config write failed"
        );
        let both = with_rollback(original(), Err(anyhow!("keyring locked")));
        let text = format!("{both:#}");
        assert!(text.contains("keyring locked"), "{text}");
        assert!(text.ends_with("config write failed"), "{text}");
        assert_eq!(both.root_cause().to_string(), "config write failed");
    }

    #[test]
    fn only_backends_holding_key_material_need_confirmation() {
        let signer = |backend| Signer {
            backend,
            address: None,
            service: None,
            username: None,
            path: None,
            vault: None,
            item: None,
        };
        for backend in [Backend::Keyring, Backend::Encrypted, Backend::Plaintext] {
            assert!(key_material(&signer(backend)).is_some(), "{backend}");
        }
        for backend in [Backend::Keystore, Backend::OnePassword] {
            assert!(key_material(&signer(backend)).is_none(), "{backend}");
        }
    }
}
