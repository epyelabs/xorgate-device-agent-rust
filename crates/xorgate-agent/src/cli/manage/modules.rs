//! The telemetry "modules" of the master-detail monitor — port of
//! `src/monitor/modules.tsx:1-610`: each has a one-line sidebar status and a
//! detail view, pure renderers of the daemon's `state.json` snapshot (plus
//! the per-pane probes the LTE/IMU details run while open).
//!
//! JSX text rules reproduced: a `{child}` that is `null`/`undefined`/a
//! boolean renders nothing ([`jsx`]); a template literal prints
//! `String(v)` ([`js_string`]); whitespace-only lines between JSX children
//! collapse, a line break inside text becomes one space.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use xorgate_core::imu::{ImuDetect, IMU_ADDRESSES, IMU_I2C_BUS};
use xorgate_core::js::{number_to_string, to_fixed};
use xorgate_core::round::js_math_round;
use xorgate_hal::network_io::WifiPresence;

use super::format::{
    angle, axis, bold, clock, coord, dim_gray, fg, fix_label, gauge, gray, leaf, pad_end, path,
    to_hex_addr, val, Status, GRAY, WHITE,
};
use crate::cli::status::{js_string, truthy};

/// The sidebar order = navigation order (`modules.tsx:610`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleId {
    Activity,
    System,
    Lte,
    Gps,
    Imu,
    /// Phase 5b: on EVERY host (`modules.tsx:731` @ 09da38d), like the TS.
    Obd,
    Media,
    Settings,
}

/// `MODULES` (`modules.tsx:731` @ 09da38d): activity, system, lte, gps, imu,
/// obd, media, settings.
pub const MODULES: [ModuleId; 8] = [
    ModuleId::Activity,
    ModuleId::System,
    ModuleId::Lte,
    ModuleId::Gps,
    ModuleId::Imu,
    ModuleId::Obd,
    ModuleId::Media,
    ModuleId::Settings,
];

impl ModuleId {
    /// `Module.title`.
    pub fn title(self) -> &'static str {
        match self {
            ModuleId::Activity => "Activity",
            ModuleId::System => "System",
            ModuleId::Lte => "LTE",
            ModuleId::Gps => "GPS",
            ModuleId::Imu => "IMU",
            ModuleId::Obd => "OBD",
            ModuleId::Media => "Media",
            ModuleId::Settings => "Settings",
        }
    }
}

/// `{child}` in JSX: `null`/`undefined`/booleans render nothing.
pub fn jsx(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(_)) => String::new(),
        other => js_string(other),
    }
}

/// `UNKNOWN` (`modules.tsx:47`).
fn unknown() -> (String, Status) {
    ("…".to_string(), Status::Unknown)
}

fn metrics<'a>(s: Option<&'a Value>, group: &str) -> Option<&'a Value> {
    path(s, &["latest", "metrics", group])
}

