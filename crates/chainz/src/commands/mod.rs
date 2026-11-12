//! Command handlers, one module per noun, routed from [`dispatch`]. A
//! handler returns the process exit code: 0, or the code of a failure it
//! has already reported itself (`doctor`, `addr find`, `serve status`). Any
//! other failure is an error: `main` prints it as `error: …` and exits 1,
//! or with the code a typed error carries (`ui::exit_code`).

mod addr;
mod chain;
mod doctor;
mod edit;
mod exec;
mod fork;
mod init;
mod rpc;
mod serve;
mod signer;
mod top;
mod var;

use crate::cli::{Command, Opt};
use anyhow::Result;

/// Route a parsed command line to its handler.
pub fn dispatch(opt: Opt) -> Result<i32> {
    let Some(cmd) = opt.cmd else {
        return init::no_command();
    };
    match cmd {
        Command::Exec(args) => exec::exec(&args),
        Command::Shell(args) => exec::shell(&args),
        Command::Env(args) => exec::env(&args),
        Command::Use(args) => chain::use_default(&args),
        Command::Ls(args) => chain::ls(&args),
        Command::Top(args) => top::run(&args),
        Command::Serve { cmd, args } => serve::run(cmd, args),
        Command::Fork { cmd, args } => fork::run(cmd, args),
        Command::Init => init::run(),
        Command::Add(args) => {
            let mut chainz = crate::load::open()?;
            chain::add(&mut chainz, &args, &mut crate::prompt::Tty)
        }
        Command::Update(args) => chain::update(&args),
        Command::Rm(args) => chain::rm(&args),
        Command::Edit(args) => edit::run(&args),
        Command::Rpc { cmd } => rpc::run(cmd),
        Command::Signer { cmd } => signer::run(cmd),
        Command::Addr { cmd } => addr::run(cmd),
        Command::Label(args) => addr::label(&args),
        Command::Var { cmd } => var::run(cmd),
        Command::Doctor(args) => doctor::run(&args),
        Command::Completions(args) => {
            crate::complete::write_loader(args.shell, &mut std::io::stdout().lock())?;
            Ok(0)
        }
    }
}
