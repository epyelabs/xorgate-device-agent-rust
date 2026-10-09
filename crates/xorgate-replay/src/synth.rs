//! `xorgate-replay synth` — deterministic tapes built from fixtures, so WP2/WP3/
//! WP4 can drive scenario tests and the rig can calibrate before a bench tape
//! exists (wp-specific-briefs.md WP7 "A `synth` subcommand …").
//!
//! Every synth tape is byte-reproducible: a fixed start wall time
//! ([`SYNTH_START_WALL`], the `now` of `fixtures/modem/telemetry-tolerance.json`),
//! no randomness, gzip with a zero mtime. Content is labelled in
//! `meta.synthetic` (O8 "labelled `synthetic`"). Coordinates are the fixture
//! ones (`fixtures/nmea/nmea.json`), never a bench location.
//!
//! Tracks (combined by the scenarios in [`SCENARIOS`]):
//! - `nmea`        1 Hz SIM7600-shaped bursts: no fix, then a fix that drifts;
//! - `nmea-silence` a 35 s silent gap (C12 reopen watchdog: > 30 s);
//! - `lte`         a realistic daemon snapshot rewritten every 10 s, with a 15 s
//!   absent span (group omitted + one warning);
//! - `lte-fixtures` every `raw` document of `fixtures/modem/telemetry-tolerance.json`
//!   for 3 s each;
//! - `exec`        `vcgencmd get_throttled` (an undervoltage blip) + `pmic_read_adc`;
//! - `dt`          the PSU limit (3000 mA, the bench's value);
//! - `imu`         i2cdetect (0x4a) → helper spawn → `ready` → 20 Hz samples, one
//!   helper exit (code 4, after an `error` line) and a respawn;
//! - `config`      retained config rev 1 (recording off) and rev 2 (timeSync +
//!   imuMount change), redelivered after every `conn` drop;
//! - `conn-drop`   one 20 s `conn closed` span;
//! - `commands`    refresh, refresh again < 30 s (rate-limited), an expired one,
//!   a duplicate requestId;
//! - `commands-sudo` update + reboot (sudo shim argv; O3 differs by design);
//! - `cli`         `set timeSync.stepThresholdSecs 15`;
//! - `clock`       every 10 s.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use xorgate_hal::tape::{ConnState, KvsInfo, Tape, TapeEvent, TapeMeta, TapeRecord};

use crate::rewrite::{civil_from_epoch, iso_z, nmea_sentence};

/// 2026-07-29T19:30:00Z — `now` of the modem telemetry-tolerance fixture.
pub const SYNTH_START_WALL: i64 = 1_785_353_400_000;
/// The tape's deviceId (the player substitutes the rig ids).
pub const SYNTH_DEVICE_ID: &str = "00000000-0000-4000-8000-0000000005e1";

/// `(name, default duration s, tracks, description)`.
pub const SCENARIOS: &[(&str, u64, &[&str], &str)] = &[
    (
        "smoke",
        60,
        &[
            "nmea",
            "lte",
            "exec",
            "dt",
            "imu",
            "config",
            "commands-refresh",
            "clock",
        ],
        "≤60 s CI smoke tape: every input kind once, no conn drop",
    ),
    (
        "calibration",
        240,
        &[
            "nmea",
            "lte",
            "exec",
            "dt",
            "imu",
            "config",
            "conn-drop",
            "commands",
            "cli",
            "clock",
        ],
        "the C3a calibration tape: everything except sudo commands",
    ),
    (
        "full",
        600,
        &[
            "nmea",
            "nmea-silence",
            "lte",
            "lte-fixtures",
            "exec",
            "dt",
            "imu",
            "config",
            "conn-drop",
            "commands",
            "commands-sudo",
            "cli",
            "clock",
        ],
        "10 min, every track",
    ),
    (
        "nmea-fix",
        60,
        &["nmea-fixonly", "clock"],
        "NMEA bursts with a fix from t=0",
    ),
    (
        "nmea-nofix",
        60,
        &["nmea-nofix", "clock"],
        "NMEA bursts without a fix",
    ),
    (
        "nmea-silence",
        120,
        &["nmea", "nmea-silence", "clock"],
        "35 s silent NMEA gap",
    ),
    (
        "lte-seq",
        120,
        &["lte", "clock"],
        "LTE snapshots incl. an absent span",
    ),
    (
        "lte-fixtures",
        60,
        &["lte-fixtures", "clock"],
        "telemetry-tolerance raw docs",
    ),
    (
        "imu",
        90,
        &["imu", "clock"],
        "IMU detect, samples, helper exit + respawn",
    ),
    (
        "config",
        90,
        &["config", "conn-drop", "clock"],
        "retained config revs + redelivery",
    ),
    (
        "commands",
        90,
        &["commands", "clock"],
        "refresh / rate-limit / expired / duplicate",
    ),
    ("cli", 60, &["cli", "clock"], "one `set` edit"),
];

