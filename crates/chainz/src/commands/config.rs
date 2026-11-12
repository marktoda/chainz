//! `config path|migrate` (`chainz edit` edits the file).

use crate::cli::{ConfigCommand, ConfigMigrateArgs, ConfigPathArgs};
use crate::ui;
use anyhow::Result;
use chainz_core::store::{Notice, WriteLock};
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
    let p = StorePaths::resolve()?;
    match migrate::status(&p) {
        migrate::Status::Done => {
            println!("Already migrated: {}", p.config().display());
            return Ok(0);
        }
        migrate::Status::NoV1 => {
            println!("No v1 config found; nothing to migrate");
            return Ok(0);
        }
        migrate::Status::Pending(_) => {}
    }
    if args.dry_run {
        let Some(plan) = migrate::preview(&p)? else {
            return Ok(0);
        };
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
        Some(summary) => ui::notice(&Notice::Migrated {
            summary,
            config: p.config().to_path_buf(),
        }),
        None => println!("Already migrated: {}", p.config().display()),
    }
    Ok(0)
}
