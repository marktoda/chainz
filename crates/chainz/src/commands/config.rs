//! `config path|edit|migrate`.

use super::chain::edit_text;
use crate::cli::ConfigCommand;
use crate::prompt::{Prompt, SystemPrompt, is_tty};
use crate::ui;
use anyhow::{Context, Result, bail};
use chainz_core::store::{NEW_CONFIG, WriteLock};
use chainz_core::{Chainz, LoadMode, StorePaths, migrate, paths};

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
        println!("{}", p.config.display());
        return Ok(0);
    }
    let runtime = chainz_core::runtime::Runtime::resolve()?;
    println!("config   {}", p.config.display());
    println!("secrets  {}", p.secrets.display());
    println!("state    {}", paths::state_dir()?.display());
    println!("cache    {}", paths::cache_dir()?.display());
    println!("runtime  {}", runtime.dir().display());
    Ok(0)
}

fn edit() -> Result<i32> {
    // Opening first runs any pending v1 migration.
    let mut chainz = Chainz::open_at(StorePaths::resolve()?, LoadMode::Lenient)?;
    let original =
        paths::read_private(&chainz.paths().config)?.unwrap_or_else(|| NEW_CONFIG.to_string());
    let mut text = original.clone();
    loop {
        let edited = edit_text(&text, ".toml")?;
        if edited == text {
            println!("No changes");
            return Ok(0);
        }
        let result = chainz.transact(|tx| {
            if tx.doc.to_string() != original {
                bail!("config.toml changed while you were editing; run the command again");
            }
            tx.doc = edited.parse().context("config.toml is not valid TOML")?;
            Ok(())
        });
        match result {
            Ok(()) => {
                println!("Saved {}", chainz.paths().config.display());
                return Ok(0);
            }
            Err(e) if is_tty() => {
                eprintln!("{}", ui::fail(&format!("{e:#}")));
                if !SystemPrompt.confirm("Edit again?", true)? {
                    return Err(ui::cancelled());
                }
                text = edited;
            }
            Err(e) => return Err(e),
        }
    }
}

fn migrate_cmd(dry_run: bool) -> Result<i32> {
    let p = StorePaths::resolve()?;
    if p.config.exists() {
        println!("Already migrated: {}", p.config.display());
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
        eprintln!("chainz: {} (dry run: nothing written)", plan.summary);
        for item in &plan.summary.attention {
            eprintln!("chainz:   - {item}");
        }
        if !plan.secrets_toml.is_empty() {
            eprintln!("chainz: secrets.toml would hold the extracted values (not printed)");
        }
        return Ok(0);
    }
    let _lock = WriteLock::acquire(&p)?;
    match migrate::migrate_locked(&p)? {
        Some(summary) => migrate::report(&summary, &p),
        None => println!("Already migrated: {}", p.config.display()),
    }
    Ok(0)
}
