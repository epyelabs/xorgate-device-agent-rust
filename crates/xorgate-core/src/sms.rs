//! The PURE half of `src/hardware/sms-modem.ts` (692 lines): the AT answer
//! parsers, the per-request limit clamp, the `…/sms` wire payload and the
//! sim/replay fixture inbox.
//!
//! Left to Phase 4's HAL (I/O, timers, processes — `sms-modem.ts` lines):
//! `resolveSmsPort` (`:76`, sysfs walk — the SMS tty is resolved by USB
//! identity `1e0e` / interface `05`, never hard-coded and never
//! `/dev/ttyUSB2`, agent-anatomy invariant 4), `openSerialTransport`
//! (`:131`), `AtSession` (`:212`), `sweepTransport` (`:315`),
//! `acquireSmsLock` (`:431`) and `SmsReader` (`:487`). The constants those
//! need are defined HERE so the strings exist once.
//!
//! Read-only contract (`sms-modem.ts:11-16`): reads use `AT+CMGRO`; `CMGF`
//! and `CPMS` mem1 are read first and restored; nothing is deleted or
//! written. Privacy (contract §6): nothing here logs.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::js::iso_seconds_utc;
use crate::sms_pdu::{SenderType, SmsMessage, SmsStorage};

/// `SmsReadStatus` (`sms-modem.ts:34`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmsReadStatus {
    Ok,
    NoModem,
    NoSim,
    ModemBusy,
    Error,
}

/// `StoreCount` (`sms-modem.ts:36-39`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreCount {
    pub used: u64,
    pub total: u64,
}

/// Per-store counts, keyed by store in first-mention order (a JS object
/// filled by `sweepTransport`, `sms-modem.ts:342-346, 374`).
pub type StorageCounts = IndexMap<SmsStorage, StoreCount>;

/// A finished sweep, before any per-request `limit` cut (`SmsSweep`,
/// `sms-modem.ts:42-53`). Serde order = interface order (it is spread into
/// the payload field by field, never serialised whole by the TS, so the
/// order only matters for fixtures).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmsSweep {
    pub status: SmsReadStatus,
    pub error: Option<String>,
    /// Epoch ms the sweep finished.
    pub finished_at: i64,
    pub duration_ms: i64,
    pub storage: StorageCounts,
    pub skipped: u64,
    pub partial: bool,
    /// All messages, newest first.
    pub messages: Vec<SmsMessage>,
}

// ---------------------------------------------------------------------------
// Port identity + session constants (`sms-modem.ts:59-60, 203, 308, 423`)
// ---------------------------------------------------------------------------

/// `SIMCOM_VENDOR_ID` (`sms-modem.ts:59`): the USB vendor the SMS tty must
/// belong to.
pub const SIMCOM_VENDOR_ID: &str = "1e0e";
/// `SMS_INTERFACE_NUMBER` (`sms-modem.ts:60`): the spare AT parser (the PPP
/// "modem" port), NOT interface 04 (`/dev/ttyUSB2`), which the daemon owns.
pub const SMS_INTERFACE_NUMBER: &str = "05";
/// Env override honoured by `resolveSmsPort` (`sms-modem.ts:78`).
pub const SMS_PORT_ENV: &str = "XORGATE_SMS_PORT";
/// `SWEEP_STORES` (`sms-modem.ts:308`): SM first, then ME.
pub const SWEEP_STORES: [SmsStorage; 2] = [SmsStorage::SM, SmsStorage::ME];
/// Per-command timeout default (`sms-modem.ts:223, 322`).
pub const SMS_COMMAND_TIMEOUT_MS: u64 = 3000;
/// Whole-sweep budget default before `partial` (`sms-modem.ts:321`).
pub const SMS_SWEEP_BUDGET_MS: u64 = 20_000;
/// `SMS_REUSE_WINDOW_MS` (`sms-modem.ts:423`): a request within this window
/// of the last finish reuses that result.
pub const SMS_REUSE_WINDOW_MS: u64 = 5000;
/// The lock file name the worker passes (`worker.ts:917`).
pub const SMS_READER_LOCK_FILE: &str = "sms-reader.lock";