thread_local! {
    /// Start wall of the tape being built: [`SYNTH_START_WALL`], or the base
    /// tape's when splicing (`--base`), so embedded NMEA/command/LTE times line
    /// up with the meta `startWall` the player shifts from.
    static W0: std::cell::Cell<i64> = const { std::cell::Cell::new(SYNTH_START_WALL) };
}

fn w0() -> i64 {
    W0.with(|c| c.get())
}

/// Builder: events are pushed in any order and sorted (stably) by `t`.
struct Builder {
    recs: Vec<TapeRecord>,
    dur_ms: u64,
}

impl Builder {
    fn push(&mut self, t: u64, ev: TapeEvent) {
        if t <= self.dur_ms {
            self.recs.push(TapeRecord {
                t,
                w: w0() + t as i64,
                ev,
            });
        }
    }
}

fn wall_secs(t: u64) -> i64 {
    (w0() + t as i64).div_euclid(1000)
}

fn hms(t: u64) -> String {
    let (_, _, _, h, m, s) = civil_from_epoch(wall_secs(t));
    format!("{h:02}{m:02}{s:02}.00")
}

fn dmy(t: u64) -> String {
    let (y, mo, d, ..) = civil_from_epoch(wall_secs(t));
    format!("{d:02}{mo:02}{:02}", y.rem_euclid(100))
}

/// One second's SIM7600-shaped burst (CRLF-terminated lines, as split on `\n`).
fn nmea_burst(t: u64, fix: bool, step: u64) -> Vec<String> {
    let time = hms(t);
    let date = dmy(t);
    let mut out = Vec::new();
    if fix {
        // Fixture position (fixtures/nmea/nmea.json), drifting north-east.
        let lat_min = 30.1234 + step as f64 * 0.0005;
        let lon_min = 1.5678 + step as f64 * 0.0007;
        let lat = format!("14{lat_min:07.4}");
        let lon = format!("121{lon_min:07.4}");
        let alt = 545.4 + (step % 7) as f64 * 0.1;
        let speed = 0.06 + (step % 5) as f64 * 1.5;
        let course = (31.66 + step as f64 * 0.5) % 360.0;
        out.push(nmea_sentence(&format!(
            "GPGGA,{time},{lat},N,{lon},E,1,08,0.9,{alt:.1},M,46.9,M,,"
        )));
        out.push(nmea_sentence(&format!(
            "GPRMC,{time},A,{lat},N,{lon},E,{speed:.2},{course:.2},{date},,,A"
        )));
        out.push(nmea_sentence("GPGSA,A,3,04,05,,09,12,,,24,,,,,2.5,1.3,2.1"));
        let cn0 = 25 + (step % 9);
        out.push(nmea_sentence(&format!(
            "GPGSV,3,1,10,05,31,077,{cn0},15,79,067,20,20,46,052,30,21,10,100,21,1"
        )));
        out.push(nmea_sentence(
            "GPGSV,3,2,10,10,11,274,,18,75,292,,23,41,286,,27,09,322,,1",
        ));
        out.push(nmea_sentence("GPGSV,3,3,10,29,21,202,,30,04,032,,1"));
        out.push(nmea_sentence(
            "GLGSV,2,1,07,66,29,037,20,76,04,067,21,82,80,220,,75,06,018,,1",
        ));
        out.push(nmea_sentence(
            "GLGSV,2,2,07,81,35,158,,67,75,310,,68,31,237,,1",
        ));
    } else {
        out.push(nmea_sentence(&format!("GPGGA,{time},,,,,0,00,,,M,,M,,")));
        out.push(nmea_sentence(&format!("GPRMC,{time},V,,,,,,,{date},,,N")));
        out.push(nmea_sentence("GNGSA,A,1,,,,,,,,,,,,,,"));
        out.push(nmea_sentence(
            "GPGSV,2,1,08,05,31,077,,15,79,067,,20,46,052,,21,,,,1",
        ));
        out.push(nmea_sentence(
            "GPGSV,2,2,08,10,11,274,,18,75,292,,23,41,286,,27,09,322,,1",
        ));
    }
    out.into_iter().map(|l| format!("{l}\r")).collect()
}

