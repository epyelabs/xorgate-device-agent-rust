//! `xorgate-rs` — the Rust device agent binary (plan README §3.3).
//!
//! Phase 2 verbs: `run [flags]` (the worker), `status`, `logs`, `set` (WP6)
//! and `-v`/`--version` (build identity).

use clap::Parser;
use xorgate_agent::{build_info, cli, worker};

fn main() {
    let cli = cli::Cli::parse();
    if cli.version {
        println!("{}", build_info::load().format());
        return;
    }
    let Some(cmd) = cli.command else {
        eprintln!("usage: xorgate-rs run [--offline] | status | logs | set <key> <value> | -v");
        std::process::exit(2);
    };
    let code = match cmd {
        cli::Command::Run { args } => worker::main(&args),
        cli::Command::Status { args } => cli::status::main(&args),
        cli::Command::Logs { args } => cli::logs::main(&args),
        cli::Command::Set { args } => cli::set::main(&args),
    };
    std::process::exit(code);
}
