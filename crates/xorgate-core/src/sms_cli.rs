//! The pure half of `xorgate sms [--limit N] [--json]` — `src/commands/sms.ts`
//! @ 89b5c64 (Phase 4 WP2): flag parsing, the `--json` text, the status line
//! and the table. The agent's `cli/sms.rs` runs the reader and prints these.
//!
//! JS string semantics are kept where they show: lengths, padding and the
//! `oneLine` cut count UTF-16 code units (a cut through a surrogate pair
//! prints U+FFFD, as Node writes a lone surrogate to a UTF-8 stdout), and
//! `--limit` goes through `Number(raw)` ([`js_string_to_number`]).
//!
//! This is a local operator tool and DOES print senders and text to the
//! terminal; nothing here goes to journald.

use crate::js;
use crate::sms::{clamp_sms_limit_num, SmsReadStatus, SmsResultPayload, SMS_LIMIT_DEFAULT};

/// The usage text (`sms.ts:35-41`). Q9: a local-only hint, so it names this
/// binary (`xorgate-rs sms`) instead of the TS's `xorgate sms`.
pub const SMS_HELP: &str = "Usage: xorgate-rs sms [--limit N] [--json]

Read the SIM7600's stored SMS (SIM + modem storage), newest first, without
marking anything read. N is 1..50 (default 10). --json prints the exact
payload the agent publishes for a cloud read_sms request.
Port: the SIMCom interface-05 tty (override with XORGATE_SMS_PORT).";

/// The `deviceId` / `requestId` the CLI builds its payload with (`sms.ts:47`).
pub const CLI_DEVICE_ID: &str = "local";
pub const CLI_REQUEST_ID: &str = "cli";

/// `flagValue(rest, name)` (`sms.ts:21-26`): the token after `name`, else
/// the value of the first `name=…` token.
pub fn flag_value<'a>(rest: &'a [String], name: &str) -> Option<&'a str> {
    if let Some(i) = rest.iter().position(|a| a == name) {
        return rest.get(i + 1).map(String::as_str);
    }
    let eq = format!("{name}=");
    rest.iter()
        .find(|a| a.starts_with(&eq))
        .map(|a| &a[eq.len()..])
}

/// What `smsCommand` reads from its argv (`sms.ts:33-45`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmsCliArgs {
    /// `--help` / `-h` anywhere: print [`SMS_HELP`], exit 0, no read.
    pub help: bool,
    /// `clampSmsLimit(raw === undefined ? undefined : Number(raw))`.
    pub limit: u32,
    pub json: bool,
}

impl SmsCliArgs {
    pub fn parse(rest: &[String]) -> Self {
        let help = rest.iter().any(|a| a == "--help" || a == "-h");
        let limit = match flag_value(rest, "--limit") {
            None => SMS_LIMIT_DEFAULT,
            Some(raw) => clamp_sms_limit_num(js_string_to_number(raw)),
        };
        let json = rest.iter().any(|a| a == "--json");
        Self { help, limit, json }
    }
}

/// `process.exitCode = 1` whenever the payload's status is not `ok`
/// (`sms.ts:51, 66`); 0 otherwise.
pub fn exit_code(p: &SmsResultPayload) -> i32 {
    if p.status == SmsReadStatus::Ok {
        0
    } else {
        1
    }
}

/// `JSON.stringify(payload, null, 2)` (`sms.ts:50`): serde_json's pretty
/// printer emits the same bytes for this shape (2-space indent, `": "`,
/// `[]`/`{}` for empties, the same escapes; D1 covers lone surrogates).
pub fn render_json(p: &SmsResultPayload) -> String {
    serde_json::to_string_pretty(p).unwrap_or_default()
}

