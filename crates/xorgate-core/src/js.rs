//! The ONE home of the ECMAScript built-in semantics the ported TS relies on
//! (Phase 1 integration: WP2 `js.rs` + WP3 `js_fmt.rs` + the private copies
//! WP1/WP4/WP5/WP6 carried). Every helper follows the spec clause the TS
//! source leans on, so a port can call `parse_int(field, 10)` where the TS
//! called `parseInt(field, 10)` — or build a template literal with
//! [`number_to_string`] — and get the same value or bytes:
//!
//! | JS | here |
//! |---|---|
//! | `\s`, `String.prototype.trim` / `trimStart` | [`is_js_whitespace`], [`trim`], [`trim_start`] |
//! | `Number(string)` | [`number`] (= [`crate::system::js_number`]) |
//! | `parseInt`, `parseFloat`, `ToInt32` | [`parse_int`], [`parse_float`], [`to_int32`] |
//! | `String(n)`, template literals, `JSON.stringify` of a number | [`number_to_string`] (and [`crate::telemetry::JsNumber`] for serde) |
//! | `Number.prototype.toFixed` | [`to_fixed`] |
//! | `Math.round` | [`math_round`] (= [`crate::round::js_math_round`]) |
//! | `Date.UTC`, `Date.parse`, `toISOString` | [`date_utc`], [`date_parse`] / [`date_parse_ms`], [`to_iso_string`], [`iso_seconds_utc`] |
//! | V8 own-key order of an object (`JSON.stringify`, `Object.keys`) | [`object_key_order`], [`parsed_key_order`] |
//!
//! Pure integer/float/string math only. The V8 behaviour is pinned by
//! `fixtures/js/js-fmt.json` (180 probes) and `fixtures/js/number-format.json`
//! (5 141 doubles by bit pattern), both dumped from node.

use indexmap::IndexMap;
use serde_json::Value;

pub use crate::round::js_math_round as math_round;
pub use crate::system::js_number as number;

// ---------------------------------------------------------------------------
// whitespace
// ---------------------------------------------------------------------------

/// ECMAScript WhiteSpace ∪ LineTerminator — the set `\s` matches and
/// `String.prototype.trim` strips (TAB, VT, FF, SP, NBSP, ZWNBSP/BOM U+FEFF,
/// every `Zs`, LF, CR, LS, PS). NOT Rust's `char::is_whitespace`, which also
/// matches U+0085 (NEL) and does not match U+FEFF. U+180E is not `Zs` since
/// Unicode 6.3, so it is excluded on both sides.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `String.prototype.trim` (ES2023 §22.1.3.32).
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `String.prototype.trimStart`, and the `^\s*` of a JS regex.
pub fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_whitespace)
}

// ---------------------------------------------------------------------------
// number parsing
// ---------------------------------------------------------------------------

/// `parseInt(string, radix)` (ES2023 §19.2.5) for radix 2..=36: leading
/// whitespace stripped, optional sign, an optional `0x`/`0X` prefix when the
/// radix is 16, then the longest prefix of valid digits. No digits → NaN.
/// The value is accumulated in f64 (the spec's "mathematical integer" then
/// rounded), which is exact for every mask/field length seen on a device.
pub fn parse_int(s: &str, radix: u32) -> f64 {
    debug_assert!((2..=36).contains(&radix));
    let mut t = trim(s);
    let mut sign = 1.0;
    if let Some(rest) = t.strip_prefix('-') {
        sign = -1.0;
        t = rest;
    } else if let Some(rest) = t.strip_prefix('+') {
        t = rest;
    }
    if radix == 16 {
        if let Some(rest) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            t = rest;
        }
    }
    let mut value = 0.0f64;
    let mut digits = 0usize;
    for c in t.chars() {
        match c.to_digit(radix) {
            Some(d) => {
                value = value * radix as f64 + d as f64;
                digits += 1;
            }
            None => break,
        }
    }
    if digits == 0 {
        f64::NAN
    } else {
        sign * value
    }
}