/// `module.status(snapshot)` (`modules.tsx:159-607`, the `status:` fields).
pub fn module_status(id: ModuleId, s: Option<&Value>) -> (String, Status) {
    match id {
        ModuleId::Imu => match val(path(metrics(s, "imu"), &["euler_yaw"])) {
            None => unknown(),
            Some(yaw) => (format!("yaw {}°", to_fixed(yaw, 0)), Status::Ok),
        },
        ModuleId::Gps => {
            let g = metrics(s, "gps");
            match (val(path(g, &["lat"])), val(path(g, &["lon"]))) {
                (Some(lat), Some(lon)) => (
                    format!("{},{}", to_fixed(lat, 2), to_fixed(lon, 2)),
                    Status::Ok,
                ),
                _ if truthy(g) => ("acquiring".to_string(), Status::Warn),
                _ => unknown(),
            }
        }
        ModuleId::Lte => {
            let lte = metrics(s, "lte");
            let Some(rssi) = val(path(lte, &["rssi"])) else {
                return unknown();
            };
            let status = match val(path(lte, &["quality"])) {
                None => Status::Unknown,
                Some(q) if q >= 15.0 => Status::Ok,
                Some(q) if q >= 10.0 => Status::Warn,
                Some(_) => Status::Error,
            };
            (format!("rssi {}", number_to_string(rssi)), status)
        }
        ModuleId::System => match val(path(metrics(s, "system"), &["cpu_usage"])) {
            None => unknown(),
            Some(cpu) => {
                let status = if cpu < 70.0 {
                    Status::Ok
                } else if cpu < 90.0 {
                    Status::Warn
                } else {
                    Status::Error
                };
                (
                    format!("cpu {}%", number_to_string(js_math_round(cpu))),
                    status,
                )
            }
        },
        ModuleId::Obd => {
            // `obd.status` (`modules.tsx:494-502` @ 09da38d).
            let o = path(s, &["obd"]);
            let m = metrics(s, "obd");
            if !truthy(o) {
                return match val(path(m, &["link"])) {
                    None => unknown(),
                    Some(link) => (
                        format!("link {}", number_to_string(link)),
                        if link == 2.0 {
                            Status::Ok
                        } else {
                            Status::Warn
                        },
                    ),
                };
            }
            let state = path(o, &["state"]).and_then(Value::as_str);
            let label = match (state, val(path(m, &["rpm"]))) {
                (Some("live"), Some(rpm)) => {
                    format!("{} rpm", number_to_string(js_math_round(rpm)))
                }
                _ => format!(
                    "{} · link {}",
                    js_string(path(o, &["state"])),
                    js_string(path(o, &["link"]))
                ),
            };
            (label, obd_badge(o))
        }
        ModuleId::Media => {
            let md = path(s, &["latest", "media"]);
            if !truthy(md) {
                return unknown();
            }
            let streams = streams_of(md);
            if streams.is_empty() {
                let live = truthy(path(md, &["kvs", "value"]))
                    || truthy(path(md, &["recording", "value"]));
                return if live {
                    ("live".to_string(), Status::Ok)
                } else {
                    ("idle".to_string(), Status::Unknown)
                };
            }
            let state = |x: &Value| x.get("state").and_then(Value::as_str).map(str::to_string);
            let running = streams
                .iter()
                .filter(|x| state(x).as_deref() == Some("running"))
                .count();
            let failed = streams
                .iter()
                .any(|x| state(x).as_deref() == Some("failed"));
            if running > 0 {
                (format!("{running}/{} live", streams.len()), Status::Ok)
            } else if failed {
                ("failed".to_string(), Status::Error)
            } else {
                ("starting".to_string(), Status::Warn)
            }
        }
        ModuleId::Settings => {
            let mode = path(s, &["mode"]);
            if truthy(mode) {
                (jsx(mode), Status::Ok)
            } else {
                unknown()
            }
        }
        ModuleId::Activity => {
            let events = events_of(s);
            let Some(last) = events.last() else {
                return unknown();
            };
            let status = match last.get("level").and_then(Value::as_str) {
                Some("error") => Status::Error,
                Some("warn") => Status::Warn,
                _ => Status::Ok,
            };
            (format!("{} ev", events.len()), status)
        }
    }
}