/// `oneLine(s, max)` (`sms.ts:28-31`): runs of CR/LF/FF/TAB become one
/// space; longer than `max` code units → the first `max - 1` units + `…`.
pub fn one_line(s: &str, max: usize) -> String {
    let mut flat = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if matches!(c, '\r' | '\n' | '\u{c}' | '\t') {
            if !in_run {
                flat.push(' ');
            }
            in_run = true;
        } else {
            flat.push(c);
            in_run = false;
        }
    }
    let units: Vec<u16> = flat.encode_utf16().collect();
    if units.len() > max {
        let mut cut = String::from_utf16_lossy(&units[..max.saturating_sub(1)]);
        cut.push('…');
        cut
    } else {
        flat
    }
}

/// JS `.length` (UTF-16 code units).
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The human output of `smsCommand` after the read (`sms.ts:54-92`), one
/// entry per `console.log` line. `port` = `resolveSmsPort() ?? "(none)"`
/// re-resolved after the read; `columns` = `process.stdout.columns`
/// (`None` when stdout is not a TTY → 120).
pub fn render_table(p: &SmsResultPayload, port: &str, columns: Option<u16>) -> Vec<String> {
    let stores = p
        .storage
        .iter()
        .map(|(k, v)| format!("{} {}/{}", k.as_str(), v.used, v.total))
        .collect::<Vec<_>>()
        .join(", ");
    let err = match p.error.as_deref() {
        Some(e) if !e.is_empty() => format!(" ({e})"),
        _ => String::new(),
    };
    let mut out = vec![format!(
        "sms: {}{err} · port {port} · {} · {} message(s){}{} · {} ms",
        status_word(p.status),
        if stores.is_empty() { "no stores" } else { &stores },
        p.total,
        if p.skipped != 0 {
            format!(", {} skipped", p.skipped)
        } else {
            String::new()
        },
        if p.partial {
            " · PARTIAL (budget ran out)"
        } else {
            ""
        },
        p.duration_ms,
    )];
    if p.status != SmsReadStatus::Ok || p.messages.is_empty() {
        return out;
    }
    let cols = i64::from(columns.unwrap_or(120));
    let width = (cols - 70).max(40) as usize;
    let rows: Vec<[String; 5]> = p
        .messages
        .iter()
        .map(|m| {
            [
                m.id.clone(),
                if m.unread { "unread" } else { "read" }.to_string(),
                m.sent_at.clone().unwrap_or_else(|| "?".to_string()),
                one_line(&m.from, 18),
                one_line(
                    &format!(
                        "{}{}{}{}",
                        if m.binary { "[binary] " } else { "" },
                        if m.incomplete { "[incomplete] " } else { "" },
                        m.text,
                        if m.truncated { " [truncated]" } else { "" },
                    ),
                    width,
                ),
            ]
        })
        .collect();
    let head = ["ID", "STATUS", "SENT", "FROM", "TEXT"].map(str::to_string);
    let widths: Vec<usize> = (0..5)
        .map(|c| {
            rows.iter()
                .map(|r| js_len(&r[c]))
                .fold(js_len(&head[c]), usize::max)
        })
        .collect();
    let fmt = |r: &[String; 5]| -> String {
        r.iter()
            .enumerate()
            .map(|(c, cell)| {
                if c == r.len() - 1 {
                    cell.clone()
                } else {
                    // `padEnd` counts code units.
                    let pad = widths[c].saturating_sub(js_len(cell));
                    format!("{cell}{}", " ".repeat(pad))
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    out.push(fmt(&head));
    for r in &rows {
        out.push(fmt(r));
    }
    if p.total > p.messages.len() {
        out.push(format!(
            "({} older not shown; --limit up to 50)",
            p.total - p.messages.len()
        ));
    }
    out
}

/// The wire word of a status (`ok`, `no_modem`, …).
pub fn status_word(s: SmsReadStatus) -> &'static str {
    match s {
        SmsReadStatus::Ok => "ok",
        SmsReadStatus::NoModem => "no_modem",
        SmsReadStatus::NoSim => "no_sim",
        SmsReadStatus::ModemBusy => "modem_busy",
        SmsReadStatus::Error => "error",
    }
}

/// `Number(string)` — ECMAScript StringToNumber (ES2023 §7.1.4.1.1): JS
/// whitespace trimmed; empty → 0; `0x`/`0o`/`0b` integer literals (no
/// sign); `[+-]Infinity`; else a StrDecimalLiteral (`[+-]` digits with an
/// optional fraction and exponent, at least one digit) or NaN. Used for
/// `--limit` (`sms.ts:43`) and the lock file's pid (`sms-modem.ts:448`).
pub fn js_string_to_number(s: &str) -> f64 {
    let t = js::trim(s);
    if t.is_empty() {
        return 0.0;
    }
    let b = t.as_bytes();
    if b.len() > 2 && b[0] == b'0' {
        let radix = match b[1] {
            b'x' | b'X' => Some(16),
            b'o' | b'O' => Some(8),
            b'b' | b'B' => Some(2),
            _ => None,
        };
        if let Some(r) = radix {
            let mut v = 0f64;
            for &c in &b[2..] {
                match (c as char).to_digit(r) {
                    Some(d) => v = v * f64::from(r) + f64::from(d),
                    None => return f64::NAN,
                }
            }
            return v;
        }
    }
    let (sign, body) = match b[0] {
        b'+' => (1.0, &t[1..]),
        b'-' => (-1.0, &t[1..]),
        _ => (1.0, t),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    if !is_str_unsigned_decimal(body.as_bytes()) {
        return f64::NAN;
    }
    // Rust's parser rounds correctly, like the spec's RoundMVResult.
    let v: f64 = body.parse().unwrap_or(f64::NAN);
    sign * v
}

/// StrUnsignedDecimalLiteral without `Infinity`: `D+ (. D*)? E?` |
/// `. D+ E?`, where `E = [eE] [+-]? D+`.
fn is_str_unsigned_decimal(b: &[u8]) -> bool {
    let mut i = 0;
    let digits = |i: &mut usize| {
        let s = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - s
    };
    let int = digits(&mut i);
    let mut frac = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        frac = digits(&mut i);
    }
    if int == 0 && frac == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_to_number_is_js_number() {
        let n = js_string_to_number;
        assert_eq!(n(""), 0.0);
        assert_eq!(n("  \n"), 0.0);
        assert_eq!(n("5"), 5.0);
        assert_eq!(n(" 12 "), 12.0);
        assert_eq!(n("0x10"), 16.0);
        assert_eq!(n("0b101"), 5.0);
        assert_eq!(n("0o17"), 15.0);
        assert!(n("-0x10").is_nan());
        assert!(n("0x").is_nan());
        assert_eq!(n("1e1"), 10.0);
        assert_eq!(n(".5"), 0.5);
        assert_eq!(n("5."), 5.0);
        assert_eq!(n("-3"), -3.0);
        assert_eq!(n("+7"), 7.0);
        assert_eq!(n("Infinity"), f64::INFINITY);
        assert_eq!(n("-Infinity"), f64::NEG_INFINITY);
        assert!(n("infinity").is_nan());
        assert!(n("inf").is_nan());
        assert!(n("NaN").is_nan());
        assert!(n("abc").is_nan());
        assert!(n("12abc").is_nan());
        assert!(n(".").is_nan());
        assert!(n("1e").is_nan());
        assert!(n("1_000").is_nan());
    }

    #[test]
    fn one_line_counts_code_units() {
        assert_eq!(one_line("a\r\n\tb", 18), "a b");
        assert_eq!(one_line("abcdef", 5), "abcd…");
        assert_eq!(one_line("abcde", 5), "abcde");
        // A cut through a surrogate pair leaves a lone high surrogate → U+FFFD.
        assert_eq!(one_line("ab👍cd", 4), "ab\u{FFFD}…");
    }
}