/// `parseFloat(string)` (ES2023 §19.2.4): leading whitespace stripped, then
/// the longest prefix that is a StrDecimalLiteral (`[+-]Infinity`,
/// `[+-]digits[.digits][e[+-]digits]`, `[+-].digits…`). No match → NaN. An
/// incomplete exponent (`"1e"`, `"1e+"`) is not consumed (`"1e"` → 1).
pub fn parse_float(s: &str) -> f64 {
    let t = trim(s);
    let b = t.as_bytes();
    let mut i = 0;
    let mut sign = 1.0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        if b[i] == b'-' {
            sign = -1.0;
        }
        i += 1;
    }
    if t[i..].starts_with("Infinity") {
        return sign * f64::INFINITY;
    }
    let start = i;
    let mut int_digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        int_digits += 1;
    }
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        let mut k = i + 1;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
            frac_digits += 1;
        }
        // "5." is a valid literal (5); "." alone is not.
        if int_digits > 0 || frac_digits > 0 {
            i = k;
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return f64::NAN;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut k = i + 1;
        if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
            k += 1;
        }
        let exp_start = k;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        if k > exp_start {
            i = k;
        }
    }
    let lit = &t[start..i];
    // Rust's f64 parser accepts exactly this grammar (digits, optional
    // fraction, optional exponent) and is correctly rounded, as V8 is.
    let v = if let Some(rest) = lit.strip_prefix('.') {
        format!("0.{rest}").parse::<f64>()
    } else if let Some(rest) = lit.strip_suffix('.') {
        rest.parse::<f64>()
    } else {
        lit.parse::<f64>()
    };
    sign * v.unwrap_or(f64::NAN)
}

/// `ToInt32` (ES2023 §7.1.6): the operand of every JS bitwise operator.
pub fn to_int32(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    let int = v.trunc();
    // modulo 2^32, then into the signed range
    let m = int.rem_euclid(4_294_967_296.0);
    if m >= 2_147_483_648.0 {
        (m - 4_294_967_296.0) as i32
    } else {
        m as i32
    }
}

/// `ToIntegerOrInfinity` (§7.1.5): truncation toward zero, NaN → 0.
fn to_integer(v: f64) -> f64 {
    if v.is_nan() {
        0.0
    } else {
        v.trunc()
    }
}

// ---------------------------------------------------------------------------
// number printing
// ---------------------------------------------------------------------------

/// The shortest round-trip decimal digits of a finite, non-zero `a > 0` and
/// its exponent `n` such that `a = 0.d₁d₂…d_k × 10ⁿ` — the `k`, `n` and `s`
/// of ECMAScript Number::toString step 5. Rust's `{:e}` without a precision
/// prints shortest digits closest to the exact value, as V8 does; the one
/// difference is an exact TIE between two k-digit candidates, where the spec
/// (and V8) take the EVEN one and Rust rounds half up — e.g. 2^-25 =
/// 2.98023223876953125e-8: V8 `2.9802322387695312e-8`, Rust `…313e-8`.
/// [`shortest_tie_to_even`] fixes those up. `fixtures/js/number-format.json`
/// pins the agreement over 5 141 doubles.
fn shortest_digits(a: f64) -> (String, i32) {
    let sci = format!("{a:e}"); // "1.2345e-7", "5e21"
    let (mant, exp) = sci.split_once('e').expect("{:e} prints an exponent");
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let e: i32 = exp.parse().expect("{:e} exponent is an integer");
    shortest_tie_to_even(a, digits.len()).unwrap_or((digits, e + 1))
}

/// When the exact value of `a` lies exactly halfway between two `k`-digit
/// decimals, the even one (if it still round-trips) as `(digits, n)`.
fn shortest_tie_to_even(a: f64, k: usize) -> Option<(String, i32)> {
    let split = |s: String| -> Option<(String, i32)> {
        let (m, x) = s.split_once('e')?;
        Some((
            m.chars().filter(char::is_ascii_digit).collect(),
            x.parse().ok()?,
        ))
    };
    // A halfway value has exactly k+1 significant digits, the last a 5; a
    // double's exact expansion has at most 767, so 800 shows all of it.
    let (wide, x) = split(format!("{a:.p$e}", p = k + 800))?;
    if wide.as_bytes()[k] != b'5' || !wide.as_bytes()[k + 1..].iter().all(|&b| b == b'0') {
        return None;
    }
    let lower = &wide[..k];
    if (lower.as_bytes()[k - 1] - b'0').is_multiple_of(2) {
        // `lower` is even; Rust picked `lower + 1`.
        return check_round_trip(a, lower.to_string(), x);
    }
    // `lower` is odd: the even candidate is `lower + 1` (decimal increment).
    let mut up = lower.as_bytes().to_vec();
    let mut i = up.len();
    let mut x = x;
    loop {
        if i == 0 {
            up.insert(0, b'1');
            up.pop();
            x += 1;
            break;
        }
        i -= 1;
        if up[i] == b'9' {
            up[i] = b'0';
        } else {
            up[i] += 1;
            break;
        }
    }
    check_round_trip(a, String::from_utf8(up).ok()?, x)
}