// ---------------------------------------------------------------------------
// AT answer parsers (`sms-modem.ts:203, 277-298, 332, 395`)
// ---------------------------------------------------------------------------

/// `FINAL_RE` (`sms-modem.ts:203`): `^(OK|ERROR|\+CM[SE] ERROR:.*|NO CARRIER)$`.
pub fn is_final_result_line(line: &str) -> bool {
    line == "OK"
        || line == "ERROR"
        || line == "NO CARRIER"
        || line.starts_with("+CMS ERROR:")
        || line.starts_with("+CME ERROR:")
}

/// One `(mem, used, total)` triple of a `+CPMS?` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpmsTriple {
    pub mem: String,
    pub used: u64,
    pub total: u64,
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    i
}

fn digits(b: &[u8], i: usize) -> Option<(u64, usize)> {
    let start = i;
    let mut j = i;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if j == start {
        return None;
    }
    // `Number("…")` of a digit run; a run that overflows u64 saturates
    // (unreachable for a modem's store counts).
    let n = std::str::from_utf8(&b[start..j])
        .ok()?
        .parse::<u64>()
        .unwrap_or(u64::MAX);
    Some((n, j))
}

/// One match of `/"([A-Z]+)"\s*,\s*(\d+)\s*,\s*(\d+)/` starting exactly at `i`.
fn cpms_triple_at(b: &[u8], i: usize) -> Option<(CpmsTriple, usize)> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    let mem_start = j;
    while j < b.len() && b[j].is_ascii_uppercase() {
        j += 1;
    }
    if j == mem_start || b.get(j) != Some(&b'"') {
        return None;
    }
    let mem = String::from_utf8_lossy(&b[mem_start..j]).into_owned();
    j = skip_ws(b, j + 1);
    if b.get(j) != Some(&b',') {
        return None;
    }
    j = skip_ws(b, j + 1);
    let (used, j) = digits(b, j)?;
    let j = skip_ws(b, j);
    if b.get(j) != Some(&b',') {
        return None;
    }
    let j = skip_ws(b, j + 1);
    let (total, j) = digits(b, j)?;
    Some((CpmsTriple { mem, used, total }, j))
}

/// `+CPMS: "SR",0,0,"ME",23,23,"ME",23,23` → three (mem, used, total)
/// triples (`parseCpmsQuery`, `sms-modem.ts:277-284`). Anything that is not
/// a `"MEM",n,n` triple is skipped, like the global regex scan.
pub fn parse_cpms_query(line: &str) -> Vec<CpmsTriple> {
    let body = line
        .strip_prefix("+CPMS:")
        .map(str::trim_start)
        .unwrap_or(line);
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match cpms_triple_at(b, i) {
            Some((t, next)) => {
                out.push(t);
                i = next;
            }
            None => i += 1,
        }
    }
    out
}

/// `+CPMS: 7,40,23,23,23,23` (answer to a set) → mem1 used/total
/// (`parseCpmsSet`, `sms-modem.ts:287-290`).
pub fn parse_cpms_set(line: &str) -> Option<StoreCount> {
    let rest = line.strip_prefix("+CPMS:")?;
    let b = rest.as_bytes();
    let i = skip_ws(b, 0);
    let (used, i) = digits(b, i)?;
    let i = skip_ws(b, i);
    if b.get(i) != Some(&b',') {
        return None;
    }
    let i = skip_ws(b, i + 1);
    let (total, _) = digits(b, i)?;
    Some(StoreCount { used, total })
}

/// SIM-absent / SIM-not-ready errors (27.005 +CMS 310-315, 27.007 +CME
/// 10/13/14) — `isNoSim` (`sms-modem.ts:293-298`). A final line without a
/// numeric code matches the textual `SIM not inserted|SIM failure|SIM busy`
/// (case-insensitive).
pub fn is_no_sim(final_line: &str) -> bool {
    let coded = ["+CMS ERROR:", "+CME ERROR:"].iter().find_map(|prefix| {
        let rest = final_line.strip_prefix(prefix)?;
        let b = rest.as_bytes();
        let (code, _) = digits(b, skip_ws(b, 0))?;
        Some((prefix.starts_with("+CMS"), code))
    });
    match coded {
        Some((true, code)) => (310..=315).contains(&code),
        Some((false, code)) => code == 10 || code == 13 || code == 14,
        None => {
            let l = final_line.to_ascii_lowercase();
            l.contains("sim not inserted") || l.contains("sim failure") || l.contains("sim busy")
        }
    }
}

