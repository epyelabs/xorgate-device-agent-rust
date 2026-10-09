//! The `system` collector's PURE half: parsers for `/proc/stat`, `/proc/meminfo`,
//! the thermal zone, `df -kP` / statvfs numbers, `vcgencmd get_throttled`,
//! `vcgencmd pmic_read_adc` and the device-tree `max_current` node, plus the
//! field assembly (names, units, rounding, order) of the published group.
//!
//! Ported 1:1 from `device/xorgate-device-agent`:
//! - `src/hardware/system.ts` (`computeCpuLoad`, `parseMemAvailable`,
//!   `readCpuTemp`'s number handling, `parseDf`, `usage`, `clampPct`)
//! - `src/hardware/power.ts` (`parseThrottled`, `parsePmicAdc`, `rail5vFrom`,
//!   `readMaxCurrentMa`'s decode)
//! - `src/agent/collectors/system.ts` (`compute()` → field order + rounding)
//!
//! Inputs are raw strings/numbers so everything is testable without I/O; the
//! readers live in `xorgate-hal::system`.

use std::collections::BTreeMap;

use crate::round::round;
use crate::telemetry::{MetricGroup, MetricLeaf};

/// ECMAScript `Number(string)` for the subset of inputs the parsers see:
/// trims whitespace, `""` → 0, decimal/exponent floats, `0x`/`0o`/`0b`
/// integers, `[+-]Infinity`; anything else → NaN.
pub fn js_number(s: &str) -> f64 {
    let t = crate::js::trim(s);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let lower = t.to_ascii_lowercase();
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(digits) = lower.strip_prefix(prefix) {
            // `from_str_radix` takes a leading sign; a JS radix literal does not.
            if digits.starts_with(['+', '-']) {
                return f64::NAN;
            }
            return u64::from_str_radix(digits, radix)
                .map(|v| v as f64)
                .unwrap_or(f64::NAN);
        }
    }
    // Rust's parser accepts "inf"/"nan"/"infinity" (any case) — JS does not.
    if lower.contains("inf") || lower.contains("nan") {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

/// One core's cumulative tick counters, the five fields Node's `os.cpus()`
/// exposes (`times.user/nice/sys/idle/irq`). libuv fills them from
/// `/proc/stat`'s `cpuN` lines (fields user, nice, system, idle, irq —
/// iowait/softirq/steal are NOT included) scaled by `1000 / CLK_TCK`; the
/// scale cancels in every ratio below, so raw ticks give bit-identical results.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CpuTimes {
    pub user: f64,
    pub nice: f64,
    pub sys: f64,
    pub idle: f64,
    pub irq: f64,
}

impl CpuTimes {
    fn total(&self) -> f64 {
        self.user + self.nice + self.sys + self.idle + self.irq
    }
}

/// Snapshot of cumulative CPU tick counters; `.len()` is the core count.
pub type CpuSample = Vec<CpuTimes>;

