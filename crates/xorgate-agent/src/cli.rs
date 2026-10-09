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
    Run {
        /// No AWS: skip MQTT and print each would-be telemetry payload to stdout once per tick.
        #[arg(long)]
        offline: bool,
    },
}