/// `+CMGF: <n>` → n (`sms-modem.ts:332`: `/\+CMGF:\s*(\d)/`, one digit).
pub fn parse_cmgf_query(line: &str) -> Option<u8> {
    let (_, rest) = line.split_once("+CMGF:")?;
    let b = rest.as_bytes();
    let i = skip_ws(b, 0);
    b.get(i).filter(|c| c.is_ascii_digit()).map(|c| c - b'0')
}

/// `+CMGRO: <stat>,<alpha>,<length>` → `(stat, length)` (`sms-modem.ts:395`:
/// `/^\+CMGRO:\s*(\d+)\s*,[^,]*,\s*(\d+)/`).
pub fn parse_cmgro_header(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("+CMGRO:")?;
    let b = rest.as_bytes();
    let i = skip_ws(b, 0);
    let (stat, i) = digits(b, i)?;
    let i = skip_ws(b, i);
    if b.get(i) != Some(&b',') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() && b[j] != b',' {
        j += 1;
    }
    if b.get(j) != Some(&b',') {
        return None;
    }
    let j = skip_ws(b, j + 1);
    let (length, _) = digits(b, j)?;
    Some((stat, length))
}

// ---------------------------------------------------------------------------
// Wire payload (contract §2; `sms-modem.ts:547-613`)
// ---------------------------------------------------------------------------

/// `SMS_LIMIT_DEFAULT` (`sms-modem.ts:547`).
pub const SMS_LIMIT_DEFAULT: u32 = 10;
/// `SMS_LIMIT_MAX` (`sms-modem.ts:548`).
pub const SMS_LIMIT_MAX: u32 = 50;

/// Contract §1: integer 1..50, missing/invalid → 10 (`clampSmsLimit`,
/// `sms-modem.ts:551-554`) over a JSON value: anything but a number
/// (string, bool, null, object, absent) is the default.
pub fn clamp_sms_limit(v: Option<&Value>) -> u32 {
    match v.and_then(Value::as_f64) {
        Some(n) => clamp_sms_limit_num(n),
        None => SMS_LIMIT_DEFAULT,
    }
}

/// `clampSmsLimit` over a number: non-finite → 10, else
/// `min(50, max(1, trunc(v)))`.
pub fn clamp_sms_limit_num(v: f64) -> u32 {
    if !v.is_finite() {
        return SMS_LIMIT_DEFAULT;
    }
    v.trunc().max(1.0).min(f64::from(SMS_LIMIT_MAX)) as u32
}

/// `SmsResultPayload` (`sms-modem.ts:556-569`): the `…/sms` message. Key
/// order = `buildUncapped`'s literal (`:599-612`), which is the interface
/// order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmsResultPayload {
    pub v: u8,
    pub device_id: String,
    pub request_id: String,
    pub ts: i64,
    pub status: SmsReadStatus,
    /// `null` on success; on failure the sweep's error, or the status word
    /// when the sweep carried none.
    pub error: Option<String>,
    pub duration_ms: i64,
    pub storage: StorageCounts,
    pub total: usize,
    pub skipped: u64,
    pub partial: bool,
    pub messages: Vec<SmsMessage>,
}

/// The per-request half of `buildSmsPayload`'s `req` (`sms-modem.ts:582`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmsPayloadRequest {
    pub device_id: String,
    pub request_id: String,
    /// Re-clamped inside (`:611`), so an unclamped value is safe here.
    pub limit: u32,
}

/// Serialized-size ceiling for one result (`SMS_PAYLOAD_MAX_BYTES`,
/// `sms-modem.ts:578`). AWS IoT rejects messages over 128 KB; 50 messages ×
/// 4000 chars of multi-byte text can exceed that, so the oldest messages are
/// dropped from the cut until it fits, and `partial` is set.
pub const SMS_PAYLOAD_MAX_BYTES: usize = 120 * 1024;