fn streams_of(md: Option<&Value>) -> Vec<Value> {
    path(md, &["streams"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn events_of(s: Option<&Value>) -> Vec<Value> {
    path(s, &["recentEvents"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `ModemPresence` (`hardware/modem.ts`): what `detectModem()` found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModemPresence {
    pub present: bool,
    pub usb_id: Option<String>,
    pub description: Option<String>,
    pub tty_ports: Vec<String>,
}

/// The Settings pane's props (`SettingsDetailProps`, `modules.tsx:460-470`).
#[derive(Debug, Clone, Default)]
pub struct SettingsView {
    pub mode_choice: Option<String>,
    pub row: usize,
    pub operating: bool,
    pub wifi: Option<WifiPresence>,
    pub wifi_busy: bool,
    pub wifi_confirm: bool,
}

/// Everything a detail view reads.
pub struct DetailCtx<'a> {
    pub snapshot: Option<&'a Value>,
    /// The LTE/IMU pane probes (`None` = still probing).
    pub imu_detect: Option<&'a ImuDetect>,
    pub modem: Option<&'a ModemPresence>,
    /// `readModemTelemetry()` (`None` = not read yet / null).
    pub link: Option<&'a Value>,
    pub settings: &'a SettingsView,
    /// The ink-spinner `dots` frame to draw.
    pub spinner: &'a str,
    /// `GPS_NMEA_PORT` (`hardware/gps.ts:15`).
    pub gps_port: &'a str,
}

fn title(t: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(t.to_string(), bold(Color::Cyan))),
        Line::default(),
    ]
}

fn no_data() -> Line<'static> {
    Line::from(Span::styled("no data yet", gray()))
}

fn s(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

fn plain(text: impl Into<String>) -> Span<'static> {
    Span::raw(text.into())
}

/// `LevelMeter` (`components/LevelMeter.tsx:7-48`), no peak.
pub fn level_meter(level: f64, width: usize, label: &str) -> Line<'static> {
    let clamped = level.clamp(0.0, 1.0);
    let filled = js_math_round(clamped * width as f64) as usize;
    let mut spans = vec![s(format!("{label} "), fg(WHITE)), plain("[")];
    for i in 0..width {
        let frac = i as f64 / width as f64;
        let c = if frac > 0.85 {
            Color::Red
        } else if frac > 0.6 {
            Color::Yellow
        } else {
            Color::Green
        };
        if i < filled {
            spans.push(s("█", fg(c)));
        } else {
            spans.push(s(" ", gray()));
        }
    }
    spans.push(plain("] "));
    spans.push(s(
        format!("{}%", number_to_string(js_math_round(clamped * 100.0))),
        gray(),
    ));
    Line::from(spans)
}

/// The detail lines of module `id` (`Detail` of each module).
pub fn detail_lines(id: ModuleId, c: &DetailCtx) -> Vec<Line<'static>> {
    match id {
        ModuleId::Imu => imu_detail(c),
        ModuleId::Gps => gps_detail(c),
        ModuleId::Lte => lte_detail(c),
        ModuleId::System => system_detail(c),
        ModuleId::Obd => obd_detail(c),
        ModuleId::Media => media_detail(c),
        ModuleId::Settings => settings_detail(c),
        ModuleId::Activity => activity_detail(c),
    }
}

fn is_real(snapshot: Option<&Value>) -> bool {
    path(snapshot, &["mode"]).and_then(Value::as_str) == Some("real")
}

