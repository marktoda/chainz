//! `config path|migrate` (`chainz edit` edits the file).

use crate::cli::{ConfigCommand, ConfigMigrateArgs, ConfigPathArgs};
use anyhow::{Context, Result};
use chainz_core::store::WriteLock;
use chainz_core::{StorePaths, migrate, paths};

pub fn run(cmd: ConfigCommand) -> Result<i32> {
    match cmd {
        ConfigCommand::Path(args) => path(&args),
        ConfigCommand::Migrate(args) => migrate_cmd(&args),
    }
}

fn path(args: &ConfigPathArgs) -> Result<i32> {
    let p = StorePaths::resolve()?;
    if !args.all {
        println!("{}", p.config().display());
        return Ok(0);
    }
    let runtime = chainz_core::runtime::Runtime::resolve()?;
    println!("config   {}", p.config().display());
    println!("secrets  {}", p.secrets().display());
    println!("state    {}", paths::state_dir()?.display());
    println!("cache    {}", paths::cache_dir()?.display());
    println!("runtime  {}", runtime.dir().display());
    println!("serve    {}", paths::serve_log()?.display());
    println!("fork     {}", paths::fork_log("<name>")?.display());
    Ok(0)
}

fn migrate_cmd(args: &ConfigMigrateArgs) -> Result<i32> {
    let dry_run = args.dry_run;
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
