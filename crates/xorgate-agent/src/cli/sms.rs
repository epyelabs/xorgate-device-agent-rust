//! `xorgate-rs sms [--limit N] [--json]` — port of `src/commands/sms.ts:33-93`
//! @ 89b5c64 (Phase 4 WP2): read the modem's stored SMS locally with the
//! SAME reader the agent's `read_sms` uses (non-destructive `AT+CMGRO` over
//! the interface-05 tty, `CMGF` / `CPMS` mem1 restored), sharing the agent's
//! lock file (`$XORGATE_HOME/sms-reader.lock`) so it never interleaves with a
//! cloud-triggered sweep. Text + table rendering: `xorgate_core::sms_cli`.
//!
//! This is a local operator tool and DOES print senders and text to the
//! terminal. Nothing here goes to journald.

use std::io::IsTerminal;
use std::sync::Arc;

use xorgate_core::sms::{build_sms_payload, SmsPayloadRequest};
use xorgate_core::sms_cli::{
    exit_code, render_json, render_table, SmsCliArgs, CLI_DEVICE_ID, CLI_REQUEST_ID, SMS_HELP,
};
use xorgate_hal::at_port::{resolve_sms_port, ResolvePortOptions};
use xorgate_hal::io::null_tape;

use crate::ctx::AgentClock;
use crate::ops::sms::{lock_path, SmsReader, SmsReaderDeps};

/// `smsCommand(rest)` → exit code (`process.exitCode = 1` unless `ok`).
pub fn main(rest: &[String]) -> i32 {
    let args = SmsCliArgs::parse(rest);
    if args.help {
        let _ = super::logs::write_lines([SMS_HELP.to_string()]);
        return 0;
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("✗ {e}");
            return 1;
        }
    };
    let home = crate::env::runtime_home();
    let sweep = rt.block_on(async {
        let reader = SmsReader::new(SmsReaderDeps::system(
            Some(lock_path(&home)),
            Arc::new(AgentClock::system()),
            null_tape(),
        ));
        reader.read().await
    });
    let payload = build_sms_payload(
        &sweep,
        &SmsPayloadRequest {
            device_id: CLI_DEVICE_ID.to_string(),
            request_id: CLI_REQUEST_ID.to_string(),
            limit: args.limit,
        },
    );
    let lines = if args.json {
        vec![render_json(&payload)]
    } else {
        // `resolveSmsPort() ?? "(none)"`, resolved again after the read.
        let port = resolve_sms_port(&ResolvePortOptions::system())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(none)".to_string());
        render_table(&payload, &port, stdout_columns())
    };
    // EPIPE (`sms | head -1`): `console.log` swallows it.
    let _ = super::logs::write_lines(lines);
    exit_code(&payload)
}

/// `process.stdout.columns`: the TTY's width, `None` when stdout is not a
/// terminal (the TS `?? 120`).
fn stdout_columns() -> Option<u16> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    // SAFETY: TIOCGWINSZ fills a `winsize` for a tty fd; `ws` is a valid,
    // zeroed out-pointer for the duration of the call.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0).then_some(ws.ws_col)
}
