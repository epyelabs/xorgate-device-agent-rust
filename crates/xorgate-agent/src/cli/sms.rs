//! `xorgate-rs sms [--limit N] [--json]` and `xorgate-rs sms clear [--yes]` —
//! port of `src/commands/sms.ts` @ 09da38d (Phase 4 WP2; the clear is Phase
//! 5b WP3): read the modem's stored SMS locally with the
//! SAME reader the agent's `read_sms` uses (non-destructive `AT+CMGRO` over
//! the interface-05 tty, `CMGF` / `CPMS` mem1 restored), sharing the agent's
//! lock file (`$XORGATE_HOME/sms-reader.lock`) so it never interleaves with a
//! cloud-triggered sweep. Text + table rendering: `xorgate_core::sms_cli`.
//!
//! This is a local operator tool and DOES print senders and text to the
//! terminal. Nothing here goes to journald. `clear` prints counts only.
//!
//! `clear` is DESTRUCTIVE and irreversible (phase-5b §6 point 2): it runs
//! only with `--yes` / `-y`, or after an interactive `y` / `yes` when stdin is
//! a terminal; with no terminal and no `--yes` it refuses (stderr, exit 1).

use std::io::IsTerminal;
use std::sync::Arc;

use xorgate_core::sms::{build_sms_payload, SmsAction, SmsPayloadRequest};
use xorgate_core::sms_cli::{
    clear_gate, clear_rest, exit_code, render_clear_line, render_json, render_table,
    sms_clear_help, ClearGate, SmsClearArgs, SmsCliArgs, CLI_DEVICE_ID, CLI_REQUEST_ID,
    SMS_CLEAR_CANCELLED, SMS_CLEAR_PROMPT, SMS_CLEAR_REFUSED, SMS_HELP,
};
use xorgate_hal::at_clear::delete_all_text;
use xorgate_hal::at_port::{resolve_sms_port, ResolvePortOptions};
use xorgate_hal::io::null_tape;

use crate::ctx::AgentClock;
use crate::ops::sms::{lock_path, SmsReader, SmsReaderDeps};
use xorgate_hal::sms_clear_token::{ClearRequest, CliConfirmation};

/// `smsCommand(rest)` → exit code (`process.exitCode = 1` unless `ok`).
pub fn main(rest: &[String]) -> i32 {
    // `if (rest[0] === "clear") return smsClearCommand(rest.slice(1));` —
    // before any flag parsing.
    if let Some(rest) = clear_rest(rest) {
        return clear_main(rest);
    }
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
    let sweep = rt.block_on(async { cli_reader(&home).read().await });
    let payload = build_sms_payload(
        &sweep,
        &SmsPayloadRequest::read(CLI_DEVICE_ID, CLI_REQUEST_ID, args.limit),
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

/// `readerForCli()` (`sms.ts:22-24`): the agent's lock file.
fn cli_reader(home: &std::path::Path) -> Arc<SmsReader> {
    SmsReader::new(SmsReaderDeps::system(
        Some(lock_path(home)),
        Arc::new(AgentClock::system()),
        null_tape(),
    ))
}

/// `smsClearCommand(rest)` (`sms.ts:50-80` @ 09da38d) → exit code.
fn clear_main(rest: &[String]) -> i32 {
    let gate = clear_gate(SmsClearArgs::parse(rest), std::io::stdin().is_terminal());
    let answer = match gate {
        ClearGate::Help => {
            let _ = super::logs::write_lines([sms_clear_help(delete_all_text())]);
            return 0;
        }
        ClearGate::Refuse => {
            eprintln!("{SMS_CLEAR_REFUSED}");
            return 1;
        }
        ClearGate::Ask => Some(ask(SMS_CLEAR_PROMPT)),
        ClearGate::Proceed => None,
    };
    // The type-level proof of `--yes` or an interactive yes.
    let Some(confirmed) = CliConfirmation::from_gate(gate, answer.as_deref()) else {
        let _ = super::logs::write_lines([SMS_CLEAR_CANCELLED.to_string()]);
        return 0;
    };
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
    let token = ClearRequest::confirmed_cli(confirmed);
    let sweep = rt.block_on(async { cli_reader(&home).clear(token).await });
    let payload = build_sms_payload(
        &sweep,
        &SmsPayloadRequest {
            device_id: CLI_DEVICE_ID.to_string(),
            request_id: CLI_REQUEST_ID.to_string(),
            limit: 1,
            action: SmsAction::Clear,
        },
    );
    let _ = super::logs::write_lines([render_clear_line(&payload)]);
    exit_code(&payload)
}

/// `confirm(question)` (`sms.ts:36-44`): the question on stdout (no
/// newline), one line from stdin (EOF or a read error = an empty answer);
/// `CliConfirmation::from_gate` applies `/^y(es)?$/i` after trim.
fn ask(question: &str) -> String {
    use std::io::{BufRead, Write};
    let mut out = std::io::stdout();
    let _ = out.write_all(question.as_bytes());
    let _ = out.flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(_) => line,
        Err(_) => String::new(),
    }
}

/// `process.stdout.columns`: the TTY's width, `None` when stdout is not a
/// terminal (the TS `?? 120`).
#[allow(unsafe_code)]
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
