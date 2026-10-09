//! Minimal NMEA 0183 parser — 1:1 port of `src/lib/nmea.ts` (312 lines, pure).
//!
//! GGA (fix + altitude + satellites), RMC (lat/lon + speed + date/time), GSA
//! (fix type + HDOP) and GSV (satellites in view per constellation, with
//! per-satellite C/N0 — the sky-view diagnostic). The tty reader
//! (`src/hardware/gps.ts streamGps`) is HAL; this module only sees lines.
//!
//! SIM7600-specific invariants reproduced exactly (`nmea.ts:277-291`):
//! - a GSA with fixType 1 (one constellation without a fix) never clobbers
//!   `fixType`/`hdop`;
//! - GSV in-view counts are tracked PER TALKER and exposed summed;
//! - a GGA with quality 0 or a void RMC clears the position fields, so a
//!   consumer never sees a stale last-known position presented as current.
//!
//! Numbers: every field the TS parsed with `parseInt`/`parseFloat` is an `f64`
//! here (JS has one number type); integral values serialise as integers
//! ([`crate::telemetry::js_num_opt`]). A `NaN` from a malformed field is
//! represented as `None` — the only consumer (`FixAggregator::update`) skipped
//! NaN exactly as it skips `undefined`, and `JSON.stringify` prints both as
//! absent/`null`, so no behaviour is lost (see the evidence file's ambiguity list).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::js;
use crate::round::js_math_round;
use crate::telemetry::js_num_opt;

/// `GpsFix` (`nmea.ts:15-55`). Key order = the TS interface.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpsFix {
    /// Decimal degrees, north positive.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub latitude: Option<f64>,
    /// Decimal degrees, east positive.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub longitude: Option<f64>,
    /// Metres above mean sea level (GGA).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub altitude: Option<f64>,
    /// GGA fix quality: 0 = none, 1 = GPS, 2 = DGPS.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub fix_quality: Option<f64>,
    /// Satellites used in the fix (GGA).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub satellites: Option<f64>,
    /// Speed over ground in km/h (from RMC knots).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub speed_kmh: Option<f64>,
    /// Course over ground in degrees true (RMC track angle); empty while stationary.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub course: Option<f64>,
    /// RMC status: true = "A" (valid), false = "V" (void).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid: Option<bool>,
    /// UTC time HH:MM:SS as reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utc_time: Option<String>,
    /// Full UTC date+time from RMC as epoch ms (two-digit years → 2000–2099);
    /// present valid fix or not — gating is the consumer's job.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub utc_ms: Option<f64>,
    /// GSA fix type: 1 = none, 2 = 2D, 3 = 3D.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub fix_type: Option<f64>,
    /// Horizontal dilution of precision (GSA); lower = tighter geometry.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub hdop: Option<f64>,
    /// Satellites in view summed across constellations (GSV).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub satellites_in_view: Option<f64>,
}

