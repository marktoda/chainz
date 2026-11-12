//! `config path|edit|migrate`.

use super::{edit_until_saved, open_lenient};
use crate::cli::ConfigCommand;
use anyhow::{Context, Result};
use chainz_core::store::WriteLock;
use chainz_core::{StorePaths, migrate, paths};

pub fn run(cmd: ConfigCommand) -> Result<i32> {
    match cmd {
        ConfigCommand::Path { all } => path(all),
        ConfigCommand::Edit => edit(),
        ConfigCommand::Migrate { dry_run } => migrate_cmd(dry_run),
    }
}

fn path(all: bool) -> Result<i32> {
    let p = StorePaths::resolve()?;
    if !all {
        println!("{}", p.config().display());
        return Ok(0);
    }
    let runtime = chainz_core::runtime::Runtime::resolve()?;
    println!("config   {}", p.config().display());
    println!("secrets  {}", p.secrets().display());
    println!("state    {}", paths::state_dir()?.display());
    println!("cache    {}", paths::cache_dir()?.display());
    println!("runtime  {}", runtime.dir().display());
    Ok(0)
}

fn edit() -> Result<i32> {
    // Opening first runs any pending v1 migration.
    let mut chainz = open_lenient()?;
    let original = chainz.config_text()?;
    let saved = edit_until_saved(&original, |edited| chainz.replace_text(&original, edited))?;
    if saved {
        println!("Saved {}", chainz.paths().config().display());
    } else {
        println!("No changes");
    }
    Ok(0)
}

fn migrate_cmd(dry_run: bool) -> Result<i32> {
    let p = StorePaths::resolve()?;
    if p.config().exists() {
        println!("Already migrated: {}", p.config().display());
        return Ok(0);
    }
    let Some(source) = p.v1_source() else {
        println!("No v1 config found; nothing to migrate");
        return Ok(0);
    };
    if dry_run {
        let text = std::fs::read_to_string(&source)
            .with_context(|| format!("failed to read {}", source.display()))?;
        let plan = migrate::plan(&text)?;
        print!("{}", plan.config_toml);
        for line in plan.summary.report(None) {
            eprintln!("chainz: {line}");
        }
        if !plan.secrets_toml.is_empty() {
            eprintln!("chainz: secrets.toml would hold the extracted values (not printed)");
        }
        return Ok(0);
    }
    let lock = WriteLock::acquire(&p)?;
    match migrate::migrate_locked(&lock)? {
        Some(summary) => {
            for line in summary.report(Some(p.config())) {
                eprintln!("chainz: {line}");
            }
        }
        None => println!("Already migrated: {}", p.config().display()),
    }
    Ok(0)
}