fn check_round_trip(a: f64, digits: String, x: i32) -> Option<(String, i32)> {
    let digits = digits.trim_end_matches('0').to_string();
    let text = format!("0.{digits}e{}", x + 1);
    (text.parse::<f64>().ok()? == a).then_some((digits, x + 1))
}

/// ECMAScript `Number::toString(x)` radix 10 (ES2023 §6.1.6.1.20) — what
/// `String(n)`, a template literal `${n}` and `JSON.stringify` print for a
/// number. Bit-exact with V8 for every double: integers below 1e21 print as
/// plain digits (`2**60` → `1152921504606847000`: the shortest digits,
/// zero-padded — NOT the exact binary value), fractions down to 1e-6 print
/// positionally (`5e-6` → `0.000005`), everything else in exponent form with
/// an explicit sign (`1e+21`, `1.5e-7`); `-0` → `0`; NaN / ±Infinity spelled
/// out. (`JSON.stringify` maps the non-finite ones to `null` —
/// [`crate::telemetry::JsNumber`] does that.)
pub fn number_to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let (digits, n) = shortest_digits(v.abs());
    let k = digits.len() as i32;
    let mut out = String::with_capacity(digits.len() + 8);
    if v < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 21 {
        // 6. integer: the digits followed by n−k zeros
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        // 7. the first n digits, '.', the rest
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        // 8. "0." then −n zeros then the digits
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        // 9./10. exponent form: d[.ddd]e±(n−1)
        let e = n - 1;
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if e >= 0 { '+' } else { '-' });
        out.push_str(&e.abs().to_string());
    }
    out
}

/// `Number.prototype.toFixed(digits)` (ES2023 §21.1.3.3), `digits` 0..=100:
/// `n` is the integer nearest `x·10^digits` and an exact tie picks the
/// LARGER `n` after the sign is split off — ties round away from zero
/// (`(4.625).toFixed(2) === "4.63"`, `(12.25).toFixed(1) === "12.3"`). Rust's
/// `{:.N}` rounds the same exact binary value but breaks ties to even, so the
/// two differ only on exact ties, which are the values `odd / 2^(digits+1)`;
/// those are resolved here on the exact decimal expansion (no fixed-width
/// integer scaling, so any `digits` and magnitude work). A negative `x`
/// keeps its sign even when every digit is zero (`(-0.001).toFixed(2) ===
/// "-0.00"`); `-0` prints as `"0.00"`. Non-finite or `|x| ≥ 1e21` falls back
/// to [`number_to_string`], as the spec does.
pub fn to_fixed(x: f64, digits: u32) -> String {
    assert!(digits <= 100, "toFixed digits {digits}: RangeError in JS");
    if !x.is_finite() || x.abs() >= 1e21 {
        return number_to_string(x);
    }
    let d = digits as usize;
    let (sign, a) = if x < 0.0 { ("-", -x) } else { ("", x.abs()) }; // -0 → "0.0…"
    let scaled = a * f64::powi(2.0, digits as i32 + 1); // exact: a power of two
    let is_tie =
        scaled.fract() == 0.0 && scaled < 9_007_199_254_740_992.0 && (scaled as u64) % 2 == 1;
    if !is_tie {
        return format!("{sign}{a:.d$}");
    }
    // a = m / 2^(d+1) with m odd ⇒ its decimal expansion has exactly d+1
    // fraction digits and ends in 5: drop the 5 and round the rest up.
    let exact = format!("{a:.prec$}", prec = d + 1);
    let mut out: Vec<u8> = exact.as_bytes()[..exact.len() - 1].to_vec();
    if out.last() == Some(&b'.') {
        out.pop();
    }
    let mut i = out.len();
    loop {
        if i == 0 {
            out.insert(0, b'1');
            break;
        }
        i -= 1;
        match out[i] {
            b'.' => {}
            b'9' => out[i] = b'0',
            c => {
                out[i] = c + 1;
                break;
            }
        }
    }
    format!("{sign}{}", String::from_utf8(out).expect("ASCII digits"))
}

// ---------------------------------------------------------------------------
// dates
// ---------------------------------------------------------------------------

