//! `xorgate-rs` — the Rust device agent binary (plan README §3.3): a thin
//! shell over `cli::main` (`src/cli.tsx`'s dispatcher).
//!
//! Phase 2 verbs: `run [flags]` (the worker), `status`, `logs`, `set` and
//! `-v`/`--version` (build identity).

#![deny(unsafe_code)]

fn main() {
    // `process.argv.slice(2)`; non-UTF-8 arguments are decoded lossily.
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    std::process::exit(xorgate_agent::cli::main(&argv));
}
