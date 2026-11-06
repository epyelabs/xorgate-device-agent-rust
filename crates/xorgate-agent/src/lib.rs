//! `xorgate-agent` — the agent's runtime as a library (the `xorgate-rs`
//! binary in `main.rs` is a thin dispatcher over it), so the spine's frozen
//! interfaces are a real API that wave B implements and tests drive.
//!
//! Module list: append-only, alphabetical (shared file; the orchestrator
//! keeps every line at merge).

#![deny(unsafe_code)]

pub mod alloc;
pub mod build_info;
pub mod cli;
pub mod collectors;
pub mod command_plane;
pub mod config_plane;
pub mod ctx;
pub mod env;
pub mod logging;
pub mod media;
pub mod mqtt_plane;
pub mod ops;
pub mod pidfile;
pub mod recording;
pub mod replay;
pub mod scope;
pub mod shutdown;
pub mod state;
pub mod timer;
pub mod worker;

/// `Date.now()`: wall-clock epoch ms — used ONLY to stamp and name things
/// (invariant 2); intervals are measured on `tokio::time::Instant`.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