const MS_PER_DAY: f64 = 86_400_000.0;

/// Days since 1970-01-01 for a proleptic-Gregorian civil date (month
/// 1..=12; the day is not range-checked — a day past the month's end rolls
/// forward like V8's `MakeDay`, e.g. 2026-02-31 → 2026-03-03). Howard
/// Hinnant's `days_from_civil`.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let m = i64::from(m);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: (year, month 1..=12, day 1..=31).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `Date.UTC(year, monthIndex, day, hours, minutes, seconds)` (ES2023
/// §21.4.3.4 + MakeDay/MakeTime/MakeDate): every argument is truncated to an
/// integer, a month outside 0..=11 or a day outside the month rolls over,
/// years 0..=99 mean 1900..=1999, and any non-finite argument yields NaN.
pub fn date_utc(year: f64, month: f64, day: f64, hours: f64, minutes: f64, seconds: f64) -> f64 {
    for v in [year, month, day, hours, minutes, seconds] {
        if !v.is_finite() {
            return f64::NAN;
        }
    }
    let mut y = to_integer(year);
    if (0.0..=99.0).contains(&y) {
        y += 1900.0;
    }
    let m = to_integer(month);
    let ym = y + (m / 12.0).floor();
    let mn = m.rem_euclid(12.0);
    let day0 = days_from_civil(ym as i64, mn as u32 + 1, 1) as f64;
    let days = day0 + to_integer(day) - 1.0;
    let time = to_integer(hours) * 3_600_000.0
        + to_integer(minutes) * 60_000.0
        + to_integer(seconds) * 1_000.0;
    days * MS_PER_DAY + time
}

