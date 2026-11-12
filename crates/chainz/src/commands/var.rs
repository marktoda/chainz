//! `var …`: variables referenced as `${NAME}`; values live in secrets.toml.

use crate::cli::{VarCommand, VarGetArgs, VarLsArgs, VarNameArgs, VarSetArgs};
use crate::load::open;
use crate::prompt::{self, StdinTrim, read_stdin_secret};
use crate::ui;
use anyhow::{Result, bail};
use chainz_core::model::Config;
use chainz_core::secrets::check_var_name;
use chainz_core::signer::Prompter;
use chainz_core::template;
use serde::Serialize;
use zeroize::Zeroizing;

pub fn run(cmd: VarCommand) -> Result<i32> {
    match cmd {
        VarCommand::Set(args) => set(&args),
        VarCommand::Get(args) => get(&args),
        VarCommand::Ls(args) => ls(&args),
        VarCommand::Rm(args) => rm(&args),
    }
}

/// Where `${name}` is referenced in config (chain/endpoint/explorer paths).
fn references(config: &Config, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (path, value) in config.templated_fields() {
        if out.last() != Some(&path) && template::references(value).is_ok_and(|r| r.contains(&name))
        {
            out.push(path);
        }
    }
    out
}

/// Redacted presentation: never any part of the value.
fn masked(value: &str) -> String {
    format!("[REDACTED] ({} chars)", value.chars().count())
}

fn set(args: &VarSetArgs) -> Result<i32> {
    let name = args.name.as_str();
    check_var_name(name)?;
    let value = match &args.value {
        Some(v) => {
            prompt::warn_secret_argument("a variable value", "--stdin or the prompt");
            Zeroizing::new(v.clone())
        }
        None if args.stdin => read_stdin_secret("value", StdinTrim::TrailingNewlines)?,
        None => {
            let terminal = prompt::Terminal;
            if !terminal.is_interactive() {
                bail!("no value given: pipe it with --stdin");
            }
            terminal.secret(&format!("Value for {name}: "))?
        }
    };
    if value.is_empty() {
        bail!("variable value cannot be empty");
    }
    let mut chainz = open()?;
    let existed = chainz.secrets()?.vars.contains_key(name);
    chainz.transact(|tx| tx.set_var(name, &value))?;
    println!(
        "{} variable '{name}'",
        if existed { "Updated" } else { "Set" }
    );
    Ok(0)
}

fn get(args: &VarGetArgs) -> Result<i32> {
    let (name, reveal) = (args.name.as_str(), args.reveal);
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

fn ls(args: &VarLsArgs) -> Result<i32> {
    let (reveal, json) = (args.reveal, args.json);
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

fn rm(args: &VarNameArgs) -> Result<i32> {
    let name = args.name.as_str();
    let mut chainz = open()?;
    let used = references(chainz.config(), name);
    if !used.is_empty() {
        bail!(
            "variable '{name}' is used by {}; change those first",
            used.join(", ")
        );
    }
    let removed = chainz.transact(|tx| Ok(tx.remove_var(name)))?;
    if !removed {
        bail!("variable '{name}' is not set");
    }
    println!("Removed variable '{name}'");
    Ok(0)
}
