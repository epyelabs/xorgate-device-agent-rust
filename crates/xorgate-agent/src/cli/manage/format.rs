//! Display formatters of the `manage` TUI over the raw `state.json` snapshot
//! (`serde_json::Value`, so either agent's file renders as the TS renders it):
//! `src/monitor/format.ts:1-34` (`leaf`, `connectionStatus`, `gauge`),
//! the `modules.tsx:47-57` value helpers (`val`, `axis`, `angle`, `coord`,
//! `fixLabel`), `lib/format.ts` (`signedFixed`, `toHexAddr`) and the ink
//! colour names.

use ratatui::style::{Color, Modifier, Style};
use serde_json::Value;
use xorgate_core::js::{number_to_string, to_fixed};

use crate::cli::status::{js_string, truthy};

/// `StatusBadge` kinds (`components/StatusBadge.tsx:3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Error,
    Unknown,
    Busy,
}

impl Status {
    /// `STYLE[status]` (`StatusBadge.tsx:5-11`): colour + glyph.
    pub fn style(self) -> (Color, &'static str) {
        match self {
            Status::Ok => (Color::Green, "●"),
            Status::Warn => (Color::Yellow, "●"),
            Status::Error => (Color::Red, "●"),
            Status::Unknown => (GRAY, "○"),
            Status::Busy => (Color::Cyan, "◍"),
        }
    }
}

/// ink `gray` = chalk `gray` (ANSI 90, bright black).
pub const GRAY: Color = Color::DarkGray;
/// ink `white` = chalk `white` (ANSI 37).
pub const WHITE: Color = Color::Gray;

/// An ink colour name → a ratatui colour (`None` = an unknown name: ink
/// prints the text uncoloured).
pub fn ink_color(name: &str) -> Option<Color> {
    Some(match name {
        "gray" | "grey" => GRAY,
        "white" => WHITE,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "red" => Color::Red,
        "cyan" => Color::Cyan,
        "magenta" => Color::Magenta,
        _ => return None,
    })
}

pub fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

pub fn gray() -> Style {
    fg(GRAY)
}

pub fn dim_gray() -> Style {
    gray().add_modifier(Modifier::DIM)
}

pub fn bold(c: Color) -> Style {
    fg(c).add_modifier(Modifier::BOLD)
}

/// `obj?.a?.b…` over a JSON value.
pub fn path<'a>(v: Option<&'a Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().try_fold(v?, |cur, k| cur.get(*k))
}

/// `val(l)` (`modules.tsx:48-49`): `typeof l?.value === "number" ? l.value : undefined`.
pub fn val(l: Option<&Value>) -> Option<f64> {
    l?.get("value")?.as_f64()
}

/// `leaf(l, dp)` (`monitor/format.ts:6-10`): `{value, unit}` → `"v unit"`,
/// `"—"` when absent; integers print as-is, others `toFixed(dp)`.
pub fn leaf(l: Option<&Value>, dp: u32) -> String {
    let Some(v) = val(l) else {
        return "—".to_string();
    };
    let s = if v.fract() == 0.0 && v.is_finite() {
        number_to_string(v)
    } else {
        to_fixed(v, dp)
    };
    let unit = l.and_then(|l| l.get("unit"));
    if truthy(unit) {
        format!("{s} {}", js_string(unit))
    } else {
        s
    }
}

/// `signedFixed(n, dp)` (`lib/format.ts:43-46`).
pub fn signed_fixed(n: f64, dp: u32) -> String {
    let v = to_fixed(n, dp);
    if v.starts_with('-') {
        v
    } else {
        format!("+{v}")
    }
}

/// `axis(l)` (`modules.tsx:50`).
pub fn axis(l: Option<&Value>) -> String {
    val(l).map_or_else(|| "—".to_string(), |v| signed_fixed(v, 3))
}

/// `angle(l)` (`modules.tsx:51`).
pub fn angle(l: Option<&Value>) -> String {
    val(l).map_or_else(|| "—".to_string(), |v| format!("{}°", signed_fixed(v, 1)))
}

/// `coord(l)` (`modules.tsx:52`).
pub fn coord(l: Option<&Value>) -> String {
    val(l).map_or_else(|| "—".to_string(), |v| to_fixed(v, 6))
}

/// `fixLabel(l)` (`modules.tsx:53-57`).
pub fn fix_label(l: Option<&Value>) -> String {
    match val(l) {
        None => "—".to_string(),
        Some(0.0) => "none".to_string(),
        Some(1.0) => "GPS".to_string(),
        Some(2.0) => "DGPS".to_string(),
        Some(v) => number_to_string(v),
    }
}

/// `toHexAddr(addr)` (`lib/format.ts:38-40`).
pub fn to_hex_addr(addr: u32) -> String {
    format!("0x{addr:02x}")
}

/// `connectionStatus(c)` (`monitor/format.ts:13-27`) — kept for parity with
/// the TS module (the TS monitor no longer calls it either).
pub fn connection_status(c: &str) -> Status {
    match c {
        "connected" => Status::Ok,
        "connecting" | "reconnecting" => Status::Busy,
        "closed" => Status::Warn,
        "error" => Status::Error,
        _ => Status::Unknown,
    }
}

/// `gauge(value, min, max)` (`monitor/format.ts:30-34`).
pub fn gauge(value: Option<f64>, min: f64, max: f64) -> f64 {
    let Some(v) = value else { return 0.0 };
    if max == min {
        return 0.0;
    }
    ((v - min) / (max - min)).clamp(0.0, 1.0)
}

/// `clock(ts)` (`modules.tsx:570-575`): local `HH:MM:SS` of an epoch-ms.
#[allow(unsafe_code)]
pub fn clock(ts: f64) -> String {
    if !ts.is_finite() {
        return "NaN:NaN:NaN".to_string();
    }
    let secs = (ts / 1000.0).floor() as libc::time_t;
    // SAFETY: localtime_r writes only into `tm`, a zeroed local.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return "NaN:NaN:NaN".to_string();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// `String.prototype.padEnd(n)` (UTF-16 length).
pub fn pad_end(s: &str, n: usize) -> String {
    let len = s.encode_utf16().count();
    format!("{s}{}", " ".repeat(n.saturating_sub(len)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn leaf_and_signed() {
        assert_eq!(leaf(Some(&json!({"value": 20, "unit": "m"})), 1), "20 m");
        assert_eq!(leaf(Some(&json!({"value": 0.5})), 1), "0.5");
        assert_eq!(
            leaf(Some(&json!({"value": 54.25, "unit": "°C"})), 1),
            "54.3 °C"
        );
        assert_eq!(leaf(None, 1), "—");
        assert_eq!(leaf(Some(&json!({"value": "x"})), 1), "—");
        assert_eq!(signed_fixed(0.92, 3), "+0.920");
        assert_eq!(signed_fixed(-0.3, 1), "-0.3");
        assert_eq!(angle(Some(&json!({"value": 23.6}))), "+23.6°");
        assert_eq!(coord(Some(&json!({"value": 37.7749}))), "37.774900");
        assert_eq!(fix_label(Some(&json!({"value": 6}))), "6");
        assert_eq!(to_hex_addr(0x4b), "0x4b");
        assert_eq!(gauge(Some(18.0), 0.0, 31.0), 18.0 / 31.0);
    }
}