/// JS `str.slice(a, b)` on code units; NMEA is ASCII so chars are code units.
fn slice(s: &str, start: usize, end: Option<usize>) -> String {
    let it = s.chars().skip(start);
    match end {
        Some(e) => it.take(e.saturating_sub(start)).collect(),
        None => it.collect(),
    }
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// `nmeaCoordToDecimal` (`nmea.ts:58-69`): ddmm.mmmm + hemisphere → signed degrees.
fn nmea_coord_to_decimal(value: &str, hemisphere: &str) -> Option<f64> {
    if value.is_empty() {
        return None;
    }
    let dot = value.chars().position(|c| c == '.')?;
    if dot < 3 {
        return None;
    }
    let deg_len = dot - 2; // minutes are always 2 integer digits
    let degrees = js::parse_int(&slice(value, 0, Some(deg_len)), 10);
    let minutes = js::parse_float(&slice(value, deg_len, None));
    if degrees.is_nan() || minutes.is_nan() {
        return None;
    }
    let mut dec = degrees + minutes / 60.0;
    if hemisphere == "S" || hemisphere == "W" {
        dec = -dec;
    }
    Some(dec)
}

/// `formatUtc` (`nmea.ts:71-74`): hhmmss[.sss] → "hh:mm:ss".
fn format_utc(raw: &str) -> Option<String> {
    if raw.is_empty() || char_len(raw) < 6 {
        return None;
    }
    Some(format!(
        "{}:{}:{}",
        slice(raw, 0, Some(2)),
        slice(raw, 2, Some(4)),
        slice(raw, 4, Some(6))
    ))
}

/// `parseRmcUtcMs` (`nmea.ts:80-93`): hhmmss[.sss] + ddmmyy → UTC epoch ms.
/// Field-slot corruption is caught by the bounds (ss < 61 tolerates a leap
/// second); two-digit years map to 2000+yy.
fn parse_rmc_utc_ms(time: &str, date: &str) -> Option<f64> {
    if time.is_empty() || char_len(time) < 6 || date.is_empty() || char_len(date) != 6 {
        return None;
    }
    let hh = js::number(&slice(time, 0, Some(2)));
    let mi = js::number(&slice(time, 2, Some(4)));
    let ss = js::number(&slice(time, 4, None)); // seconds incl. optional fraction
    let dd = js::number(&slice(date, 0, Some(2)));
    let mo = js::number(&slice(date, 2, Some(4)));
    let yy = js::number(&slice(date, 4, Some(6)));
    if [hh, mi, ss, dd, mo, yy].iter().any(|v| v.is_nan()) {
        return None;
    }
    if hh > 23.0 || mi > 59.0 || ss >= 61.0 || dd < 1.0 || dd > 31.0 || mo < 1.0 || mo > 12.0 {
        return None;
    }
    Some(js::date_utc(2000.0 + yy, mo - 1.0, dd, hh, mi, 0.0) + js_math_round(ss * 1000.0))
}

/// `checksumValid` (`nmea.ts:96-105`): verify `*HH` when present; a sentence
/// without one is tolerated.
pub fn checksum_valid(sentence: &str) -> bool {
    let Some(star) = sentence.chars().position(|c| c == '*') else {
        return true; // no checksum provided
    };
    let start = usize::from(sentence.starts_with('$'));
    let body = slice(sentence, start, Some(star));
    let expected = js::parse_int(&slice(sentence, star + 1, Some(star + 3)), 16);
    if expected.is_nan() {
        return false;
    }
    let mut cs: u16 = 0;
    for unit in body.encode_utf16() {
        cs ^= unit;
    }
    f64::from(cs) == expected
}

/// Split a validated sentence into its comma fields (the `*HH` tail dropped).
fn fields_of(trimmed: &str) -> Vec<String> {
    let body = match trimmed.chars().position(|c| c == '*') {
        Some(star) => slice(trimmed, 0, Some(star)),
        None => trimmed.to_string(),
    };
    body.split(',').map(str::to_string).collect()
}

/// `fields[i]` with JS semantics: a missing field reads like `undefined`,
/// which every consumer below treats exactly like `""`.
fn field(fields: &[String], i: usize) -> &str {
    fields.get(i).map(String::as_str).unwrap_or("")
}

/// `fields[i] ? parseInt(fields[i], 10) : undefined`, NaN → None.
fn opt_int(fields: &[String], i: usize) -> Option<f64> {
    let f = field(fields, i);
    if f.is_empty() {
        return None;
    }
    let v = js::parse_int(f, 10);
    (!v.is_nan()).then_some(v)
}

/// `fields[i] ? parseFloat(fields[i]) : undefined`, NaN → None.
fn opt_float(fields: &[String], i: usize) -> Option<f64> {
    let f = field(fields, i);
    if f.is_empty() {
        return None;
    }
    let v = js::parse_float(f);
    (!v.is_nan()).then_some(v)
}

/// `parseSentence` (`nmea.ts:108-154`): one sentence → a partial fix, or
/// `None` when unsupported/invalid.
pub fn parse_sentence(line: &str) -> Option<GpsFix> {
    let trimmed = js::trim(line);
    if !trimmed.starts_with('$') {
        return None;
    }
    if !checksum_valid(trimmed) {
        return None;
    }
    let fields = fields_of(trimmed);
    let kind = slice(field(&fields, 0), 3, None); // drop the $GP / $GN / $GL talker id

    if kind == "GGA" {
        return Some(GpsFix {
            utc_time: format_utc(field(&fields, 1)),
            latitude: nmea_coord_to_decimal(field(&fields, 2), field(&fields, 3)),
            longitude: nmea_coord_to_decimal(field(&fields, 4), field(&fields, 5)),
            fix_quality: opt_int(&fields, 6),
            satellites: opt_int(&fields, 7),
            altitude: opt_float(&fields, 9),
            ..GpsFix::default()
        });
    }

    if kind == "RMC" {
        let knots = opt_float(&fields, 7);
        // Field 8 = track angle / course over ground; empty when stationary.
        let course = opt_float(&fields, 8);
        return Some(GpsFix {
            utc_time: format_utc(field(&fields, 1)),
            utc_ms: parse_rmc_utc_ms(field(&fields, 1), field(&fields, 9)),
            valid: Some(field(&fields, 2) == "A"),
            latitude: nmea_coord_to_decimal(field(&fields, 3), field(&fields, 4)),
            longitude: nmea_coord_to_decimal(field(&fields, 5), field(&fields, 6)),
            speed_kmh: knots.map(|k| k * 1.852),
            course,
            ..GpsFix::default()
        });
    }

    if kind == "GSA" {
        // $GNGSA,<mode>,<fixType>,<sv x12>,<pdop>,<hdop>,<vdop>[,<systemId>]
        return Some(GpsFix {
            fix_type: opt_int(&fields, 2),
            hdop: opt_float(&fields, 16),
            ..GpsFix::default()
        });
    }

    None
}

/// `GsvSatellite` (`nmea.ts:157-172`). `snr: None` = the C/N0 field was BLANK:
/// listed from almanac but not heard — the whole point of this parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GsvSatellite {
    pub prn: i64,
    #[serde(with = "js_num_opt")]
    pub elevation: Option<f64>,
    #[serde(with = "js_num_opt")]
    pub azimuth: Option<f64>,
    #[serde(with = "js_num_opt")]
    pub snr: Option<f64>,
}