/// `new Date(ms).toISOString()` (§21.4.4.36): `YYYY-MM-DDTHH:mm:ss.sssZ`, the
/// year expanded to `±YYYYYY` outside 0..=9999. `ms` is clipped to an integer
/// like `TimeClip` (truncation). Non-finite input (a RangeError in JS) is
/// rendered as `Invalid Date` so a log line can never panic.
pub fn to_iso_string(ms: f64) -> String {
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return "Invalid Date".to_string();
    }
    let t = to_integer(ms) as i64;
    let days = t.div_euclid(86_400_000);
    let in_day = t.rem_euclid(86_400_000);
    let (y, mo, d) = civil_from_days(days);
    let h = in_day / 3_600_000;
    let mi = (in_day / 60_000) % 60;
    let s = (in_day / 1_000) % 60;
    let milli = in_day % 1_000;
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    };
    format!("{year}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// `new Date(ms).toISOString().slice(0, 19)` — the UTC civil time of an
/// epoch-ms instant without the `.sssZ` tail (the sim/replay SMS fixture,
/// `sms-modem.ts:621-624`). Negative instants floor like JS.
pub fn iso_seconds_utc(ms: i64) -> String {
    to_iso_string(ms as f64).chars().take(19).collect()
}

/// ECMAScript `Date.parse` for the strings the agent compares: the ISO
/// "Date Time String Format" (ES2024 §21.4.1.32) the modem daemon writes
/// (`date -u '+%Y-%m-%dT%H:%M:%SZ'`, `modem/sim7600-lte.sh:496`), the SMS
/// `sentAt` (`decode_scts`) and `Date#toISOString` produce, with V8's
/// leniencies inside that grammar (probed in node 24 and node 20,
/// `fixtures/js/js-fmt.json`): `T`/`t`/space separator, `Z`/`z`, `±HH:mm` or
/// `±HHmm` offsets, any number of fraction digits (the first three are used,
/// truncated), a day of 1–31 that rolls over like `MakeDay` (`2026-02-30` is
/// 2 March), and `24:00:00` as the end of the day. Returns `NaN` otherwise.
///
/// Accepted divergence (`evidence/accepted-divergences.md`): a date-time
/// WITHOUT an offset is LOCAL time in JS; core has no time zone, so it is
/// read as UTC (every producer writes `Z` or an offset). V8's legacy fallback
/// formats (`"2026-1-9"`, `"Oct 9 2026"`) are not reproduced and parse as
/// `NaN`.
pub fn date_parse(s: &str) -> f64 {
    parse_iso_date(s.as_bytes()).unwrap_or(f64::NAN)
}

/// [`date_parse`] as integral epoch ms, `None` for NaN — the shape the
/// `Number.isFinite(Date.parse(…))` call sites (`config-sync.ts:515`,
/// `sms-pdu.ts:348`) consume.
pub fn date_parse_ms(s: &str) -> Option<i64> {
    let v = date_parse(s);
    (!v.is_nan()).then_some(v as i64)
}

fn parse_iso_date(b: &[u8]) -> Option<f64> {
    let mut i = 0usize;
    let digits = |i: &mut usize, n: usize| -> Option<i64> {
        if *i + n > b.len() || !b[*i..*i + n].iter().all(u8::is_ascii_digit) {
            return None;
        }
        let v = b[*i..*i + n]
            .iter()
            .fold(0i64, |acc, d| acc * 10 + i64::from(d - b'0'));
        *i += n;
        Some(v)
    };
    let year = match b.first()? {
        b'+' | b'-' => {
            let neg = b[0] == b'-';
            i = 1;
            let y = digits(&mut i, 6)?;
            if neg && y == 0 {
                return None; // -000000 is not allowed
            }
            if neg {
                -y
            } else {
                y
            }
        }
        _ => digits(&mut i, 4)?,
    };
    let mut month = 1;
    let mut day = 1;
    if b.get(i) == Some(&b'-') {
        i += 1;
        month = digits(&mut i, 2)?;
        if b.get(i) == Some(&b'-') {
            i += 1;
            day = digits(&mut i, 2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut h, mut m, mut sec, mut ms) = (0i64, 0i64, 0i64, 0i64);
    let mut offset_min: i64 = 0;
    if i < b.len() {
        if !matches!(b[i], b'T' | b't' | b' ') {
            return None;
        }
        i += 1;
        h = digits(&mut i, 2)?;
        if b.get(i) != Some(&b':') {
            return None;
        }
        i += 1;
        m = digits(&mut i, 2)?;
        if b.get(i) == Some(&b':') {
            i += 1;
            sec = digits(&mut i, 2)?;
            if b.get(i) == Some(&b'.') {
                i += 1;
                let start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                let frac = &b[start..i];
                let mut v = 0i64;
                for k in 0..3 {
                    v = v * 10 + frac.get(k).map_or(0, |d| i64::from(d - b'0'));
                }
                ms = v;
            }
        }
        if h > 24 || m > 59 || sec > 59 || (h == 24 && (m > 0 || sec > 0 || ms > 0)) {
            return None;
        }
        match b.get(i) {
            None => {}
            Some(b'Z' | b'z') => i += 1,
            Some(&c @ (b'+' | b'-')) => {
                i += 1;
                let oh = digits(&mut i, 2)?;
                if b.get(i) == Some(&b':') {
                    i += 1;
                }
                let om = digits(&mut i, 2)?;
                if oh > 23 || om > 59 {
                    return None;
                }
                offset_min = (oh * 60 + om) * if c == b'-' { -1 } else { 1 };
            }
            Some(_) => return None,
        }
        if i != b.len() {
            return None;
        }
    }
    let days = days_from_civil(year, month as u32, 1) + (day - 1);
    let t = days * 86_400_000 + ((h * 60 + m - offset_min) * 60 + sec) * 1000 + ms;
    // TimeClip: |t| <= 8.64e15.
    if t.abs() > 8_640_000_000_000_000 {
        return None;
    }
    Some(t as f64)
}

// ---------------------------------------------------------------------------
// object key order
// ---------------------------------------------------------------------------

/// Is `key` a JS "array index" property name (canonical decimal integer in
/// `0..2^32-1`)? Those enumerate first, ascending, in `Object.entries` /
/// `JSON.stringify`, ahead of string keys in insertion order.
fn is_array_index_key(key: &str) -> Option<u64> {
    let b = key.as_bytes();
    if b.is_empty() || b.len() > 10 || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if b.len() > 1 && b[0] == b'0' {
        return None;
    }
    let n: u64 = key.parse().ok()?;
    (n < 4_294_967_295).then_some(n)
}

/// Re-order a map the way V8 enumerates object keys: array-index keys first
/// in ascending numeric order, then the rest in insertion order. Applied to
/// every operator-keyed map (`recording.video.cameras`, `cameraMount.cameras`,
/// `effective.cameraMount.{cameras,pending}`) so the serialised text matches
/// `JSON.stringify` byte for byte.
pub fn object_key_order<V>(map: IndexMap<String, V>) -> IndexMap<String, V> {
    let mut indexed: Vec<(u64, String, V)> = Vec::new();
    let mut named: Vec<(String, V)> = Vec::new();
    for (k, v) in map {
        match is_array_index_key(&k) {
            Some(n) => indexed.push((n, k, v)),
            None => named.push((k, v)),
        }
    }
    indexed.sort_by_key(|(n, _, _)| *n);
    indexed
        .into_iter()
        .map(|(_, k, v)| (k, v))
        .chain(named)
        .collect()
}

/// The key order `JSON.parse` leaves behind, applied recursively: V8 own-key
/// order at every object level ([`object_key_order`]). serde_json with
/// `preserve_order` keeps TEXT order, so any object parsed from JSON text
/// that can be re-serialised (the `config-sync.json` `namespaces` baseline
/// re-persisted by `{...state, …}`, the cloud message's `raw` half) goes
/// through this to stringify like the TS.
pub fn parsed_key_order(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let m: IndexMap<String, Value> = m
                .into_iter()
                .map(|(k, v)| (k, parsed_key_order(v)))
                .collect();
            Value::Object(object_key_order(m).into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(parsed_key_order).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_int_prefix_semantics() {
        assert_eq!(parse_int("08", 10), 8.0);
        assert_eq!(parse_int("08abc", 10), 8.0);
        assert_eq!(parse_int("  -12.9", 10), -12.0);
        assert!(parse_int("", 10).is_nan());
        assert!(parse_int("abc", 10).is_nan());
        assert_eq!(parse_int("5A", 16), 90.0);
        assert_eq!(parse_int("0x1f", 16), 31.0);
        assert!(parse_int("0x", 16).is_nan());
        assert_eq!(parse_int("e0008", 16), 0xe0008 as f64);
    }

    #[test]
    fn parse_float_prefix_semantics() {
        assert_eq!(parse_float("545.4"), 545.4);
        assert_eq!(parse_float("1.3abc"), 1.3);
        assert_eq!(parse_float(".5"), 0.5);
        assert_eq!(parse_float("5."), 5.0);
        assert_eq!(parse_float("-.5"), -0.5);
        assert_eq!(parse_float("1e"), 1.0);
        assert_eq!(parse_float("1e3"), 1000.0);
        assert_eq!(parse_float("1e+"), 1.0);
        assert!(parse_float(".").is_nan());
        assert!(parse_float("").is_nan());
        assert!(parse_float("abc").is_nan());
        assert_eq!(parse_float("-Infinity"), f64::NEG_INFINITY);
        assert_eq!(parse_float("0.0"), 0.0);
    }

    #[test]
    fn to_int32_wraps() {
        assert_eq!(to_int32(0xe0000 as f64), 0xe0000);
        assert_eq!(to_int32(4_294_967_297.0), 1);
        assert_eq!(to_int32(2_147_483_648.0), i32::MIN);
        assert_eq!(to_int32(-1.5), -1);
        assert_eq!(to_int32(f64::NAN), 0);
        assert_eq!(to_int32(f64::INFINITY), 0);
    }

    #[test]
    fn date_utc_matches_v8() {
        assert_eq!(
            date_utc(2026.0, 5.0, 13.0, 8.0, 37.0, 45.0),
            1_781_339_865_000.0
        );
        assert_eq!(
            date_utc(2025.0, 0.0, 1.0, 0.0, 0.0, 0.0),
            1_735_689_600_000.0
        );
        assert_eq!(
            date_utc(2100.0, 0.0, 1.0, 0.0, 0.0, 0.0),
            4_102_444_800_000.0
        );
        assert_eq!(date_utc(1980.0, 0.0, 6.0, 0.0, 0.0, 0.0), 315_964_800_000.0);
        assert_eq!(date_utc(1970.0, 0.0, 1.0, 0.0, 0.0, 0.0), 0.0);
        // day overflow rolls forward (Date.UTC(2026, 5, 31) = 2026-07-01)
        assert_eq!(
            date_utc(2026.0, 5.0, 31.0, 0.0, 0.0, 0.0),
            date_utc(2026.0, 6.0, 1.0, 0.0, 0.0, 0.0)
        );
        // month overflow and the 0..=99 → 19xx rule
        assert_eq!(
            date_utc(2025.0, 12.0, 1.0, 0.0, 0.0, 0.0),
            date_utc(2026.0, 0.0, 1.0, 0.0, 0.0, 0.0)
        );
        assert_eq!(
            date_utc(99.0, 11.0, 31.0, 23.0, 59.0, 59.0),
            date_utc(1999.0, 11.0, 31.0, 23.0, 59.0, 59.0)
        );
        // fractional arguments truncate
        assert_eq!(
            date_utc(2026.0, 0.0, 1.0, 0.5, 0.0, 0.0),
            date_utc(2026.0, 0.0, 1.0, 0.0, 0.0, 0.0)
        );
        assert!(date_utc(f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0).is_nan());
    }

    #[test]
    fn iso_string_matches_v8() {
        assert_eq!(
            to_iso_string(1_785_420_306_789.0),
            "2026-07-30T14:05:06.789Z"
        );
        assert_eq!(to_iso_string(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(to_iso_string(315_964_800_000.0), "1980-01-06T00:00:00.000Z");
        assert_eq!(to_iso_string(-1.0), "1969-12-31T23:59:59.999Z");
        assert_eq!(
            to_iso_string(1_785_420_306_789.9),
            "2026-07-30T14:05:06.789Z"
        );
        assert_eq!(
            to_iso_string(date_utc(10_000.0, 0.0, 1.0, 0.0, 0.0, 0.0)),
            "+010000-01-01T00:00:00.000Z"
        );
        assert_eq!(
            to_iso_string(date_utc(-1.0, 0.0, 1.0, 0.0, 0.0, 0.0)),
            "-000001-01-01T00:00:00.000Z"
        );
        assert_eq!(to_iso_string(f64::NAN), "Invalid Date");
    }

    #[test]
    fn iso_seconds_and_civil_days() {
        // (formerly sms_pdu.rs; WP5)
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 2, 31), days_from_civil(2026, 3, 3));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(iso_seconds_utc(0), "1970-01-01T00:00:00");
        assert_eq!(iso_seconds_utc(-1), "1969-12-31T23:59:59");
        assert_eq!(iso_seconds_utc(1_790_812_800_000), "2026-10-01T00:00:00");
    }

    #[test]
    fn date_parse_accepted_divergences() {
        // ACCEPTED (evidence/accepted-divergences.md): an offset-less
        // date-time is LOCAL time in V8 but UTC here (core has no zone; every
        // producer writes `Z`); V8's legacy forms are NaN here (node 24:
        // Date.parse("2026-1-9") = 1767934800000 in America/Toronto).
        assert_eq!(
            date_parse("2026-08-06T12:00:00"),
            date_parse("2026-08-06T12:00:00Z")
        );
        assert!(date_parse("2026-1-9").is_nan());
        assert!(date_parse("Oct 9 2026").is_nan());
    }

    #[test]
    fn date_parse_ms_adapter() {
        // (formerly config_report::js_date_parse_iso's unit test; WP4)
        assert_eq!(
            date_parse_ms("2026-08-06T12:00:00Z").unwrap()
                - date_parse_ms("2026-08-06T11:59:40Z").unwrap(),
            20_000
        );
        assert_eq!(date_parse_ms("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(date_parse_ms("1970-01-01"), Some(0));
        assert_eq!(date_parse_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(date_parse_ms(""), None);
        assert_eq!(date_parse_ms("garbage"), None);
        assert_eq!(date_parse_ms("2026-13-01"), None);
        // V8 rejects an offset hour of 24 (WP4's copy accepted it, review-wp3)
        assert_eq!(date_parse_ms("2026-10-09T12:00:00+24:00"), None);
    }

    #[test]
    fn to_fixed_rounds_ties_away_from_zero() {
        assert_eq!(to_fixed(12.25, 1), "12.3");
        assert_eq!(to_fixed(-12.25, 1), "-12.3");
        assert_eq!(to_fixed(12.75, 1), "12.8");
        assert_eq!(to_fixed(0.25, 1), "0.3");
        assert_eq!(to_fixed(10.05, 1), "10.1"); // 10.05 is not a tie in binary
        assert_eq!(to_fixed(10.45, 1), "10.4"); // 10.4499999… in binary
        assert_eq!(to_fixed(300.0, 1), "300.0");
        assert_eq!(to_fixed(-0.0, 1), "0.0");
        assert_eq!(to_fixed(-0.01, 1), "-0.0");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(0.5, 0), "1");
        assert_eq!(to_fixed(9.5, 0), "10");
        assert_eq!(to_fixed(-9.5, 0), "-10");
        assert_eq!(to_fixed(99.95, 1), "100.0"); // 99.9500000000000028… — not a tie
        assert_eq!(to_fixed(0.125, 2), "0.13");
        assert_eq!(to_fixed(1.005, 2), "1.00"); // 1.00499999… in binary
        assert_eq!(to_fixed(f64::NAN, 1), "NaN");
        assert_eq!(to_fixed(1e21, 2), "1e+21");
        assert_eq!(to_fixed(f64::INFINITY, 2), "Infinity");
        assert_eq!(to_fixed(1e-30, 2), "0.00");
        assert_eq!(to_fixed(123456.0, 0), "123456");
    }

    #[test]
    fn to_fixed_has_no_fixed_width_overflow() {
        // review-wp3 F2: WP3's u128 scaling panicked for digits ≥ 39 and
        // wrapped near 1e21. node 24: (0.1).toFixed(40), (2**-60).toFixed(59) [an exact tie],
        // (999999999999999900000).toFixed(20), (0.5).toFixed(100).
        assert_eq!(
            to_fixed(0.1, 40),
            "0.1000000000000000055511151231257827021182"
        );
        assert_eq!(
            to_fixed(2f64.powi(-60), 59),
            "0.00000000000000000086736173798840354720596224069595336914063"
        );
        assert_eq!(
            to_fixed(999_999_999_999_999_900_000.0, 20),
            "999999999999999868928.00000000000000000000"
        );
        assert_eq!(to_fixed(0.5, 100).len(), 102);
    }

    #[test]
    fn number_to_string_ranges() {
        // node 24 (and fixtures/js/number-format.json): review-wp6 M2 bands.
        assert_eq!(number_to_string(4.85), "4.85");
        assert_eq!(number_to_string(80.0), "80");
        assert_eq!(number_to_string(-0.0), "0");
        assert_eq!(number_to_string(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(
            number_to_string(9_007_199_254_740_992.0),
            "9007199254740992"
        );
        assert_eq!(number_to_string(2f64.powi(60)), "1152921504606847000");
        assert_eq!(number_to_string(1e20), "100000000000000000000");
        assert_eq!(number_to_string(1e21), "1e+21");
        assert_eq!(number_to_string(5e-6), "0.000005");
        assert_eq!(number_to_string(-1.5e-6), "-0.0000015");
        assert_eq!(number_to_string(1e-7), "1e-7");
        assert_eq!(number_to_string(1.5e-7), "1.5e-7");
        assert_eq!(number_to_string(f64::MAX), "1.7976931348623157e+308");
        assert_eq!(number_to_string(5e-324), "5e-324");
        // an exact tie between two 17-digit candidates: V8 takes the even one
        assert_eq!(number_to_string(2f64.powi(-25)), "2.9802322387695312e-8");
        assert_eq!(number_to_string(f64::NAN), "NaN");
        assert_eq!(number_to_string(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn whitespace_is_js_not_unicode() {
        assert_eq!(trim("  a\r\n"), "a");
        assert_eq!(trim("\u{feff}a\u{feff}"), "a");
        assert_eq!(trim("\u{85}a"), "\u{85}a");
        assert_eq!(trim("\u{2028}a\u{a0}"), "a");
        assert_eq!(trim("\u{feff} a \u{3000}\n"), "a");
        assert_eq!(trim("\u{180e}a"), "\u{180e}a");
        assert_eq!(trim_start("\u{feff} a "), "a ");
        assert_eq!(trim_start("\u{85}a"), "\u{85}a");
        // the predicate is exactly Unicode White_Space − {U+0085} + {U+FEFF}
        for c in (0..=0x3000u32)
            .filter_map(char::from_u32)
            .chain(['\u{feff}'])
        {
            let expect = (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}';
            assert_eq!(is_js_whitespace(c), expect, "U+{:04X}", c as u32);
        }
    }

    #[test]
    fn key_order_is_v8() {
        let m: IndexMap<String, i32> = [("cam0", 0), ("10", 1), ("-a", 2), ("9", 3), ("01", 4)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let keys: Vec<String> = object_key_order(m).into_keys().collect();
        assert_eq!(keys, ["9", "10", "cam0", "-a", "01"]);
        let v: Value =
            serde_json::from_str(r#"{"b":{"2":0,"a":1,"1":2},"0":[{"z":0,"5":1}]}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&parsed_key_order(v)).unwrap(),
            r#"{"0":[{"5":1,"z":0}],"b":{"1":2,"2":0,"a":1}}"#
        );
    }
}