/// `ImuDetail` (`modules.tsx:93-157`).
fn imu_detail(c: &DetailCtx) -> Vec<Line<'static>> {
    let m = metrics(c.snapshot, "imu");
    let real = is_real(c.snapshot);
    let addr_list = IMU_ADDRESSES
        .iter()
        .map(|a| to_hex_addr(*a))
        .collect::<Vec<_>>()
        .join(" / ");
    let mut out = title("IMU · orientation + acceleration");
    if real {
        out.push(Line::from(s(
            "Sensor",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        out.push(match c.imu_detect {
            None => Line::from(format!("{} detecting (i2cdetect)…", c.spinner)),
            Some(d) if !d.available => Line::from(s(
                format!("⚠ {}", d.reason.clone().unwrap_or_default()),
                fg(Color::Red),
            )),
            Some(d) if d.imu_present && d.imu_address.is_some() => Line::from(vec![
                s("● ", fg(Color::Green)),
                plain(format!(
                    "BNO085 @ {addr_list} — present (found at {})",
                    to_hex_addr(d.imu_address.unwrap_or_default())
                )),
            ]),
            Some(_) => Line::from(s(
                format!("● BNO085 @ {addr_list} — not detected on I2C bus {IMU_I2C_BUS}"),
                fg(Color::Red),
            )),
        });
        out.push(Line::default());
    }
    if truthy(m) {
        let g = |k: &str| path(m, &[k]);
        out.push(Line::from(vec![
            s("quat ", gray()),
            plain(format!(
                "r {} i {} j {} k {}",
                axis(g("quat_r")),
                axis(g("quat_i")),
                axis(g("quat_j")),
                axis(g("quat_k"))
            )),
        ]));
        out.push(Line::from(vec![
            s("euler ", gray()),
            plain(format!(
                "roll {} pitch {} yaw {}",
                angle(g("euler_roll")),
                angle(g("euler_pitch")),
                angle(g("euler_yaw"))
            )),
        ]));
        out.push(Line::from(vec![
            s("accel ", gray()),
            plain(format!(
                "x {} y {} z {} ",
                axis(g("accel_x")),
                axis(g("accel_y")),
                axis(g("accel_z"))
            )),
            s("m/s²", gray()),
        ]));
    } else if real {
        out.push(Line::from(s("no samples yet", gray())));
    } else {
        out.push(no_data());
    }
    out
}

/// `gps.Detail` (`modules.tsx:183-224`).
fn gps_detail(c: &DetailCtx) -> Vec<Line<'static>> {
    let m = metrics(c.snapshot, "gps");
    let g = |k: &str| path(m, &[k]);
    let has_fix = val(g("lat")).is_some() && val(g("lon")).is_some();
    let mut out = title("GPS · position");
    out.push(Line::from(s(format!("NMEA on {}", c.gps_port), gray())));
    if has_fix {
        out.push(Line::from(vec![s("lat ", gray()), plain(coord(g("lat")))]));
        out.push(Line::from(vec![s("lon ", gray()), plain(coord(g("lon")))]));
        out.push(Line::from(s(
            format!("alt {} · speed {}", leaf(g("alt"), 1), leaf(g("speed"), 1)),
            gray(),
        )));
        let ttff = if val(g("ttff_s")).is_some() {
            format!(" · ttff {}s", leaf(g("ttff_s"), 1))
        } else {
            String::new()
        };
        out.push(Line::from(s(
            format!(
                "sats {}/{} in view · hdop {} · fix {}{ttff}",
                leaf(g("satellites"), 0),
                leaf(g("satellites_in_view"), 0),
                leaf(g("hdop"), 1),
                fix_label(g("fix_quality"))
            ),
            gray(),
        )));
    } else if truthy(m) {
        out.push(Line::from(s(
            "○ Receiving NMEA but no position fix yet.",
            fg(Color::Yellow),
        )));
        out.push(Line::from(s(
            format!(
                "sats {}/{} in view · waiting for satellite lock.",
                leaf(g("satellites"), 0),
                leaf(g("satellites_in_view"), 0)
            ),
            gray(),
        )));
    } else {
        out.push(Line::from(s(
            "○ no NMEA — waiting for GNSS stream.",
            gray(),
        )));
    }
    out
}

/// `LINK_COLOR` (`modules.tsx:59-64`) `?? "white"`.
fn link_color(status: &str) -> Color {
    match status {
        "connected" => Color::Green,
        "connecting" | "disconnected" => Color::Yellow,
        "dead_zone" => Color::Red,
        _ => WHITE,
    }
}

/// `GNSS_HEALTH_COLOR` (`modules.tsx:66-71`) `?? "gray"`.
fn gnss_health_color(h: &str) -> Color {
    match h {
        "ok" => Color::Green,
        "acquiring" | "degraded" => Color::Yellow,
        "unhealthy" => Color::Red,
        _ => GRAY,
    }
}

/// `LteDetail` (`modules.tsx:234-341`).
fn lte_detail(c: &DetailCtx) -> Vec<Line<'static>> {
    let m = metrics(c.snapshot, "lte");
    let real = is_real(c.snapshot);
    let link = c.link.filter(|l| truthy(Some(l)));
    let mut out = title("LTE · cellular signal");
    if real {
        out.push(Line::from(s(
            "Modem",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        match c.modem {
            None => out.push(Line::from(format!("{} detecting (lsusb)…", c.spinner))),
            Some(md) if md.present => {
                let mut spans = vec![
                    s("● ", fg(Color::Green)),
                    plain(format!(
                        "Present{}",
                        md.usb_id
                            .as_deref()
                            .filter(|u| !u.is_empty())
                            .map(|u| format!(" — {u}"))
                            .unwrap_or_default()
                    )),
                ];
                if let Some(d) = md.description.as_deref().filter(|d| !d.is_empty()) {
                    spans.push(s(format!(" {d}"), gray()));
                }
                out.push(Line::from(spans));
                let ports = if md.tty_ports.is_empty() {
                    "none".to_string()
                } else {
                    md.tty_ports.join(" ")
                };
                out.push(Line::from(s(format!("ports: {ports}"), gray())));
            }
            Some(md) => {
                out.push(Line::from(s("● not detected on USB", fg(Color::Red))));
                if !md.tty_ports.is_empty() {
                    out.push(Line::from(s(
                        format!("ports: {}", md.tty_ports.join(" ")),
                        gray(),
                    )));
                }
            }
        }
        out.push(Line::default());
    }
    let mut head = vec![s(
        "Signal & link",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if link.is_some() {
        head.push(s(" (live from daemon)", gray()));
    }
    out.push(Line::from(head));
    if let Some(l) = link {
        let st = l.get("status");
        out.push(Line::from(s(
            format!("● {}", jsx(st)),
            fg(link_color(st.and_then(Value::as_str).unwrap_or(""))),
        )));
    }
    if truthy(m) {
        out.push(Line::from(vec![
            s("rssi ", gray()),
            plain(leaf(path(m, &["rssi"]), 0)),
            s(" · quality ", gray()),
            plain(leaf(path(m, &["quality"]), 0)),
        ]));
        out.push(Line::default());
        out.push(level_meter(
            gauge(val(path(m, &["quality"])), 0.0, 31.0),
            24,
            "signal",
        ));
    } else if link.is_some() {
        out.push(Line::from(s("waiting for signal…", gray())));
    } else if real {
        out.push(Line::from(s("no telemetry yet", gray())));
    } else {
        out.push(no_data());
    }
    if let Some(l) = link {
        let ip = match l.get("ip") {
            None | Some(Value::Null) => "—".to_string(),
            v => js_string(v),
        };
        out.push(Line::from(s(
            format!(
                "{} · ip {ip} · {}",
                js_string(l.get("iface")),
                js_string(l.get("timestamp"))
            ),
            gray(),
        )));
        let gnss = l.get("gnss");
        let recovery = l.get("recovery");
        if truthy(gnss) || truthy(recovery) {
            out.push(Line::default());
            let health = match path(gnss, &["health"]) {
                None | Some(Value::Null) => Value::String("unknown".into()),
                Some(v) => v.clone(),
            };
            let color = gnss_health_color(
                path(gnss, &["health"])
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            let mut tail = String::new();
            if let Some(t) = path(gnss, &["ttff_s"]).filter(|v| v.is_number()) {
                tail.push_str(&format!(" · ttff {}s", js_string(Some(t))));
            }
            if truthy(path(gnss, &["standalone_fallback"])) {
                tail.push_str(" · standalone fallback");
            }
            out.push(Line::from(vec![
                s("gnss ", gray()),
                s(jsx(Some(&health)), fg(color)),
                s(tail, gray()),
            ]));
            let resets = match path(recovery, &["resets_24h"]) {
                None | Some(Value::Null) => "0".to_string(),
                v => jsx(v),
            };
            let last = path(recovery, &["last_reset"]);
            let last_s = if truthy(last) {
                format!(
                    " · last {} @ {}",
                    js_string(path(last, &["path"])),
                    js_string(path(last, &["ts"]))
                )
            } else {
                " · no reset this run".to_string()
            };
            out.push(Line::from(s(
                format!("resets 24h {resets}{last_s}"),
                gray(),
            )));
        }
    }
    out
}

/// `system.Detail` (`modules.tsx:365-382`).
fn system_detail(c: &DetailCtx) -> Vec<Line<'static>> {
    let m = metrics(c.snapshot, "system");
    let mut out = title("System · device health");
    if truthy(m) {
        let g = |k: &str| val(path(m, &[k]));
        out.push(level_meter(gauge(g("cpu_usage"), 0.0, 100.0), 24, "cpu "));
        out.push(level_meter(gauge(g("ram_usage"), 0.0, 100.0), 24, "ram "));
        out.push(level_meter(gauge(g("disk_usage"), 0.0, 100.0), 24, "disk"));
        out.push(Line::from(s(
            format!("temp {}", leaf(path(m, &["cpu_temp"]), 1)),
            gray(),
        )));
    } else {
        out.push(no_data());
    }
    out
}

/// `obdBadge(obd)` (`modules.tsx:387-408` @ 09da38d): collector state → badge.
fn obd_badge(o: Option<&Value>) -> Status {
    if path(o, &["forbidden"])
        .and_then(Value::as_f64)
        .is_some_and(|f| f > 0.0)
    {
        return Status::Error;
    }
    match path(o, &["state"]).and_then(Value::as_str) {
        Some("live") => Status::Ok,
        Some("connecting" | "pairing" | "probing" | "starting") => Status::Busy,
        Some("ecu_silent" | "backoff" | "retrying" | "unconfigured") => Status::Warn,
        Some("parked" | "stopped" | "adapter_not_found") => Status::Error,
        _ => Status::Unknown,
    }
}

/// `OBD_BADGE_COLOR` (`modules.tsx:409-415` @ 09da38d).
fn obd_badge_color(st: Status) -> Color {
    match st {
        Status::Ok => Color::Green,
        Status::Busy | Status::Warn => Color::Yellow,
        Status::Error => Color::Red,
        Status::Unknown => GRAY,
    }
}

/// `LINK_LABEL` (`modules.tsx:416` @ 09da38d).
fn obd_link_label(link: Option<f64>) -> &'static str {
    match link.and_then(xorgate_core::obd::obd_link) {
        Some(0) => "no adapter",
        Some(1) => "adapter only",
        Some(2) => "ECU answering",
        _ => "?",
    }
}

/// `ObdDetail` (`modules.tsx:418-490` @ 09da38d).
fn obd_detail(c: &DetailCtx) -> Vec<Line<'static>> {
    let obd = path(c.snapshot, &["obd"]).filter(|o| truthy(Some(o)));
    let m = metrics(c.snapshot, "obd").filter(|m| truthy(Some(m)));
    let mut out = title("OBD2 · vehicle (ELM327 over Bluetooth)");
    if let Some(o) = obd {
        let g = |k: &str| o.get(k);
        let mut head = vec![
            s("● ", fg(obd_badge_color(obd_badge(Some(o))))),
            plain(jsx(g("state"))),
        ];
        if truthy(g("detail")) {
            head.push(s(format!(" — {}", jsx(g("detail"))), gray()));
        }
        out.push(truncate_marker(Line::from(head)));
        let link = g("link").and_then(Value::as_f64);
        let rate = match g("reqPerSec").and_then(Value::as_f64) {
            None => "—".to_string(),
            Some(r) => format!("{} req/s", to_fixed(r, 1)),
        };
        let forbidden = g("forbidden").and_then(Value::as_f64).unwrap_or(0.0);
        let helper = match g("helperVersion") {
            None | Some(Value::Null) => "—".to_string(),
            v => jsx(v),
        };
        out.push(truncate_marker(Line::from(vec![
            s("link ", gray()),
            plain(format!("{} ({})", jsx(g("link")), obd_link_label(link))),
            s("   rate ", gray()),
            plain(rate),
            s("   err ", gray()),
            plain(jsx(g("errors"))),
            s("   forbidden ", gray()),
            s(
                jsx(g("forbidden")),
                if forbidden > 0.0 {
                    fg(Color::Red)
                } else {
                    Style::default()
                },
            ),
            s("   helper ", gray()),
            plain(helper),
        ])));
        out.push(Line::default()); // marginBottom={1}
    }
    if let Some(m) = m {
        let l = |k: &str, dp: u32| leaf(path(Some(m), &[k]), dp);
        let row = |label: &str, text: String| {
            truncate_marker(Line::from(vec![s(label.to_string(), gray()), plain(text)]))
        };
        out.push(row(
            "engine ",
            format!(
                "{} · load {} · throttle {} · run {}",
                l("rpm", 0),
                l("engine_load", 1),
                l("throttle", 1),
                l("run_time", 0)
            ),
        ));
        out.push(row(
            "motion ",
            format!(
                "{} · gear {} · odometer {}",
                l("speed", 0),
                l("gear", 0),
                l("odometer", 1)
            ),
        ));
        out.push(row(
            "temps  ",
            format!(
                "coolant {} · intake {} · ambient {}",
                l("coolant_temp", 0),
                l("intake_temp", 0),
                l("ambient_temp", 0)
            ),
        ));
        out.push(row(
            "torque ",
            format!(
                "{} · {} of {}",
                l("engine_torque", 1),
                l("engine_torque_pct", 0),
                l("reference_torque", 0)
            ),
        ));
        out.push(row(
            "fuel   ",
            format!("{} · rate {}", l("fuel_level", 1), l("fuel_rate", 2)),
        ));
        out.push(row(
            "power  ",
            format!(
                "module {} · adapter {}",
                l("module_voltage", 2),
                l("adapter_voltage", 2)
            ),
        ));
        out.push(row(
            "dtc    ",
            format!(
                "mil {} · count {} · since clear {}",
                l("mil", 0),
                l("dtc_count", 0),
                l("distance_since_clear", 0)
            ),
        ));
    } else if obd.is_some() {
        out.push(Line::from(s("no vehicle samples yet", gray())));
    } else {
        out.push(no_data());
    }
    out
}

/// `STREAM_STATE_COLOR` (`modules.tsx:386-395`).
fn stream_color(state: &str) -> Option<Color> {
    Some(match state {
        "starting" | "retrying" => Color::Yellow,
        "running" => Color::Green,
        "failed" => Color::Red,
        "stopped" => GRAY,
        "no-camera" | "no-pipeline" => Color::Magenta,
        _ => return None,
    })
}

fn opt_fg(c: Option<Color>) -> Style {
    c.map(fg).unwrap_or_default()
}

/// `media.Detail` (`modules.tsx:413-446`).
fn media_detail(c: &DetailCtx) -> Vec<Line<'static>> {
    let md = path(c.snapshot, &["latest", "media"]);
    let mut out = title("Media · KVS WebRTC");
    if !truthy(md) {
        out.push(no_data());
        return out;
    }
    let onoff = |v: Option<&Value>| {
        if truthy(v) {
            s("on", fg(Color::Green))
        } else {
            s("off", gray())
        }
    };
    out.push(Line::from(vec![
        s("KVS ", gray()),
        onoff(path(md, &["kvs", "value"])),
        s(" · recording ", gray()),
        onoff(path(md, &["recording", "value"])),
    ]));
    let streams = streams_of(md);
    if streams.is_empty() {
        out.push(Line::from(s(
            "no channels (run `xorgate set media on`, then check kvs.json)",
            dim_gray(),
        )));
    }
    for st in &streams {
        let state = st.get("state");
        let color = opt_fg(stream_color(state.and_then(Value::as_str).unwrap_or("")));
        out.push(truncate_marker(Line::from(vec![
            s("● ", color),
            s(jsx(st.get("streamKey")), fg(WHITE)),
            s(format!(" {} ", jsx(st.get("channelName"))), gray()),
            s(jsx(state), color),
        ])));
    }
    out
}

/// Settings detail (`SettingsDetail`, `modules.tsx:472-523`) + `WifiRow`
/// (`:526-555`). Q10 (D-row): the mode row's apply saves settings.json and
/// says how to restart (`sudo systemctl restart xorgate-agent-rs`) instead of
/// restarting a self-managed daemon, so the hint reads `↵ save (restart to
/// apply)` where the TS reads `↵ apply & restart`.
fn settings_detail(c: &DetailCtx) -> Vec<Line<'static>> {
    let v = c.settings;
    let cur_mode = match path(c.snapshot, &["mode"]) {
        None | Some(Value::Null) => "sim".to_string(),
        m => jsx(m),
    };
    let val_mode = if v.operating {
        v.mode_choice.clone().unwrap_or_else(|| cur_mode.clone())
    } else {
        cur_mode.clone()
    };
    let changed = val_mode != cur_mode;
    let mut out = title("Settings");
    out.push(Line::from(s(
        "Run mode (sim/real) and the WiFi radio.",
        gray(),
    )));
    out.push(Line::default());
    // rows = [run mode]; rows.length = 1 = the wifi row's index.
    let focused = v.operating && v.row == 0;
    let row_style = fg(if focused { Color::Cyan } else { WHITE });
    let mut spans = vec![
        s(if focused { "▶ " } else { "  " }, row_style),
        s(pad_end("run mode", 9), gray()),
        s(format!(" {val_mode}"), row_style),
    ];
    if changed {
        spans.push(s(" *", fg(Color::Yellow).add_modifier(Modifier::DIM)));
    }
    out.push(Line::from(spans));
    out.push(wifi_row(v, v.operating && v.row == 1));
    out.push(Line::default());
    let hint = if v.operating {
        if v.row == 1 {
            "↑↓ field · space/↵ toggle wifi · ←/q back"
        } else {
            "↑↓ field · space toggle · ↵ save (restart to apply) · ←/q back"
        }
    } else {
        "↵ to edit settings"
    };
    out.push(Line::from(s(hint, dim_gray())));
    out
}

/// `WifiRow` (`modules.tsx:526-555`).
fn wifi_row(v: &SettingsView, focused: bool) -> Line<'static> {
    if let Some(w) = v.wifi.as_ref().filter(|w| !w.present) {
        let _ = w;
        return Line::from(s(
            format!("  {} — not present", pad_end("wifi", 9)),
            dim_gray(),
        ));
    }
    let value = match &v.wifi {
        None => "…",
        Some(_) if v.wifi_busy => "applying…",
        Some(w) if w.enabled => "on",
        Some(_) => "off",
    };
    let row_style = fg(if focused { Color::Cyan } else { WHITE });
    let mut spans = vec![
        s(if focused { "▶ " } else { "  " }, row_style),
        s(pad_end("wifi", 9), gray()),
        s(format!(" {value}"), row_style),
    ];
    if v.wifi_confirm {
        spans.push(s(" · press again to confirm cutoff", fg(Color::Yellow)));
    }
    Line::from(spans)
}