/// `GsvMessage` (`nmea.ts:175-183`): one GSV sentence of a talker's cycle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GsvMessage {
    /// Talker id without the `$` — GP, GL, BD/GB, GA, GQ …
    pub talker: String,
    pub msg_num: i64,
    pub num_msgs: i64,
    /// Satellites in view for THIS talker (repeated on every message of the cycle).
    pub in_view: i64,
    pub satellites: Vec<GsvSatellite>,
}

/// `numOrNull` (`nmea.ts:185-189`): blank/missing → None, non-finite → None.
fn num_or_null(v: Option<&str>) -> Option<f64> {
    let v = v?;
    if v.is_empty() {
        return None;
    }
    let n = js::number(v);
    n.is_finite().then_some(n)
}

/// `parseInt(field, 10)` as an integer, or None for NaN.
fn int_field(fields: &[String], i: usize) -> Option<i64> {
    let v = js::parse_int(field(fields, i), 10);
    (!v.is_nan()).then_some(v as i64)
}

/// `parseGsv` (`nmea.ts:203-227`): satellite blocks are read as complete
/// quadruples only, so the NMEA 4.10 `signalId` suffix (and the extra pair the
/// SIM7600 appends to $BDGSV) is simply left unread.
pub fn parse_gsv(line: &str) -> Option<GsvMessage> {
    let trimmed = js::trim(line);
    if !trimmed.starts_with('$') || !checksum_valid(trimmed) {
        return None;
    }
    let fields = fields_of(trimmed);
    if slice(field(&fields, 0), 3, None) != "GSV" {
        return None;
    }
    let num_msgs = int_field(&fields, 1)?;
    let msg_num = int_field(&fields, 2)?;
    let in_view = int_field(&fields, 3)?;
    let mut satellites = Vec::new();
    let mut i = 4;
    while i + 3 < fields.len() {
        let prn = js::parse_int(field(&fields, i), 10);
        // A non-numeric id means we're reading trailing fields, not a satellite.
        if !(prn.is_nan() || prn <= 0.0) {
            satellites.push(GsvSatellite {
                prn: prn as i64,
                elevation: num_or_null(fields.get(i + 1).map(String::as_str)),
                azimuth: num_or_null(fields.get(i + 2).map(String::as_str)),
                snr: num_or_null(fields.get(i + 3).map(String::as_str)),
            });
        }
        i += 4;
    }
    Some(GsvMessage {
        talker: slice(field(&fields, 0), 1, Some(3)),
        msg_num,
        num_msgs,
        in_view,
        satellites,
    })
}

/// `parseGsvInView` result (`nmea.ts:234`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GsvInView {
    pub talker: String,
    pub in_view: i64,
}

/// `parseGsvInView` (`nmea.ts:234-237`): a GSV header's talker + in-view count.
pub fn parse_gsv_in_view(line: &str) -> Option<GsvInView> {
    parse_gsv(line).map(|m| GsvInView {
        talker: m.talker,
        in_view: m.in_view,
    })
}

/// `FixAggregator` / `createFixAggregator` (`nmea.ts:239-301`): the rolling
/// multi-sentence fix. Merges only defined, non-NaN values so a sentence
/// missing a field never clobbers a good prior one — except that a GGA with
/// quality 0 or a void RMC clears the position fields.
#[derive(Debug, Clone, Default)]
pub struct FixAggregator {
    /// The current rolling fix.
    pub fix: GpsFix,
    /// GSV in-view count per talker (`inView` Map, `nmea.ts:259`).
    in_view: IndexMap<String, i64>,
}

fn merge<T: PartialEq + Clone>(dst: &mut Option<T>, src: &Option<T>, updated: &mut bool) {
    if let Some(v) = src {
        if dst.as_ref() != Some(v) {
            *updated = true;
        }
        *dst = Some(v.clone());
    }
}