/// Parse `/proc/stat` into per-core samples (the `cpuN` lines in order; the
/// aggregate `cpu` line is skipped, as libuv does). Lines with fewer than six
/// numeric fields are skipped.
pub fn parse_proc_stat(text: &str) -> CpuSample {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let mut it = rest.split_whitespace();
        let _label = it.next();
        let fields: Vec<f64> = it.take(6).map(js_number).collect();
        if fields.len() < 6 || fields.iter().any(|f| !f.is_finite()) {
            continue;
        }
        out.push(CpuTimes {
            user: fields[0],
            nice: fields[1],
            sys: fields[2],
            idle: fields[3],
            irq: fields[5],
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct CpuLoad {
    /// Mean busy% across all cores, 0–100.
    pub overall: f64,
    /// Busy% per core in core order, 0–100.
    pub per_core: Vec<f64>,
}

fn clamp_pct(n: f64) -> f64 {
    // Math.max(0, Math.min(100, n)) — NaN propagates in JS (max/min of NaN is NaN).
    if n.is_nan() {
        return n;
    }
    n.clamp(0.0, 100.0)
}

/// Pure: busy% per core from two tick snapshots taken some time apart.
/// `busy = (Δtotal − Δidle) / Δtotal`. Cores with no elapsed ticks read 0;
/// a core missing from `prev` reads 0.
pub fn compute_cpu_load(prev: &[CpuTimes], curr: &[CpuTimes]) -> CpuLoad {
    let mut per_core = Vec::with_capacity(curr.len());
    for (i, c) in curr.iter().enumerate() {
        let Some(p) = prev.get(i) else {
            per_core.push(0.0);
            continue;
        };
        let d_total = c.total() - p.total();
        let d_idle = c.idle - p.idle;
        per_core.push(if d_total > 0.0 {
            clamp_pct(((d_total - d_idle) / d_total) * 100.0)
        } else {
            0.0
        });
    }
    let overall = if per_core.is_empty() {
        0.0
    } else {
        per_core.iter().fold(0.0, |a, b| a + b) / per_core.len() as f64
    };
    CpuLoad { overall, per_core }
}

// ---------------------------------------------------------------------------
// Memory / disk
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageInfo {
    pub total_bytes: f64,
    pub used_bytes: f64,
    pub used_pct: f64,
}

/// `usage(totalBytes, usedBytes)` from system.ts: clamp used into [0, total].
pub fn usage(total_bytes: f64, used_bytes: f64) -> UsageInfo {
    let clamped_used = used_bytes.min(total_bytes).max(0.0);
    UsageInfo {
        total_bytes,
        used_bytes: clamped_used,
        used_pct: if total_bytes > 0.0 {
            (clamped_used / total_bytes) * 100.0
        } else {
            0.0
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemAvailable {
    pub total_bytes: f64,
    pub available_bytes: f64,
}

/// `/^MemTotal:\s+(\d+)\s*kB/m` — a line starting with the key, ≥1 whitespace,
/// digits, optional whitespace, `kB`.
fn meminfo_kb(text: &str, key: &str) -> Option<f64> {
    for line in text.split('\n') {
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let trimmed = rest.trim_start();
        if trimmed.len() == rest.len() {
            continue; // `\s+` needs at least one whitespace char
        }
        let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        if trimmed[digits.len()..].trim_start().starts_with("kB") {
            return Some(js_number(&digits));
        }
    }
    None
}

/// Pure: extract MemTotal/MemAvailable (kB → bytes) from `/proc/meminfo` text.
pub fn parse_mem_available(meminfo: &str) -> Option<MemAvailable> {
    let total = meminfo_kb(meminfo, "MemTotal:")?;
    let avail = meminfo_kb(meminfo, "MemAvailable:")?;
    Some(MemAvailable {
        total_bytes: total * 1024.0,
        available_bytes: avail * 1024.0,
    })
}

/// `readMemory()`: prefer `/proc/meminfo`'s MemAvailable; fall back to the
/// OS totals (`os.totalmem()` / `os.freemem()`, i.e. sysinfo) when the file is
/// missing or unparseable. `None` only when neither input exists.
pub fn read_memory(meminfo: Option<&str>, os_fallback: Option<(f64, f64)>) -> Option<UsageInfo> {
    if let Some(parsed) = meminfo.and_then(parse_mem_available) {
        return Some(usage(
            parsed.total_bytes,
            parsed.total_bytes - parsed.available_bytes,
        ));
    }
    os_fallback.map(|(total, free)| usage(total, total - free))
}

/// `readCpuTemp()`'s number handling: `Number(raw.trim()) / 1000`, `None` when
/// not finite (the file itself being absent is the HAL's `None`).
pub fn cpu_temp_from_text(raw: &str) -> Option<f64> {
    let milli = js_number(raw.trim());
    if milli.is_finite() {
        Some(milli / 1000.0)
    } else {
        None
    }
}

/// Pure: parse `df -kP <mount>` output (1024-byte blocks, single data line).
pub fn parse_df(stdout: &str) -> Option<UsageInfo> {
    let lines: Vec<&str> = stdout
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.len() < 2 {
        return None;
    }
    let cols: Vec<&str> = lines[1].split_whitespace().collect();
    let blocks = cols.get(1).map(|s| js_number(s)).unwrap_or(f64::NAN);
    let used = cols.get(2).map(|s| js_number(s)).unwrap_or(f64::NAN);
    if !blocks.is_finite() || !used.is_finite() || blocks <= 0.0 {
        return None;
    }
    Some(usage(blocks * 1024.0, used * 1024.0))
}

/// Disk usage from statvfs numbers, shaped exactly like `df -kP` would print
/// them: df's `1024-blocks` = ⌈blocks·frsize / 1024⌉ and `Used` =
/// ⌈(blocks − bfree)·frsize / 1024⌉ (free = `f_bfree`, i.e. root-reserved
/// blocks count as used, as in df). The TS then multiplies by 1024 — so does
/// this. `None` when the filesystem reports zero blocks (df would print a
/// line `parse_df` rejects).
pub fn disk_usage_from_statvfs(blocks: u64, bfree: u64, frsize: u64) -> Option<UsageInfo> {
    let total_kb = (blocks as u128 * frsize as u128).div_ceil(1024);
    let used_kb = ((blocks.saturating_sub(bfree)) as u128 * frsize as u128).div_ceil(1024);
    if total_kb == 0 {
        return None;
    }
    Some(usage(total_kb as f64 * 1024.0, used_kb as f64 * 1024.0))
}

// ---------------------------------------------------------------------------
// Power (vcgencmd + device tree)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleState {
    /// Bits 0–3 of the raw mask: what is happening right now.
    pub current: u32,
    /// Bits 16–19 shifted down: what has happened since boot.
    pub sticky: u32,
}

/// Pure: parse `vcgencmd get_throttled` output (`throttled=0x50005`).
pub fn parse_throttled(stdout: &str) -> Option<ThrottleState> {
    let idx = stdout.find("throttled=0x")?;
    let hex: String = stdout[idx + "throttled=0x".len()..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.is_empty() {
        return None;
    }
    // parseInt(hex, 16): longer-than-u64 masks are not a real-world case;
    // keep the low bits (what `& 0xf` / `>> 16 & 0xf` read) by saturating.
    let mask = u64::from_str_radix(&hex, 16).unwrap_or(u64::MAX);
    Some(ThrottleState {
        current: (mask & 0xf) as u32,
        sticky: ((mask >> 16) & 0xf) as u32,
    })
}

/// One PMIC rail, keyed by name with the `_A`/`_V` suffix stripped.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PmicRail {
    pub amps: Option<f64>,
    pub volts: Option<f64>,
}

pub type PmicAdc = BTreeMap<String, PmicRail>;

/// Matches `^\s*(\S+)_<SUFFIX> <kind>\(\d+\)=([\d.]+)<UNIT>` for one line.
fn pmic_line(line: &str, suffix: &str, kind: &str, unit: char) -> Option<(String, f64)> {
    let t = line.trim_start();
    let sp = t.find(char::is_whitespace)?;
    let (token, rest) = t.split_at(sp);
    let name = token.strip_suffix(suffix)?;
    if name.is_empty() {
        return None;
    }
    let rest = rest.strip_prefix(' ')?; // exactly one literal space in the regex
    let rest = rest.strip_prefix(kind)?.strip_prefix('(')?;
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = rest[digits..].strip_prefix(")=")?;
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if num.is_empty() || !rest[num.len()..].starts_with(unit) {
        return None;
    }
    Some((name.to_string(), js_number(&num)))
}

/// Pure: parse the no-arg `vcgencmd pmic_read_adc` dump. `None` when no
/// channel parses (a Pi 4 answers an error string).
pub fn parse_pmic_adc(stdout: &str) -> Option<PmicAdc> {
    let mut out = PmicAdc::new();
    for line in stdout.split('\n') {
        if let Some((name, amps)) = pmic_line(line, "_A", "current", 'A') {
            out.entry(name).or_default().amps = Some(amps);
            continue;
        }
        if let Some((name, volts)) = pmic_line(line, "_V", "volt", 'V') {
            out.entry(name).or_default().volts = Some(volts);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Pure: the EXT5V (5 V input rail) voltage from a parsed ADC dump.
pub fn rail_5v_from(adc: &PmicAdc) -> Option<f64> {
    adc.get("EXT5V").and_then(|r| r.volts)
}

/// Decode the device-tree `chosen/power/max_current` node (big-endian u32,
/// mA). `None` on a short buffer.
pub fn max_current_ma_from_bytes(buf: &[u8]) -> Option<u32> {
    if buf.len() < 4 {
        return None;
    }
    Some(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

// ---------------------------------------------------------------------------
// Group assembly
// ---------------------------------------------------------------------------

/// Everything one tick of the system collector derived, before rounding.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SystemReading {
    /// `computeCpuLoad(prev, curr).overall`.
    pub cpu_overall: f64,
    /// `readMemory().usedPct`; `None` only when no memory source exists at all.
    pub ram_used_pct: Option<f64>,
    /// °C from the thermal zone; `None` off-Pi.
    pub cpu_temp_c: Option<f64>,
    /// `readDisk("/").usedPct`; `None` when unavailable.
    pub disk_used_pct: Option<f64>,
    /// `parseThrottled(...).current`; `None` without vcgencmd / on failure.
    pub throttled: Option<u32>,
    /// `rail5vFrom(readPmicAdc())`; `None` when rails are gated off or absent.
    pub rail_5v: Option<f64>,
    /// `readMaxCurrentMa()`; `None` off the Pi 5 family.
    pub psu_max_current_ma: Option<u32>,
}

/// Assemble the published `system` group exactly as `collectors/system.ts:
/// compute()` does: same keys, same insertion order, same units, same
/// `round(v, n)`; absent inputs are omitted (never emitted as 0).
pub fn system_group(r: &SystemReading) -> MetricGroup {
    let mut g = MetricGroup::new();
    g.insert(
        "cpu_usage".into(),
        MetricLeaf::with_unit(round(r.cpu_overall, 1), "%"),
    );
    if let Some(ram) = r.ram_used_pct {
        g.insert(
            "ram_usage".into(),
            MetricLeaf::with_unit(round(ram, 1), "%"),
        );
    }
    if let Some(t) = r.cpu_temp_c {
        g.insert("cpu_temp".into(), MetricLeaf::with_unit(round(t, 1), "°C"));
    }
    if let Some(d) = r.disk_used_pct {
        g.insert("disk_usage".into(), MetricLeaf::with_unit(round(d, 1), "%"));
    }
    if let Some(th) = r.throttled {
        g.insert("throttled".into(), MetricLeaf::new(th as f64));
    }
    if let Some(v) = r.rail_5v {
        g.insert("rail_5v".into(), MetricLeaf::with_unit(round(v, 3), "V"));
    }
    if let Some(ma) = r.psu_max_current_ma {
        g.insert(
            "psu_max_current".into(),
            MetricLeaf::with_unit(ma as f64, "mA"),
        );
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_edge_cases() {
        assert_eq!(js_number(""), 0.0);
        assert_eq!(js_number("  42 "), 42.0);
        assert_eq!(js_number("0x10"), 16.0);
        assert_eq!(js_number("1e3"), 1000.0);
        assert!(js_number("abc").is_nan());
        assert!(js_number("inf").is_nan());
        assert_eq!(js_number("Infinity"), f64::INFINITY);
    }

    #[test]
    fn proc_stat_parses_per_core_lines_only() {
        let text = "cpu  325582 2648 130882 15822545 18151 0 656 0 0 0\n\
                    cpu0 79566 716 38695 3916107 4887 0 483 0 0 0\n\
                    cpu1 82447 625 30869 3968319 4363 0 54 0 0 0\n\
                    intr 1 2 3\n";
        let s = parse_proc_stat(text);
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[0],
            CpuTimes {
                user: 79566.0,
                nice: 716.0,
                sys: 38695.0,
                idle: 3916107.0,
                irq: 0.0
            }
        );
        assert_eq!(s[1].idle, 3968319.0);
    }

    #[test]
    fn statvfs_shapes_like_df() {
        // 100000 4 KiB blocks, 75000 free → df -kP: 400000 1024-blocks, 100000 used → 25 %.
        let u = disk_usage_from_statvfs(100_000, 75_000, 4096).unwrap();
        assert_eq!(u.total_bytes, 400_000.0 * 1024.0);
        assert_eq!(u.used_bytes, 100_000.0 * 1024.0);
        assert_eq!(u.used_pct, 25.0);
        assert!(disk_usage_from_statvfs(0, 0, 4096).is_none());
    }

    #[test]
    fn group_order_units_and_rounding() {
        let r = SystemReading {
            cpu_overall: 5.8499,
            ram_used_pct: Some(11.24),
            cpu_temp_c: Some(52.9),
            disk_used_pct: Some(19.16),
            throttled: Some(0),
            rail_5v: Some(4.9385),
            psu_max_current_ma: Some(3000),
        };
        let g = system_group(&r);
        let keys: Vec<&str> = g.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "cpu_usage",
                "ram_usage",
                "cpu_temp",
                "disk_usage",
                "throttled",
                "rail_5v",
                "psu_max_current"
            ]
        );
        assert_eq!(
            serde_json::to_string(&g).unwrap(),
            r#"{"cpu_usage":{"value":5.8,"unit":"%"},"ram_usage":{"value":11.2,"unit":"%"},"cpu_temp":{"value":52.9,"unit":"°C"},"disk_usage":{"value":19.2,"unit":"%"},"throttled":{"value":0},"rail_5v":{"value":4.939,"unit":"V"},"psu_max_current":{"value":3000,"unit":"mA"}}"#
        );
        // Off-Pi: only cpu + ram.
        let g = system_group(&SystemReading {
            cpu_overall: 0.0,
            ram_used_pct: Some(50.0),
            ..Default::default()
        });
        assert_eq!(
            serde_json::to_string(&g).unwrap(),
            r#"{"cpu_usage":{"value":0,"unit":"%"},"ram_usage":{"value":50,"unit":"%"}}"#
        );
    }
}