/// `LEVEL_COLOR` (`modules.tsx:564-568`).
fn level_color(level: &str) -> Option<Color> {
    match level {
        "info" => Some(GRAY),
        "warn" => Some(Color::Yellow),
        "error" => Some(Color::Red),
        _ => None,
    }
}

/// `activity.Detail` (`modules.tsx:587-606`): the last 14 events.
fn activity_detail(c: &DetailCtx) -> Vec<Line<'static>> {
    let events = events_of(c.snapshot);
    let tail = &events[events.len().saturating_sub(14)..];
    let mut out = title("Activity · recent events");
    if tail.is_empty() {
        out.push(Line::from(s("(no events yet)", dim_gray())));
    }
    for e in tail {
        let ts = e.get("ts").and_then(Value::as_f64).unwrap_or(f64::NAN);
        let color = level_color(e.get("level").and_then(Value::as_str).unwrap_or(""));
        out.push(truncate_marker(Line::from(vec![
            s(format!("{} ", clock(ts)), gray()),
            s(jsx(e.get("msg")), opt_fg(color)),
        ])));
    }
    out
}

/// Marks a line as `wrap="truncate-end"` (the view clips it with `…`
/// instead of wrapping) — carried in the line's alignment-free style slot.
pub fn truncate_marker(mut l: Line<'static>) -> Line<'static> {
    l.style = l.style.add_modifier(TRUNCATE);
    l
}

/// The marker modifier [`truncate_marker`] sets on a line (never drawn: the
/// view strips it). `HIDDEN` is unused by every TS text, so it is free.
pub const TRUNCATE: Modifier = Modifier::HIDDEN;
