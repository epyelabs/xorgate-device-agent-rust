//! Player-side input rewriting: deviceId substitution and the wall-time shift.
//!
//! A tape recorded at wall time `W0` and replayed at `W1` would hand the agents
//! inputs whose embedded timestamps are `W1 - W0` in the past: every LTE
//! snapshot "stale", every command "expired", every RMC fix far from the system
//! clock. The phase file calls the LTE file "player-rewritten"; the player
//! therefore shifts every embedded wall time by the same whole number of
//! seconds `Δ = round((W1 - W0) / 1000)`, which keeps every RELATIVE time on the
//! tape (an expired command stays expired, a stale snapshot stays stale) and
//! leaves everything else byte-identical. `--no-shift` disables it.
//!
//! Shifted:
//! - LTE file: every `YYYY-MM-DDTHH:MM:SS[.fff]Z` string, every `"read_at": N`
//!   (epoch seconds) and every `"cclk": "yy/MM/dd,HH:mm:ss±zz"` (SIM7600 local
//!   time + quarter-hour offset);
//! - NMEA: the UTC time of GGA/RMC/GLL/GNS/ZDA and the RMC/ZDA date, with the
//!   checksum recomputed (a line whose checksum was already wrong is left alone);
//! - command payloads (`…/command`): top-level numeric `ts` and `expiresAt`.

