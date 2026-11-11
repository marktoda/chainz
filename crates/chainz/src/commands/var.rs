//! `var …`: variables referenced as `${NAME}`; values live in secrets.toml.

use super::open;
use crate::cli::VarCommand;
use crate::prompt::{StdinTrim, read_stdin_secret};
use crate::ui;
use anyhow::{Result, bail};
use chainz_core::model::Config;
use chainz_core::secrets::BUILTIN_VARS;
use chainz_core::signer::{KeyBackend, SystemKeyBackend};
use chainz_core::{edit, template};
use serde::Serialize;
use zeroize::Zeroizing;

pub fn run(cmd: VarCommand) -> Result<i32> {
    match cmd {
        VarCommand::Set { name, value, stdin } => set(&name, value, stdin),
        VarCommand::Get { name, reveal } => get(&name, reveal),
        VarCommand::Ls { reveal, json } => ls(reveal, json),
        VarCommand::Rm { name } => rm(&name),
    }
}

fn check_name(name: &str) -> Result<()> {
    if !template::is_valid_name(name) {
        bail!("variable name '{name}' must match [A-Za-z_][A-Za-z0-9_]*");
    }
    if BUILTIN_VARS.contains(&name) {
        bail!("'{name}' is built in (the chain being run); pick another name");
    }
    Ok(())
}

/// Where `${name}` is referenced in config (chain/endpoint/explorer paths).
fn references(config: &Config, name: &str) -> Vec<String> {
    let uses = |s: &str| template::references(s).is_ok_and(|r| r.contains(&name));
    let mut out = Vec::new();
    for (chain, c) in &config.chains {
        for (ep, e) in &c.rpcs {
            if uses(&e.url) || e.headers.values().any(|v| uses(v)) {
                out.push(format!("chains.{chain}.rpcs.{ep}"));
            }
        }
    }
    for (n, e) in &config.explorers {
        if uses(&e.api) || e.key.as_deref().is_some_and(uses) {
            out.push(format!("explorers.{n}"));
        }
    }
    out
}

/// Redacted presentation: never any part of the value.
fn masked(value: &str) -> String {
    format!("[REDACTED] ({} chars)", value.chars().count())
}

fn set(name: &str, value: Option<String>, stdin: bool) -> Result<i32> {
    check_name(name)?;
    let value = match value {
        Some(v) => Zeroizing::new(v),
        None if stdin => read_stdin_secret("value", StdinTrim::TrailingNewlines)?,
        None => {
            let backend = SystemKeyBackend;
            if !backend.is_interactive() {
                bail!("no value given: pipe it with --stdin");
            }
            backend.prompt_secret(&format!("Value for {name}: "))?
        }
    };
    if value.is_empty() {
        bail!("variable value cannot be empty");
    }
    let mut chainz = open()?;
    let existed = chainz.secrets()?.vars.contains_key(name);
    chainz.transact(|tx| edit::set_var(&mut tx.secrets, name, &value))?;
    println!(
        "{} variable '{name}'",
        if existed { "Updated" } else { "Set" }
    );
    Ok(0)
}

fn get(name: &str, reveal: bool) -> Result<i32> {
    let chainz = open()?;
    let Some(value) = chainz.secrets()?.vars.get(name) else {
        bail!("variable '{name}' is not set");
    };
    println!("{}", if reveal { value.clone() } else { masked(value) });
    Ok(0)
}

#[derive(Serialize)]
struct VarRow {
    name: String,
    value: String,
    used_by: Vec<String>,
}

fn ls(reveal: bool, json: bool) -> Result<i32> {
    let chainz = open()?;
    let rows: Vec<VarRow> = chainz
        .secrets()?
        .vars
        .iter()
        .map(|(n, v)| VarRow {
            name: n.clone(),
            value: if reveal { v.clone() } else { masked(v) },
            used_by: references(chainz.config(), n),
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if rows.is_empty() {
        println!("No variables. Set one with `chainz var set NAME --stdin`.");
        return Ok(0);
    }
    for r in rows {
        let used = if r.used_by.is_empty() {
            "unused".to_string()
        } else {
            r.used_by.join(", ")
        };
        println!("{:<24} {}  {}", r.name, r.value, ui::dim(&used));
    }
    Ok(0)
}

fn rm(name: &str) -> Result<i32> {
    let mut chainz = open()?;
    let used = references(chainz.config(), name);
    if !used.is_empty() {
        bail!(
            "variable '{name}' is used by {}; change those first",
            used.join(", ")
        );
    }
    let removed = chainz.transact(|tx| Ok(edit::remove_var(&mut tx.secrets, name)))?;
    if !removed {
        bail!("variable '{name}' is not set");
    }
    println!("Removed variable '{name}'");
    Ok(0)
}