impl FixAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// `update(line)` (`nmea.ts:263-298`): feed one raw line; true if the fix changed.
    pub fn update(&mut self, line: &str) -> bool {
        if let Some(gsv) = parse_gsv_in_view(line) {
            if self.in_view.get(&gsv.talker) == Some(&gsv.in_view) {
                return false;
            }
            self.in_view.insert(gsv.talker, gsv.in_view);
            let total: i64 = self.in_view.values().sum();
            self.fix.satellites_in_view = Some(total as f64);
            return true;
        }

        let Some(partial) = parse_sentence(line) else {
            return false;
        };

        // GSA comes once per constellation; one without a fix (fixType 1, empty
        // DOPs — e.g. $BDGSA,A,1 while GPS holds a solid lock) says nothing
        // about the overall solution, so don't let it clobber fixType/hdop.
        // True lock loss is signalled by GGA quality 0 / void RMC below.
        if partial.fix_type == Some(1.0) {
            return false;
        }

        let mut updated = false;
        if partial.fix_quality == Some(0.0) || partial.valid == Some(false) {
            // POSITION_KEYS (`nmea.ts:247`): fields that must not outlive the lock.
            let f = &mut self.fix;
            for slot in [
                &mut f.latitude,
                &mut f.longitude,
                &mut f.altitude,
                &mut f.speed_kmh,
                &mut f.course,
                &mut f.hdop,
            ] {
                if slot.is_some() {
                    *slot = None;
                    updated = true;
                }
            }
        }
        let f = &mut self.fix;
        merge(&mut f.latitude, &partial.latitude, &mut updated);
        merge(&mut f.longitude, &partial.longitude, &mut updated);
        merge(&mut f.altitude, &partial.altitude, &mut updated);
        merge(&mut f.fix_quality, &partial.fix_quality, &mut updated);
        merge(&mut f.satellites, &partial.satellites, &mut updated);
        merge(&mut f.speed_kmh, &partial.speed_kmh, &mut updated);
        merge(&mut f.course, &partial.course, &mut updated);
        merge(&mut f.valid, &partial.valid, &mut updated);
        merge(&mut f.utc_time, &partial.utc_time, &mut updated);
        merge(&mut f.utc_ms, &partial.utc_ms, &mut updated);
        merge(&mut f.fix_type, &partial.fix_type, &mut updated);
        merge(&mut f.hdop, &partial.hdop, &mut updated);
        merge(
            &mut f.satellites_in_view,
            &partial.satellites_in_view,
            &mut updated,
        );
        updated
    }
}

/// `aggregateFix` (`nmea.ts:304-308`): merge a burst of sentences into one fix.
pub fn aggregate_fix<S: AsRef<str>>(lines: &[S]) -> GpsFix {
    let mut agg = FixAggregator::new();
    for line in lines {
        agg.update(line.as_ref());
    }
    agg.fix
}

/// `hasPosition` (`nmea.ts:310-312`).
pub fn has_position(fix: &GpsFix) -> bool {
    fix.latitude.is_some() && fix.longitude.is_some()
}

/// A GSV satellite row's numeric fields, for callers building test data.
impl GsvSatellite {
    pub fn new(prn: i64, elevation: Option<f64>, azimuth: Option<f64>, snr: Option<f64>) -> Self {
        Self {
            prn,
            elevation,
            azimuth,
            snr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_fields_read_as_absent() {
        // TS: {fixQuality: NaN, …}; the aggregator skips NaN, JSON prints null.
        let f = parse_sentence("$GPGGA,0837,x,N,y,E,q,s,,alt,M,,M,,").unwrap();
        assert_eq!(f.fix_quality, None);
        assert_eq!(f.satellites, None);
        assert_eq!(f.altitude, None);
        assert_eq!(f.utc_time, None);
        assert_eq!(f.latitude, None);
    }

    #[test]
    fn gps_fix_serialises_like_json_stringify() {
        let f = GpsFix {
            latitude: Some(14.502056666666666),
            fix_quality: Some(1.0),
            satellites: Some(8.0),
            valid: Some(true),
            utc_time: Some("08:37:45".into()),
            ..GpsFix::default()
        };
        assert_eq!(
            serde_json::to_string(&f).unwrap(),
            r#"{"latitude":14.502056666666666,"fixQuality":1,"satellites":8,"valid":true,"utcTime":"08:37:45"}"#
        );
        let sat = GsvSatellite::new(21, None, None, Some(21.0));
        assert_eq!(
            serde_json::to_string(&sat).unwrap(),
            r#"{"prn":21,"elevation":null,"azimuth":null,"snr":21}"#
        );
    }
}