fn lte_doc(t: u64, csq: i64, ok: bool) -> String {
    let now = wall_secs(t);
    let (y, mo, d, h, mi, s) = civil_from_epoch(now - 4 * 3600);
    let cclk = format!(
        "{:02}/{mo:02}/{d:02},{h:02}:{mi:02}:{s:02}-16",
        y.rem_euclid(100)
    );
    let rssi = -113 + 2 * csq;
    // Field order + pretty layout of `write_telemetry` in modem/sim7600-lte.sh;
    // identity values are obviously fake.
    format!(
        "{{\n  \"timestamp\": \"{ts}\",\n  \"csq\": {csq},\n  \"rssi_dbm\": {rssi},\n  \"csq_interval\": 30,\n  \"status\": \"{status}\",\n  \"reg\": 1,\n  \"iface\": \"usb0\",\n  \"ip\": \"192.0.2.37\",\n  \"xtra_valid_min\": 8622,\n  \"gnss_ant_bias\": 1,\n  \"modem_temp\": {temp},\n  \"modem_clock\": {{\"cclk\": \"{cclk}\", \"read_at\": {now}, \"reg\": 1}},\n  \"gnss\": {{\n    \"engine_on\": 1,\n    \"has_fix\": 1,\n    \"fix_checked_at\": \"{fixat}\",\n    \"ttff_s\": 32,\n    \"last_engine_start\": \"{start}\",\n    \"last_engine_restart\": null,\n    \"standalone_fallback\": 0,\n    \"no_fix_since\": null,\n    \"wedge_restarts_1h\": 0,\n    \"health\": \"ok\"\n  }},\n  \"recovery\": {{\n    \"connect_fails\": 0,\n    \"radio_cycles_since_ok\": 0,\n    \"resets_24h\": 0,\n    \"last_reset\": null,\n    \"last_gate_skip\": null,\n    \"last_lte_ok\": \"{ts}\"\n  }},\n  \"sim\": {{\"present\": 1, \"state\": \"ready\", \"iccid\": \"8900000000000000000\", \"msisdn\": \"+10000000000\", \"carrier\": \"Synth Mobile\", \"act\": \"lte\", \"registration\": \"home\", \"checked_at\": \"{fixat}\"}},\n  \"cellular\": {{\"state\": \"in-force\", \"apn_applied\": \"synth.apn\", \"fails\": 0, \"changed_at\": null}},\n  \"uplink\": {{\n    \"primary\": \"wlan0\",\n    \"modem_only\": 0\n  }}\n}}\n",
        ts = iso_z(now),
        status = if ok { "connected" } else { "connecting" },
        temp = 48 + (t / 30_000) % 4,
        fixat = iso_z(now - 180),
        start = iso_z(now - 3600),
    )
}

const PMIC_TEMPLATE: &str = " 3V7_WL_SW_A current(0)=0.10149670A\n   3V3_SYS_A current(1)=0.04586871A\n   1V8_SYS_A current(2)=0.12003940A\n  DDR_VDD2_A current(3)=0.00683151A\n  DDR_VDDQ_A current(4)=0.00000000A\n   1V1_SYS_A current(5)=0.19811380A\n    0V8_SW_A current(6)=0.22446390A\n  VDD_CORE_A current(7)=1.15197000A\n   3V3_DAC_A current(17)=0.00036630A\n   3V3_ADC_A current(18)=0.00109890A\n   0V8_AON_A current(16)=0.00347985A\n      HDMI_A current(22)=0.01245420A\n 3V7_WL_SW_V volt(8)=3.59770100V\n   3V3_SYS_V volt(9)=3.30353800V\n   1V8_SYS_V volt(10)=1.82075500V\n  DDR_VDD2_V volt(11)=1.11355200V\n  DDR_VDDQ_V volt(12)=0.60659280V\n   1V1_SYS_V volt(13)=1.10146400V\n    0V8_SW_V volt(14)=0.80439480V\n  VDD_CORE_V volt(15)=0.86979160V\n   3V3_DAC_V volt(20)=3.31043600V\n   3V3_ADC_V volt(21)=3.30036300V\n   0V8_AON_V volt(19)=0.80058540V\n      HDMI_V volt(23)=4.95264000V\n     EXT5V_V volt(24)=@EXT5V@V\n      BATT_V volt(25)=0.00512820V\n";

