//! tracing setup: journald layer when running under systemd (`JOURNAL_STREAM`
//! is set), human `fmt` on **stderr** otherwise (stdout is reserved for
//! `--offline`'s payload lines). Message text keeps the TS `[agent] …`
//! prefixes so operators can grep the same lines.

use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

pub fn init() -> &'static str {
    let filter = EnvFilter::try_from_env("XORGATE_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,rumqttc=warn"));
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        if let Ok(journald) = tracing_journald::layer() {
            tracing_subscriber::registry()
                .with(filter)
                .with(journald.with_syslog_identifier("xorgate-rs".into()))
                .init();
            return "journald";
        }
    }
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false)
                .with_ansi(false)
                .compact(),
        )
        .init();
    "fmt"
}
