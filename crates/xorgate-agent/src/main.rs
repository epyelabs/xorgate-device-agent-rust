//! `xorgate-rs` — the Rust device agent binary (plan README §3.3).
//!
//! Phase 0 verbs: `run [--offline]` (the worker) and `-v`/`--version`
//! (build identity). The remaining CLI verbs (`status`, `set`, `update`, …)
//! arrive with the phases that port their subsystems.

mod build_info;
mod cli;
mod env;
mod logging;
mod pidfile;
mod state;
mod worker;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    if cli.version {
        println!("{}", build_info::load().format());
        return;
    }
    let Some(cmd) = cli.command else {
        eprintln!("usage: xorgate-rs run [--offline] | xorgate-rs -v");
        std::process::exit(2);
    };
    match cmd {
        cli::Command::Run { offline } => {
            let code = worker::main(worker::RunArgs { offline });
            std::process::exit(code);
        }
    }
}