/// `i2cdetect -y 1` with the BNO085 at 0x4a (i2c-tools 4.3 layout).
pub const I2CDETECT_4A: &str = "     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f\n00:                         -- -- -- -- -- -- -- -- \n10: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- \n20: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- \n30: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- \n40: -- -- -- -- -- -- -- -- -- -- 4a -- -- -- -- -- \n50: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- \n60: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- \n70: -- -- -- -- -- -- -- --                         \n";

fn imu_sample(k: u64) -> String {
    // BNO085 Q-points: quaternion 2^-14, linear acceleration 2^-8.
    let a = k as f64 * 0.01;
    let q = |x: f64| (x * 16384.0).round() / 16384.0;
    let la = |x: f64| (x * 256.0).round() / 256.0;
    let (r, i, j, kk) = (
        q((a / 2.0).cos() * 0.9),
        q((a / 2.0).sin() * 0.3),
        q((a / 3.0).sin() * 0.2),
        q((a / 5.0).cos() * 0.25),
    );
    let v = json!({
        "type": "sample",
        "quat": {"r": r, "i": i, "j": j, "k": kk},
        "linaccel": {"x": la(a.sin() * 0.4), "y": la(a.cos() * 0.4), "z": la((a / 2.0).sin() * 0.2)},
    });
    python_json(&v)
}

/// `json.dumps(obj)` layout (`", "` / `": "` separators), as the helper prints.
pub fn python_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{}: {}", Value::String(k.clone()), python_json(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(python_json).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap_or(0.0);
            if f.fract() == 0.0 && f.abs() < 1e16 {
                format!("{f:.1}")
            } else {
                n.to_string()
            }
        }
        other => other.to_string(),
    }
}

fn config_doc(rev: i64, step: u32, enabled_mount: bool) -> String {
    json!({
        "v": 1,
        "rev": rev,
        "gnssAntBias": true,
        "recording": {
            "telemetry": {"enabled": false, "rateHz": 10},
            "video": {"enabled": false, "preset": "1080p15-2mbps", "segmentSeconds": 60},
            "upload": {"mode": "off"},
        },
        "imuMount": {"enabled": enabled_mount, "rollDeg": if enabled_mount { 90 } else { 0 }, "pitchDeg": 0, "yawDeg": 0},
        "timeSync": {"offlineSources": true, "stepThresholdSecs": step, "maxStepsPerBoot": 3},
    })
    .to_string()
}

fn command(id: &str, cmd: &str, t: u64, ttl_ms: i64) -> String {
    let ts = w0() + t as i64;
    json!({"v": 1, "requestId": id, "command": cmd, "ts": ts, "expiresAt": ts + ttl_ms}).to_string()
}

/// Build a tape for `scenario` (`duration_s` overrides the default).
pub fn build(scenario: &str, duration_s: Option<u64>, fixtures: &Path) -> Result<Tape> {
    build_with(scenario, duration_s, fixtures, None)
}

/// Kinds a `--base` tape (record-shared) contributes; the synthetic tracks
/// that produce them are dropped.
const BASE_KINDS: &[&str] = &["lte_file", "exec", "dt", "clock", "proc"];

