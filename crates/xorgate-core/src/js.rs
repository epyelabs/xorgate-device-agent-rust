//! ECMAScript number/string/date semantics the WP2 parsers lean on
//! (`parseInt`, `parseFloat`, `ToInt32`, `String.prototype.trim`, `Date.UTC`,
//! `Date.prototype.toISOString`). Each helper follows the spec clause the TS
//! source relies on, so a port can call `parse_int(field, 10)` where the TS
//! called `parseInt(field, 10)` and get the same number — including the
//! lenient prefix parsing (`"08abc"` → 8) and the `NaN` results the
//! aggregators later filter. Pure integer/float math only.
//!
//! `Number(string)` already exists as [`crate::system::js_number`]; it is
//! re-exported here so the WP2 modules have one import for all of them.

pub use crate::system::js_number as number;

/// `String.prototype.trim` (ES2023 §22.1.3.32): strips WhiteSpace +
/// LineTerminator. Rust's `str::trim` uses the Unicode `White_Space` property,
/// which additionally includes U+0085 (NEL) and excludes U+FEFF (BOM); JS does
/// the opposite on both.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

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

const MS_PER_DAY: f64 = 86_400_000.0;

/// Days since 1970-01-01 for a proleptic-Gregorian civil date (month 1..=12,
/// any day — day overflow rolls forward like `Date.UTC` does).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: (year, month 1..=12, day 1..=31).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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
    let day0 = days_from_civil(ym as i64, mn as i64 + 1, 1) as f64;
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
    fn trim_is_js_not_unicode() {
        assert_eq!(trim("  a\r\n"), "a");
        assert_eq!(trim("\u{feff}a\u{feff}"), "a");
        assert_eq!(trim("\u{85}a"), "\u{85}a");
        assert_eq!(trim("\u{2028}a\u{a0}"), "a");
    }
}