use serde_json::Value;

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Epoch seconds → (y, mo, d, h, mi, s).
pub fn civil_from_epoch(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

/// ISO-8601 UTC with whole seconds (`2026-10-09T18:14:24Z`).
pub fn iso_z(secs: i64) -> String {
    let (y, m, d, h, mi, s) = civil_from_epoch(secs);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn digits(b: &[u8]) -> Option<i64> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(b).ok()?.parse().ok()
}

/// Shift every `YYYY-MM-DDTHH:MM:SS[.f+]Z` in `text` by `delta_s` seconds.
pub fn shift_iso_strings(text: &str, delta_s: i64) -> String {
    if delta_s == 0 {
        return text.to_string();
    }
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut copied = 0;
    while i + 20 <= b.len() {
        let c = &b[i..];
        let shape = c[4] == b'-'
            && c[7] == b'-'
            && c[10] == b'T'
            && c[13] == b':'
            && c[16] == b':'
            && (i == 0 || !b[i - 1].is_ascii_digit());
        if shape {
            if let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (
                digits(&c[0..4]),
                digits(&c[5..7]),
                digits(&c[8..10]),
                digits(&c[11..13]),
                digits(&c[14..16]),
                digits(&c[17..19]),
            ) {
                // Optional fraction, then a mandatory Z.
                let mut j = 19;
                if c.get(j) == Some(&b'.') {
                    j += 1;
                    while c.get(j).is_some_and(u8::is_ascii_digit) {
                        j += 1;
                    }
                }
                if c.get(j) == Some(&b'Z') && (1..=12).contains(&mo) && (1..=31).contains(&d) {
                    let secs = days_from_civil(y, mo as u32, d as u32) * 86_400
                        + h * 3600
                        + mi * 60
                        + s
                        + delta_s;
                    out.push_str(&text[copied..i]);
                    let shifted = iso_z(secs);
                    // Keep the fraction (sub-second part is not shifted).
                    out.push_str(&shifted[..19]);
                    out.push_str(&text[i + 19..i + j + 1]);
                    i += j + 1;
                    copied = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    out
}

/// Shift `"read_at": N` integers (epoch seconds) by `delta_s`.
fn shift_read_at(text: &str, delta_s: i64) -> String {
    let key = "\"read_at\"";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find(key) {
        let after = &rest[p + key.len()..];
        let ws1 = after.len() - after.trim_start().len();
        let a1 = &after[ws1..];
        if let Some(a2) = a1.strip_prefix(':') {
            let ws2 = a2.len() - a2.trim_start().len();
            let num = &a2[ws2..];
            let n_len = num.bytes().take_while(u8::is_ascii_digit).count();
            if n_len > 0 && !num[n_len..].starts_with('.') {
                if let Ok(n) = num[..n_len].parse::<i64>() {
                    let head = p + key.len() + ws1 + 1 + ws2;
                    out.push_str(&rest[..head]);
                    out.push_str(&(n + delta_s).to_string());
                    rest = &rest[head + n_len..];
                    continue;
                }
            }
        }
        out.push_str(&rest[..p + key.len()]);
        rest = &rest[p + key.len()..];
    }
    out.push_str(rest);
    out
}

/// Shift a SIM7600 `+CCLK` string `yy/MM/dd,HH:mm:ss±zz` by `delta_s`, keeping
/// the zone. `None` when it does not have that shape.
pub fn shift_cclk(s: &str, delta_s: i64) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[2] != b'/'
        || b[5] != b'/'
        || b[8] != b','
        || b[11] != b':'
        || b[14] != b':'
    {
        return None;
    }
    let (yy, mo, d, h, mi, sec) = (
        digits(&b[0..2])?,
        digits(&b[3..5])?,
        digits(&b[6..8])?,
        digits(&b[9..11])?,
        digits(&b[12..14])?,
        digits(&b[15..17])?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let zone = &s[17..];
    let secs = days_from_civil(2000 + yy, mo as u32, d as u32) * 86_400
        + h * 3600
        + mi * 60
        + sec
        + delta_s;
    let (y, m, dd, hh, mm, ss) = civil_from_epoch(secs);
    Some(format!(
        "{:02}/{m:02}/{dd:02},{hh:02}:{mm:02}:{ss:02}{zone}",
        y.rem_euclid(100)
    ))
}

fn shift_cclk_fields(text: &str, delta_s: i64) -> String {
    let key = "\"cclk\"";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find(key) {
        let after = &rest[p + key.len()..];
        let ws1 = after.len() - after.trim_start().len();
        let a1 = &after[ws1..];
        if let Some(a2) = a1.strip_prefix(':') {
            let ws2 = a2.len() - a2.trim_start().len();
            let v = &a2[ws2..];
            if let Some(body) = v.strip_prefix('"') {
                if let Some(end) = body.find('"') {
                    if let Some(shifted) = shift_cclk(&body[..end], delta_s) {
                        let head = p + key.len() + ws1 + 1 + ws2 + 1;
                        out.push_str(&rest[..head]);
                        out.push_str(&shifted);
                        rest = &rest[head + end..];
                        continue;
                    }
                }
            }
        }
        out.push_str(&rest[..p + key.len()]);
        rest = &rest[p + key.len()..];
    }
    out.push_str(rest);
    out
}

/// The LTE telemetry file, shifted (see the module doc).
pub fn shift_lte_text(text: &str, delta_s: i64) -> String {
    if delta_s == 0 {
        return text.to_string();
    }
    shift_cclk_fields(
        &shift_read_at(&shift_iso_strings(text, delta_s), delta_s),
        delta_s,
    )
}

/// NMEA XOR checksum of the body between `$` and `*`.
pub fn nmea_checksum(body: &str) -> u8 {
    body.bytes().fold(0u8, |a, b| a ^ b)
}

/// Build a sentence `$<body>*HH`.
pub fn nmea_sentence(body: &str) -> String {
    format!("${body}*{:02X}", nmea_checksum(body))
}

fn shift_hms(field: &str, delta_s: i64) -> Option<(String, i64)> {
    let b = field.as_bytes();
    if b.len() < 6 {
        return None;
    }
    let (h, m, s) = (digits(&b[0..2])?, digits(&b[2..4])?, digits(&b[4..6])?);
    let frac = &field[6..];
    if !(frac.is_empty()
        || (frac.starts_with('.') && frac[1..].bytes().all(|c| c.is_ascii_digit())))
    {
        return None;
    }
    let total = h * 3600 + m * 60 + s + delta_s;
    let day_carry = total.div_euclid(86_400);
    let t = total.rem_euclid(86_400);
    Some((
        format!("{:02}{:02}{:02}{frac}", t / 3600, t % 3600 / 60, t % 60),
        day_carry,
    ))
}

/// Shift one NMEA line (see the module doc). Keeps a trailing `\r`.
pub fn shift_nmea_line(line: &str, delta_s: i64) -> String {
    if delta_s == 0 {
        return line.to_string();
    }
    let (core, cr) = match line.strip_suffix('\r') {
        Some(c) => (c, "\r"),
        None => (line, ""),
    };
    let trimmed = core.trim();
    let Some(after_dollar) = trimmed.strip_prefix('$') else {
        return line.to_string();
    };
    let (body, had_cs) = match after_dollar.rsplit_once('*') {
        Some((body, cs)) => {
            let ok = u8::from_str_radix(cs, 16).ok() == Some(nmea_checksum(body)) && cs.len() == 2;
            if !ok {
                return line.to_string();
            }
            (body, true)
        }
        None => (after_dollar, false),
    };
    let mut f: Vec<String> = body.split(',').map(str::to_string).collect();
    let id = f[0].get(2..).unwrap_or("").to_string();
    let time_idx = match id.as_str() {
        "GGA" | "RMC" | "GNS" | "ZDA" => 1,
        "GLL" => 5,
        _ => return line.to_string(),
    };
    let Some(tf) = f.get(time_idx).cloned() else {
        return line.to_string();
    };
    if tf.is_empty() {
        return line.to_string();
    }
    let Some((t_new, carry)) = shift_hms(&tf, delta_s) else {
        return line.to_string();
    };
    f[time_idx] = t_new;
    if id == "RMC" {
        if let Some(df) = f.get(9).cloned() {
            if df.len() == 6 {
                if let (Some(d), Some(m), Some(y)) = (
                    digits(&df.as_bytes()[0..2]),
                    digits(&df.as_bytes()[2..4]),
                    digits(&df.as_bytes()[4..6]),
                ) {
                    if (1..=12).contains(&m) && (1..=31).contains(&d) {
                        let days = days_from_civil(2000 + y, m as u32, d as u32) + carry;
                        let (yy, mm, dd) = civil_from_days(days);
                        f[9] = format!("{dd:02}{mm:02}{:02}", yy.rem_euclid(100));
                    }
                }
            }
        }
    } else if id == "ZDA" && f.len() > 4 {
        if let (Some(d), Some(m), Some(y)) = (
            digits(f[2].as_bytes()),
            digits(f[3].as_bytes()),
            digits(f[4].as_bytes()),
        ) {
            if (1..=12).contains(&m) && (1..=31).contains(&d) {
                let (yy, mm, dd) = civil_from_days(days_from_civil(y, m as u32, d as u32) + carry);
                f[2] = format!("{dd:02}");
                f[3] = format!("{mm:02}");
                f[4] = format!("{yy:04}");
            }
        }
    }
    let body = f.join(",");
    let lead = &core[..core.len() - core.trim_start().len()];
    if had_cs {
        format!("{lead}{}{cr}", nmea_sentence(&body))
    } else {
        format!("{lead}${body}{cr}")
    }
}

/// Command payload: shift top-level numeric `ts` / `expiresAt` by `delta_ms`.
/// Anything that is not a JSON object is returned unchanged.
pub fn shift_command_payload(payload: &[u8], delta_ms: i64) -> Vec<u8> {
    if delta_ms == 0 {
        return payload.to_vec();
    }
    let Ok(mut v) = serde_json::from_slice::<Value>(payload) else {
        return payload.to_vec();
    };
    let Some(obj) = v.as_object_mut() else {
        return payload.to_vec();
    };
    let mut changed = false;
    for key in ["ts", "expiresAt"] {
        if let Some(n) = obj.get(key).and_then(Value::as_i64) {
            obj.insert(key.to_string(), Value::from(n + delta_ms));
            changed = true;
        }
    }
    if !changed {
        return payload.to_vec();
    }
    serde_json::to_vec(&v).unwrap_or_else(|_| payload.to_vec())
}

/// Replace every occurrence of the tape's deviceId by the rig agent's.
pub fn substitute_id(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    if from.is_empty() {
        return bytes.to_vec();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.replace(from, to).into_bytes(),
        Err(_) => bytes.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for days in [-1_000_000i64, -1, 0, 1, 19_000, 20_725, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(iso_z(1_791_569_664), "2026-10-09T18:14:24Z");
    }

    #[test]
    fn iso_strings_shift_and_keep_fractions() {
        let t = r#"{"timestamp": "2026-10-09T18:14:24Z", "x": "2026-12-31T23:59:59.250Z", "n": "12026-10-09T18:14:24Z", "bad": "2026-13-09T18:14:24Z"}"#;
        let s = shift_iso_strings(t, 1);
        assert!(s.contains(r#""timestamp": "2026-10-09T18:14:25Z""#), "{s}");
        assert!(s.contains(r#""x": "2027-01-01T00:00:00.250Z""#), "{s}");
        assert!(s.contains(r#""n": "12026-10-09T18:14:24Z""#), "{s}");
        assert!(s.contains(r#""bad": "2026-13-09T18:14:24Z""#), "{s}");
        assert_eq!(shift_iso_strings(t, 0), t);
    }

    #[test]
    fn lte_text_shift_covers_read_at_and_cclk() {
        let t = "{\n  \"modem_clock\": {\"cclk\": \"26/10/09,14:14:06-16\", \"read_at\": 1791569647, \"reg\": 1},\n  \"gnss\": {\"fix_checked_at\": \"2026-10-09T18:11:09Z\", \"last_engine_restart\": null}\n}";
        let s = shift_lte_text(t, 86_400 + 60);
        assert!(s.contains("\"cclk\": \"26/10/10,14:15:06-16\""), "{s}");
        assert!(s.contains("\"read_at\": 1791656107"), "{s}");
        assert!(
            s.contains("\"fix_checked_at\": \"2026-10-10T18:12:09Z\""),
            "{s}"
        );
        // Everything else byte-identical.
        assert_eq!(s.len(), t.len());
        assert_eq!(
            shift_cclk("26/12/31,23:59:59+32", 1).unwrap(),
            "27/01/01,00:00:00+32"
        );
        assert_eq!(shift_cclk("garbage", 1), None);
    }

    #[test]
    fn nmea_shift_recomputes_checksums() {
        let rmc = "$GPRMC,083745.000,A,1430.1234,N,12101.5678,E,0.06,31.66,130626,,,A*5A\r";
        let s = shift_nmea_line(rmc, 16 * 3600);
        assert_eq!(
            s,
            format!(
                "{}\r",
                nmea_sentence("GPRMC,003745.000,A,1430.1234,N,12101.5678,E,0.06,31.66,140626,,,A")
            )
        );
        let gga = "$GPGGA,083745.000,1430.1234,N,12101.5678,E,1,08,0.9,545.4,M,46.9,M,,*56";
        assert_eq!(
            shift_nmea_line(gga, 15),
            nmea_sentence("GPGGA,083800.000,1430.1234,N,12101.5678,E,1,08,0.9,545.4,M,46.9,M,,")
        );
        // Unshifted kinds, bad checksums and empty times are untouched.
        for l in [
            "$GPGSA,A,3,04,05,,09,12,,,24,,,,,2.5,1.3,2.1*39",
            "$GPGGA,083745.000,1430.1234,N*00",
            "$GPRMC,,V,,,,,,,,,,N*53",
            "hello",
        ] {
            assert_eq!(shift_nmea_line(l, 99), l);
        }
        // Sentences without a checksum stay without one.
        assert_eq!(
            shift_nmea_line("$GPRMC,120000.250,A,,,,,0.0,,010126,,,A", -1),
            "$GPRMC,115959.250,A,,,,,0.0,,010126,,,A"
        );
        assert_eq!(
            shift_nmea_line("$GPRMC,000000.00,A,,,,,0.0,,010126,,,A", -1),
            "$GPRMC,235959.00,A,,,,,0.0,,311225,,,A"
        );
    }

    #[test]
    fn command_shift_and_id_substitution() {
        let p = br#"{"v":1,"requestId":"r","command":"refresh","ts":1000,"expiresAt":61000}"#;
        assert_eq!(
            shift_command_payload(p, 5),
            br#"{"v":1,"requestId":"r","command":"refresh","ts":1005,"expiresAt":61005}"#.to_vec()
        );
        assert_eq!(shift_command_payload(b"not json", 5), b"not json".to_vec());
        assert_eq!(
            substitute_id(b"xorgate/devices/abc/config", "abc", "x0a"),
            b"xorgate/devices/x0a/config".to_vec()
        );
    }
}
