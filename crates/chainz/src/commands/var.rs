//! `var …`: variables referenced as `${NAME}`; values live in secrets.toml.

use crate::cli::{VarCommand, VarLsArgs, VarRmArgs, VarSetArgs, usage_error};
use crate::load::open;
use crate::prompt::{self, StdinTrim, read_stdin_secret};
use crate::{ui, view};
use anyhow::{Result, bail};
use chainz_core::model::{Config, Record};
use chainz_core::secrets::check_var_name;
use chainz_core::signer::Prompter;
use serde::Serialize;
use zeroize::Zeroizing;

pub fn run(cmd: VarCommand) -> Result<i32> {
    match cmd {
        VarCommand::Set(args) => set(&args),
        VarCommand::Ls(args) => ls(&args),
        VarCommand::Rm(args) => rm(&args),
    }
}

/// Where `${name}` is referenced, as config paths.
fn references(config: &Config, name: &str) -> Vec<String> {
    config
        .var_uses(name)
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// What uses a variable, in a few words: `24 endpoints on 17 chains`,
/// `2 endpoints on eth, explorer etherscan`, or `unused`.
fn summary(uses: &[Record<'_>]) -> String {
    let mut chains: Vec<&str> = Vec::new();
    let mut endpoints = 0;
    let mut explorers: Vec<&str> = Vec::new();
    for record in uses {
        match *record {
            Record::Endpoint { chain, .. } => {
                endpoints += 1;
                if !chains.contains(&chain) {
                    chains.push(chain);
                }
            }
            Record::Explorer(name) => explorers.push(name),
        }
    }
    let mut parts = Vec::new();
    match chains.as_slice() {
        [] => {}
        [chain] => parts.push(format!("{} on {chain}", ui::count(endpoints, "endpoint"))),
        _ => parts.push(format!(
            "{} on {}",
            ui::count(endpoints, "endpoint"),
            ui::count(chains.len(), "chain")
        )),
    }
    match explorers.as_slice() {
        [] => {}
        [name] => parts.push(format!("explorer {name}")),
        _ => parts.push(ui::count(explorers.len(), "explorer")),
    }
    if parts.is_empty() {
        "unused".into()
    } else {
        parts.join(", ")
    }
}

/// Redacted presentation: never any part of the value.
fn masked(value: &str) -> String {
    format!(
        "{} ({} chars)",
        chainz_core::redact::REDACTED,
        value.chars().count()
    )
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
                return Err(usage_error(
                    &["var", "set"],
                    "no value given: pipe it with --stdin",
                ));
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

#[derive(Serialize)]
struct VarRow {
    name: String,
    value: String,
    used_by: Vec<String>,
}

fn ls(args: &VarLsArgs) -> Result<i32> {
    let (reveal, json) = (args.reveal, args.json);
    let chainz = open()?;
    let config = chainz.config();
    let secrets = chainz.secrets()?;
    let value = |v: &str| if reveal { v.to_string() } else { masked(v) };
    for name in &args.names {
        if !secrets.vars.contains_key(name) {
            bail!("variable '{name}' is not set");
        }
    }
    let vars: Vec<(&String, &String)> = secrets
        .vars
        .iter()
        .filter(|(n, _)| args.names.is_empty() || args.names.contains(n))
        .collect();
    if json {
        let rows: Vec<VarRow> = vars
            .iter()
            .map(|(n, v)| VarRow {
                name: n.to_string(),
                value: value(v),
                used_by: references(config, n),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if vars.is_empty() {
        println!("No variables. Set one with `chainz var set NAME --stdin`.");
        return Ok(0);
    }
    let rows: Vec<[String; 3]> = vars
        .iter()
        .map(|(n, v)| [n.to_string(), value(v), summary(&config.var_uses(n))])
        .collect();
    let lines = view::table_lines(&["NAME", "VALUE", "USED BY"], &rows);
    println!("{}", lines[0]);
    for ((name, _), line) in vars.iter().zip(&lines[1..]) {
        println!("{line}");
        if args.verbose {
            for record in config.var_uses(name) {
                println!("  {}", ui::dim(&record.to_string()));
            }
        }
    }
    Ok(0)
}

fn rm(args: &VarRmArgs) -> Result<i32> {
    let name = args.name.as_str();
    let mut chainz = open()?;
    let used = references(chainz.config(), name);
    if !used.is_empty() {
        bail!(
            "variable '{name}' is used by {}; change those first",
            used.join(", ")
        );
    }
    if !chainz.secrets()?.vars.contains_key(name) {
        bail!("variable '{name}' is not set");
    }
    prompt::confirm_removal(
        &mut prompt::Tty,
        args.yes,
        &prompt::Removal::of(
            &format!("var rm {name}"),
            &format!("variable '{name}' (its value is kept nowhere else)"),
        ),
    )?;
    chainz.transact(|tx| Ok(tx.remove_var(name)))?;
    println!("Removed variable '{name}'");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint<'a>(chain: &'a str, endpoint: &'a str) -> Record<'a> {
        Record::Endpoint { chain, endpoint }
    }

    #[test]
    fn summaries_count_endpoints_and_chains() {
        assert_eq!(summary(&[]), "unused");
        assert_eq!(summary(&[endpoint("eth", "a")]), "1 endpoint on eth");
        assert_eq!(
            summary(&[
                endpoint("eth", "a"),
                endpoint("eth", "b"),
                endpoint("base", "a")
            ]),
            "3 endpoints on 2 chains"
        );
        assert_eq!(
            summary(&[endpoint("eth", "a"), Record::Explorer("etherscan")]),
            "1 endpoint on eth, explorer etherscan"
        );
        assert_eq!(
            summary(&[Record::Explorer("a"), Record::Explorer("b")]),
            "2 explorers"
        );
    }
}
