//! The `chainz` binary: clap surface, pickers, output, and dispatch.
//!
//! `exec`, `env`, `ls` and `shell` call only `chainz-core` (synchronous, no
//! sockets, no threads). Commands that talk to the network build a
//! single-threaded tokio runtime on demand through [`net::block_on`].

mod args;
mod cli;
mod commands;
mod complete;
mod load;
mod net;
mod probe;
mod prompt;
mod select;
mod sys;
mod ui;
mod view;

use clap::Parser;

fn main() {
    // Shell completion requests (`CHAINZ_COMPLETE=zsh chainz -- …`): one env
    // lookup on every other run.
    if complete::requested() {
        complete::run();
    }
    let opt = cli::Opt::parse();
    let code = match commands::dispatch(opt) {
        Ok(code) => code,
        Err(error) => {
            ui::report(&error);
            ui::exit_code(&error)
        }
    };
    std::process::exit(code);
}
