//! CLI routing skeleton (Phase 2 WP1; **WP6 takes this file and `main.rs`
//! over** and makes routing, usage text and exit codes equal
//! `src/cli.tsx:1-167`).
//!
//! Every verb receives its raw arguments: `run` parses them with the TS
//! `parseArgs` rules (`--offline`, `--mode real`, `--rate 2`, …; see
//! `xorgate_core::agent_config`), the others are WP6's.

pub mod logs;
pub mod set;
pub mod status;

use clap::{Parser, Subcommand};

/// Xorgate device agent (Rust).
#[derive(Parser, Debug)]
#[command(name = "xorgate-rs", disable_version_flag = true)]
pub struct Cli {
    /// Print the build identity (`<packageVersion> (<version>, <channel>)`) and exit.
    #[arg(short = 'v', long = "version", global = true)]
    pub version: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the agent worker (what `xorgate-agent-rs.service` executes).
    /// Flags as the TS worker: `--offline`, `--mode <sim|real|replay>`,
    /// `--rate <hz>`, `--device-id`, `--endpoint`, `--cert`, `--key`, `--ca`, …
    #[command(disable_help_flag = true)]
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show whether the agent is running (WP6).
    #[command(disable_help_flag = true)]
    Status {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show the agent log (WP6).
    #[command(disable_help_flag = true)]
    Logs {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Change a setting in settings.json (WP6).
    #[command(disable_help_flag = true)]
    Set {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}