/// `buildSmsPayload` (`sms-modem.ts:580-592`): the uncapped payload, then
/// drop messages from the END (oldest) while the serialised size exceeds
/// [`SMS_PAYLOAD_MAX_BYTES`], flagging `partial`. The size bookkeeping is the
/// TS's (`size -= byteLength(JSON.stringify(dropped)) + 1`), and
/// `serde_json` prints the same bytes `JSON.stringify` does for these shapes
/// (no non-ASCII escaping either side).
pub fn build_sms_payload(sweep: &SmsSweep, req: &SmsPayloadRequest) -> SmsResultPayload {
    let mut p = build_uncapped(sweep, req);
    let mut size = serde_json::to_string(&p).map(|s| s.len()).unwrap_or(0);
    while size > SMS_PAYLOAD_MAX_BYTES && !p.messages.is_empty() {
        let dropped = p.messages.pop().expect("non-empty");
        size -= serde_json::to_string(&dropped)
            .map(|s| s.len())
            .unwrap_or(0)
            + 1;
        p.partial = true;
    }
    p
}

/// `buildUncapped` (`sms-modem.ts:594-613`).
fn build_uncapped(sweep: &SmsSweep, req: &SmsPayloadRequest) -> SmsResultPayload {
    let ok = sweep.status == SmsReadStatus::Ok;
    SmsResultPayload {
        v: 1,
        device_id: req.device_id.clone(),
        request_id: req.request_id.clone(),
        ts: sweep.finished_at,
        status: sweep.status,
        error: if ok {
            None
        } else {
            Some(sweep.error.clone().unwrap_or_else(|| {
                serde_json::to_value(sweep.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            }))
        },
        duration_ms: sweep.duration_ms,
        storage: sweep.storage.clone(),
        total: if ok { sweep.messages.len() } else { 0 },
        skipped: sweep.skipped,
        partial: sweep.partial,
        messages: if ok {
            let n = clamp_sms_limit_num(f64::from(req.limit)) as usize;
            sweep.messages.iter().take(n).cloned().collect()
        } else {
            Vec::new()
        },
    }
}

/// Fixed inbox for `--mode sim` / `--mode replay` (no modem), so cloud dev
/// E2E runs without hardware (`fixtureSmsSweep`, `sms-modem.ts:620-692`).
/// Synthetic content only. Timestamps are relative to `now` (epoch ms) so
/// the newest message always looks recent.
pub fn fixture_sms_sweep(now: i64) -> SmsSweep {
    let iso = |ms_ago: i64| format!("{}+00:00", iso_seconds_utc(now - ms_ago));
    const H: i64 = 3_600_000;
    let msg = |id: &str,
               storage: SmsStorage,
               indexes: &[i64],
               from: &str,
               from_type: SenderType,
               sent_at: String,
               unread: bool,
               text: &str| SmsMessage {
        id: id.to_string(),
        storage,
        indexes: indexes.to_vec(),
        from: from.to_string(),
        from_type,
        sent_at: Some(sent_at),
        unread,
        text: text.to_string(),
        binary: false,
        incomplete: false,
        truncated: false,
    };
    let messages = vec![
        msg(
            "SM:2",
            SmsStorage::SM,
            &[2],
            "XORGATE",
            SenderType::Alphanumeric,
            iso(H / 2),
            true,
            "Simulated device: this is a fixture inbox, not a real modem.",
        ),
        msg(
            "SM:0+SM:1",
            SmsStorage::SM,
            &[0, 1],
            "+15550100001",
            SenderType::International,
            iso(26 * H),
            false,
            "Your data plan renews tomorrow. This fixture message was split in two parts \
             and joined on the device, the way a long carrier notice would be.",
        ),
        msg(
            "ME:4",
            SmsStorage::ME,
            &[4],
            "5550100",
            SenderType::National,
            iso(9 * 24 * H),
            false,
            "Fixture with unicode: café, 東京, 👍",
        ),
        msg(
            "ME:1",
            SmsStorage::ME,
            &[1],
            "+15550100002",
            SenderType::International,
            iso(40 * 24 * H),
            false,
            "Oldest fixture message.",
        ),
    ];
    let mut storage = StorageCounts::new();
    storage.insert(SmsStorage::SM, StoreCount { used: 3, total: 40 });
    storage.insert(SmsStorage::ME, StoreCount { used: 2, total: 23 });
    SmsSweep {
        status: SmsReadStatus::Ok,
        error: None,
        finished_at: now,
        duration_ms: 0,
        storage,
        skipped: 0,
        partial: false,
        messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The AT transcript shapes `test/sms-modem.test.ts`'s FakeModem
    /// (`:75-102`) answers with; Phase 4 replays them against the HAL
    /// `AtSession`. The pure parsers above must read every one.
    #[test]
    fn at_answer_parsers_read_the_fake_modem_transcript() {
        assert!(is_final_result_line("OK"));
        assert!(is_final_result_line("ERROR"));
        assert!(is_final_result_line("+CMS ERROR: 310"));
        assert!(is_final_result_line("+CME ERROR: 10"));
        assert!(is_final_result_line("NO CARRIER"));
        assert!(!is_final_result_line("+CMGF: 0"));
        assert!(!is_final_result_line("ATE0"));
        assert!(!is_final_result_line("+CMTI: \"SM\",3"));
        assert!(!is_final_result_line(" OK"));

        assert_eq!(parse_cmgf_query("+CMGF: 0"), Some(0));
        assert_eq!(parse_cmgf_query("+CMGF: 1"), Some(1));
        assert_eq!(parse_cmgf_query("+CMGF:"), None);
        assert_eq!(parse_cmgf_query("OK"), None);

        assert_eq!(
            parse_cpms_set("+CPMS: 7,40,23,23,23,23"),
            Some(StoreCount { used: 7, total: 40 })
        );
        assert_eq!(
            parse_cpms_set("+CPMS: 0,23,23,23,23,23"),
            Some(StoreCount { used: 0, total: 23 })
        );
        assert_eq!(parse_cpms_set("+CPMS: \"SM\",7,40"), None);
        assert_eq!(parse_cpms_set("+CPMS: 7"), None);

        assert_eq!(parse_cmgro_header("+CMGRO: 0,\"\",28"), Some((0, 28)));
        assert_eq!(
            parse_cmgro_header("+CMGRO: 1,\"+15550100001\",159"),
            Some((1, 159))
        );
        assert_eq!(parse_cmgro_header("+CMGRO: 3,,2"), Some((3, 2)));
        assert_eq!(parse_cmgro_header("+CMGRO: x,\"\",28"), None);
        assert_eq!(parse_cmgro_header("+CMGRO: 0"), None);

        assert!(is_no_sim("+CMS ERROR: 310"));
        assert!(is_no_sim("+CMS ERROR: 315"));
        assert!(!is_no_sim("+CMS ERROR: 316"));
        assert!(!is_no_sim("+CMS ERROR: 309"));
        assert!(is_no_sim("+CME ERROR: 10"));
        assert!(is_no_sim("+CME ERROR: 13"));
        assert!(is_no_sim("+CME ERROR: 14"));
        assert!(!is_no_sim("+CME ERROR: 11"));
        assert!(!is_no_sim("ERROR"));
        assert!(is_no_sim("+CME ERROR: SIM not inserted"));
        assert!(is_no_sim("+CMS ERROR: sim BUSY"));
        assert!(!is_no_sim("+CME ERROR: operation not allowed"));
    }

    #[test]
    fn limit_clamp_edges() {
        assert_eq!(clamp_sms_limit_num(f64::NAN), 10);
        assert_eq!(clamp_sms_limit_num(f64::INFINITY), 10);
        assert_eq!(clamp_sms_limit_num(-0.5), 1);
        assert_eq!(clamp_sms_limit_num(50.9), 50);
        assert_eq!(clamp_sms_limit(None), 10);
        assert_eq!(clamp_sms_limit(Some(&Value::String("5".into()))), 10);
        assert_eq!(clamp_sms_limit(Some(&serde_json::json!(25))), 25);
    }
}
