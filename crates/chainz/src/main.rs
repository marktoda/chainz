//! The `chainz` binary: clap surface, pickers, output, and dispatch.
//!
//! `exec`, `env`, `ls` and `shell` call only `chainz-core` (synchronous, no
//! sockets, no threads). Commands that talk to the network build a
//! single-threaded tokio runtime on demand through [`net::block_on`].

mod cli;
mod commands;
mod net;
mod prompt;
mod ui;
mod view;

use clap::Parser;

fn main() {
    let opt = cli::Opt::parse();
    let code = match commands::dispatch(opt) {
        Ok(code) => code,
        Err(error) if ui::is_cancelled(&error) => {
            eprintln!("Cancelled");
            130
        }
        Err(error) => {
            eprintln!("Error: {error:#}");
            1
        }
    };
    std::process::exit(code);
}