/// [`build`] spliced onto a recorded base tape: the base's real
/// `lte_file`/`exec`/`dt`/`clock` records (aligned at t = 0, cut to the
/// duration) replace the synthetic ones; NMEA/IMU/MQTT/CLI stay synthetic and
/// are generated on the base's start wall.
pub fn build_with(
    scenario: &str,
    duration_s: Option<u64>,
    fixtures: &Path,
    base: Option<&Tape>,
) -> Result<Tape> {
    W0.with(|c| c.set(base.map(|b| b.meta.start_wall).unwrap_or(SYNTH_START_WALL)));
    let res = build_inner(scenario, duration_s, fixtures, base);
    W0.with(|c| c.set(SYNTH_START_WALL));
    res
}

fn build_inner(
    scenario: &str,
    duration_s: Option<u64>,
    fixtures: &Path,
    base: Option<&Tape>,
) -> Result<Tape> {
    let Some(&(name, default_s, tracks, desc)) = SCENARIOS.iter().find(|s| s.0 == scenario) else {
        bail!(
            "unknown scenario {scenario:?}; known: {}",
            SCENARIOS.iter().map(|s| s.0).collect::<Vec<_>>().join(", ")
        );
    };
    let dur_ms = duration_s.unwrap_or(default_s) * 1000;
    let mut b = Builder {
        recs: Vec::new(),
        dur_ms,
    };
    let base_owned = ["lte", "lte-fixtures", "exec", "dt", "clock"];
    let has = |t: &str| tracks.contains(&t) && !(base.is_some() && base_owned.contains(&t));
    let topic = |s: &str| format!("xorgate/devices/{SYNTH_DEVICE_ID}/{s}");
    let pct = |p: u64| dur_ms * p / 100 / 1000 * 1000;

    b.push(
        0,
        TapeEvent::Conn {
            state: ConnState::Connected,
        },
    );

    // ---- conn-drop: 20 s closed at 50 %.
    let drop_at = pct(50) + 300;
    let drop_end = drop_at + 20_000;
    let drop = has("conn-drop") && drop_end + 5_000 < dur_ms;
    if drop {
        b.push(
            drop_at,
            TapeEvent::Conn {
                state: ConnState::Closed,
            },
        );
        b.push(
            drop_end,
            TapeEvent::Conn {
                state: ConnState::Connected,
            },
        );
    }

    // ---- config (retained; redelivered after a reconnect)
    if has("config") {
        let rev2_at = pct(60) + 700;
        let doc1 = config_doc(1, 10, false);
        let doc2 = config_doc(2, 12, true);
        b.push(
            400,
            TapeEvent::mqtt_in(&topic("config"), doc1.as_bytes(), true, 1),
        );
        if drop {
            let doc = if drop_end > rev2_at { &doc2 } else { &doc1 };
            b.push(
                drop_end + 300,
                TapeEvent::mqtt_in(&topic("config"), doc.as_bytes(), true, 1),
            );
        }
        b.push(
            rev2_at,
            TapeEvent::mqtt_in(&topic("config"), doc2.as_bytes(), false, 1),
        );
    }

    // ---- dt
    if has("dt") {
        b.push(50, TapeEvent::dt(Some(&[0x00, 0x00, 0x0b, 0xb8])));
    }

    // ---- exec: get_throttled every change, pmic every 5 s.
    if has("exec") {
        let thr = vec!["vcgencmd".to_string(), "get_throttled".to_string()];
        let pmic = vec!["vcgencmd".to_string(), "pmic_read_adc".to_string()];
        let uv_at = pct(35) + 200;
        b.push(
            60,
            TapeEvent::Exec {
                argv: thr.clone(),
                stdout: "throttled=0x0\n".into(),
                code: Some(0),
            },
        );
        b.push(
            uv_at,
            TapeEvent::Exec {
                argv: thr.clone(),
                stdout: "throttled=0x50005\n".into(),
                code: Some(0),
            },
        );
        b.push(
            uv_at + 10_000,
            TapeEvent::Exec {
                argv: thr,
                stdout: "throttled=0x50000\n".into(),
                code: Some(0),
            },
        );
        let mut t = 70;
        let mut k = 0u64;
        while t < dur_ms {
            let uv = t >= uv_at && t < uv_at + 10_000;
            let volts = if uv { 4.64 } else { 5.10 } + (k % 4) as f64 * 0.01;
            b.push(
                t,
                TapeEvent::Exec {
                    argv: pmic.clone(),
                    stdout: PMIC_TEMPLATE.replace("@EXT5V@", &format!("{volts:.8}")),
                    code: Some(0),
                },
            );
            t += 5_000;
            k += 1;
        }
    }

    // ---- lte: rewritten every 10 s, absent for 15 s at 40 %.
    if has("lte") {
        let gone_at = pct(40) + 500;
        let mut t = 80;
        let mut k = 0i64;
        let mut absent_done = false;
        while t < dur_ms {
            if !absent_done && t >= gone_at {
                b.push(gone_at, TapeEvent::lte_file(None));
                absent_done = true;
                t = gone_at + 15_000;
                continue;
            }
            let csq = 18 + (k % 7);
            b.push(
                t,
                TapeEvent::lte_file(Some(lte_doc(t, csq, true).as_bytes())),
            );
            t += 10_000;
            k += 1;
        }
    }
    if has("lte-fixtures") {
        let path = fixtures.join("modem/telemetry-tolerance.json");
        let doc: Value = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )?;
        let start = if has("lte") { pct(70) } else { 100 };
        let mut t = start;
        for case in doc["cases"].as_array().into_iter().flatten() {
            if let Some(raw) = case["input"]["raw"].as_str() {
                b.push(t, TapeEvent::lte_file(Some(raw.as_bytes())));
                t += 3_000;
            }
        }
    }

    // ---- nmea
    let nmea_mode = if has("nmea") {
        Some("mixed")
    } else if has("nmea-fixonly") {
        Some("fix")
    } else if has("nmea-nofix") {
        Some("nofix")
    } else {
        None
    };
    if let Some(mode) = nmea_mode {
        let fix_from = match mode {
            "fix" => 0,
            "nofix" => u64::MAX,
            _ => pct(25),
        };
        let (silent_from, silent_to) = if has("nmea-silence") {
            let s = pct(80).max(pct(30));
            (s, s + 35_000)
        } else {
            (u64::MAX, u64::MAX)
        };
        let mut sec = 0u64;
        let mut step = 0u64;
        loop {
            let t0 = 900 + sec * 1000;
            if t0 >= dur_ms {
                break;
            }
            if !(t0 >= silent_from && t0 < silent_to) {
                let fix = t0 >= fix_from;
                for (i, line) in nmea_burst(t0, fix, step).into_iter().enumerate() {
                    b.push(t0 + i as u64 * 8, TapeEvent::Nmea { line });
                }
                if fix {
                    step += 1;
                }
            }
            sec += 1;
        }
    }

    // ---- imu
    if has("imu") {
        let argv = vec![
            "python3".to_string(),
            "/opt/xorgate-rs/python/bno085_read.py".to_string(),
            "--address".to_string(),
            "0x4a".to_string(),
        ];
        b.push(
            300,
            TapeEvent::I2cdetect {
                stdout: Some(I2CDETECT_4A.to_string()),
                not_found: false,
            },
        );
        let exit_at = pct(80) + 450;
        let respawn_at = exit_at + 2_500;
        let spawn = |b: &mut Builder, t: u64| {
            b.push(t, TapeEvent::ImuSpawn { argv: argv.clone() });
            b.push(
                t + 600,
                TapeEvent::Imu {
                    line: r#"{"type": "ready", "address": "0x4a"}"#.to_string(),
                },
            );
        };
        spawn(&mut b, 350);
        let mut t = 1_000;
        let mut k = 0u64;
        while t < dur_ms {
            if t >= exit_at && t < respawn_at + 700 {
                t += 50;
                continue;
            }
            b.push(
                t,
                TapeEvent::Imu {
                    line: imu_sample(k),
                },
            );
            t += 50;
            k += 1;
        }
        if exit_at < dur_ms {
            b.push(
                exit_at,
                TapeEvent::Imu {
                    line: r#"{"type": "error", "error": "read failed 40x: synthetic bus error"}"#
                        .to_string(),
                },
            );
            b.push(exit_at + 5, TapeEvent::ImuExit { code: Some(4) });
            b.push(
                respawn_at - 100,
                TapeEvent::I2cdetect {
                    stdout: Some(I2CDETECT_4A.to_string()),
                    not_found: false,
                },
            );
            spawn(&mut b, respawn_at);
        }
    }

    // ---- commands
    if has("commands") || has("commands-refresh") {
        let at = pct(30) + 100;
        b.push(
            at,
            TapeEvent::mqtt_in(
                &topic("command"),
                command("synth-refresh-1", "refresh", at, 60_000).as_bytes(),
                false,
                1,
            ),
        );
        if has("commands") {
            let t2 = at + 10_000;
            b.push(
                t2,
                TapeEvent::mqtt_in(
                    &topic("command"),
                    command("synth-refresh-2", "refresh", t2, 60_000).as_bytes(),
                    false,
                    1,
                ),
            );
            // Expired: issued 3 min before it arrives, 60 s TTL.
            let t3 = at + 15_000;
            let mut exp: Value =
                serde_json::from_str(&command("synth-expired", "refresh", t3, 60_000))?;
            exp["ts"] = json!(w0() + t3 as i64 - 180_000);
            exp["expiresAt"] = json!(w0() + t3 as i64 - 120_000);
            b.push(
                t3,
                TapeEvent::mqtt_in(&topic("command"), exp.to_string().as_bytes(), false, 1),
            );
            // Duplicate of the first requestId, 50 s later (past the window).
            let t4 = at + 50_000;
            b.push(
                t4,
                TapeEvent::mqtt_in(
                    &topic("command"),
                    command("synth-refresh-1", "refresh", t4, 60_000).as_bytes(),
                    false,
                    1,
                ),
            );
        }
    }
    if has("commands-sudo") {
        let at = pct(90);
        b.push(
            at,
            TapeEvent::mqtt_in(
                &topic("command"),
                command("synth-update-1", "update", at, 60_000).as_bytes(),
                false,
                1,
            ),
        );
        b.push(
            at + 5_000,
            TapeEvent::mqtt_in(
                &topic("command"),
                command("synth-reboot-1", "reboot", at + 5_000, 60_000).as_bytes(),
                false,
                1,
            ),
        );
    }

    // ---- cli
    if has("cli") {
        b.push(
            pct(70) + 300,
            TapeEvent::Cli {
                argv: vec![
                    "set".into(),
                    "timeSync.stepThresholdSecs".into(),
                    "15".into(),
                ],
            },
        );
    }

    // ---- clock
    if has("clock") {
        let mut t = 10_000;
        while t <= dur_ms {
            b.push(
                t,
                TapeEvent::Clock {
                    wall: w0() + t as i64,
                    mono: 1_000_000 + t,
                },
            );
            t += 10_000;
        }
    }

    if let Some(base) = base {
        for r in &base.records {
            if r.t <= dur_ms && BASE_KINDS.contains(&r.ev.kind()) {
                b.recs.push(r.clone());
            }
        }
    }
    b.recs.sort_by_key(|r| r.t);
    let mut meta = TapeMeta::new(SYNTH_DEVICE_ID, "synth", w0());
    meta.kvs = KvsInfo { channels: vec![] };
    meta.source = Some(match base {
        Some(b) => format!("{}+synth:{name}", b.meta.source.as_deref().unwrap_or("bench")),
        None => format!("synth:{name}"),
    });
    meta.synthetic = tracks
        .iter()
        .filter(|t| base.is_none() || !base_owned.contains(t))
        .map(|t| t.to_string())
        .collect();
    meta.env = TapeMeta::redacted_env(&|k| match k {
        "MODE" => Some("real".into()),
        _ => None,
    });
    let _ = desc;
    Ok(Tape {
        meta,
        records: b.recs,
        truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::nmea_checksum;
    use std::path::PathBuf;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
    }

    #[test]
    fn every_scenario_builds_sorted_and_within_duration() {
        for (name, dur, ..) in SCENARIOS {
            let tape = build(name, None, &fixtures()).unwrap();
            assert!(!tape.records.is_empty(), "{name}");
            assert!(tape.records.windows(2).all(|w| w[0].t <= w[1].t), "{name}");
            assert!(tape.duration_ms() <= dur * 1000, "{name}");
            assert_eq!(tape.meta.source.as_deref(), Some(&*format!("synth:{name}")));
        }
        assert!(build("nope", None, &fixtures()).is_err());
    }

    #[test]
    fn synth_is_deterministic() {
        let a = build("calibration", Some(120), &fixtures()).unwrap();
        let b = build("calibration", Some(120), &fixtures()).unwrap();
        let (mut x, mut y) = (Vec::new(), Vec::new());
        a.write_to(&mut x).unwrap();
        b.write_to(&mut y).unwrap();
        assert_eq!(x, y);
    }

    #[test]
    fn nmea_lines_carry_valid_checksums_and_crlf() {
        let tape = build("nmea-fix", Some(5), &fixtures()).unwrap();
        let mut n = 0;
        for r in &tape.records {
            if let TapeEvent::Nmea { line } = &r.ev {
                let core = line.strip_suffix('\r').expect("CRLF");
                let (body, cs) = core[1..].rsplit_once('*').unwrap();
                assert_eq!(
                    u8::from_str_radix(cs, 16).unwrap(),
                    nmea_checksum(body),
                    "{line}"
                );
                n += 1;
            }
        }
        assert!(n >= 8 * 4, "{n}");
    }

    #[test]
    fn smoke_has_every_core_kind() {
        let tape = build("smoke", None, &fixtures()).unwrap();
        for k in [
            "nmea",
            "imu",
            "imu_spawn",
            "i2cdetect",
            "lte_file",
            "exec",
            "dt",
            "mqtt_in",
            "conn",
            "clock",
        ] {
            assert!(tape.records.iter().any(|r| r.ev.kind() == k), "{k}");
        }
        // The lte docs are valid JSON.
        for r in &tape.records {
            if let Some(b) = r.ev.lte_bytes() {
                serde_json::from_slice::<Value>(&b).unwrap();
            }
        }
    }

    /// CI smoke (WP7 brief): the committed smoke tape reads back and is
    /// exactly what `synth smoke` produces today (regenerate with
    /// `xorgate-replay synth smoke -o fixtures/replay/smoke.ndjson.gz`).
    #[test]
    fn committed_smoke_tape_is_current() {
        let path = fixtures().join("replay/smoke.ndjson.gz");
        let tape = Tape::read(&path).unwrap();
        assert!(!tape.truncated);
        assert!(tape.duration_ms() <= 60_000, "O8: ≤60 s");
        let fresh = build("smoke", None, &fixtures()).unwrap();
        assert_eq!(tape.meta, fresh.meta);
        assert_eq!(
            tape.records, fresh.records,
            "fixtures/replay/smoke.ndjson.gz is stale"
        );
        let s = crate::inspect::summarize(&tape);
        assert_eq!(s.out_of_order, 0);
        assert!(s.wall_steps.is_empty());
    }

    #[test]
    fn splice_keeps_base_power_and_lte_and_synthesizes_the_rest() {
        let mut base = build("lte-seq", Some(30), &fixtures()).unwrap();
        base.meta.start_wall = 1_791_000_000_000;
        base.meta.source = Some("record-shared".into());
        let t = build_with("smoke", Some(30), &fixtures(), Some(&base)).unwrap();
        assert_eq!(t.meta.start_wall, 1_791_000_000_000);
        assert_eq!(t.meta.source.as_deref(), Some("record-shared+synth:smoke"));
        assert!(!t.meta.synthetic.contains(&"lte".to_string()));
        let lte: Vec<_> = t.records.iter().filter(|r| r.ev.kind() == "lte_file").collect();
        let base_lte: Vec<_> = base.records.iter().filter(|r| r.ev.kind() == "lte_file").collect();
        assert_eq!(lte, base_lte);
        assert!(t.records.iter().any(|r| r.ev.kind() == "nmea"));
        // Synthetic records are stamped on the base's wall clock.
        assert!(t.records.iter().all(|r| r.w >= 1_791_000_000_000 || BASE_KINDS.contains(&r.ev.kind())));
        // And the thread-local is restored.
        assert_eq!(build("smoke", Some(5), &fixtures()).unwrap().meta.start_wall, SYNTH_START_WALL);
    }

    #[test]
    fn lte_fixtures_track_reads_the_fixture() {
        let tape = build("lte-fixtures", None, &fixtures()).unwrap();
        assert!(
            tape.records
                .iter()
                .filter(|r| r.ev.kind() == "lte_file")
                .count()
                >= 10
        );
    }
}
