//! Phase 4 synthetic scenarios (WP6, `phase-4-operations.md` §8): the inputs
//! of the LED ladder, SMS, clock-stepper, camera-mount and update-command
//! tapes. A child of `synth` (private helpers reached through `super::`);
//! every scenario lists the `p4` track and this module adds its records by
//! scenario name. The rig side (Pi namespace, fake modem, shims) is `p4.rs`.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use xorgate_hal::tape::{ConnState, TapeEvent};

use super::{
    cellular_block, civil_from_epoch, command, lte_doc, media_hardware_block, nmea_burst,
    rec_namespace, wall_secs, Builder, CM5_CAMERA_LIST, PMIC_TEMPLATE, SYNTH_DEVICE_ID,
};

/// How far ahead of the host clock the GPS / NITZ sources run in the clock
/// tapes: the rig cannot move the container clock, so the sources move (the
/// bench recipe sets the clock −600 s instead, §9 E8).
pub const CLOCK_AHEAD_S: i64 = 600;

/// A bench-shaped boot config (`test/boot-config.test.ts` `CONFIG_TXT` @89b5c64).
pub const CONFIG_TXT: &str = "# Raspberry Pi config\ncamera_auto_detect=0\ndtoverlay=vc4-kms-v3d\n[cm4]\notg_mode=1\n[cm5]\ndtoverlay=dwc2,dr_mode=host\n[all]\ndtoverlay=imx290,cam0,4lane\ndtoverlay=imx290,cam1\n#dtoverlay=googlevoicehat-soundcard\n";

/// `dtoverlay -h imx290` (the parameter list carries `rotation`, so the
/// camera-mount probe `boot-config.ts:209-215` answers `supports: true`).
pub const DTOVERLAY_IMX290: &str = "Name:   imx290\n\nInfo:   Sony IMX290 camera module.\n        Uses Unicam 1, which is the standard camera connector on most Pi\n        variants.\n\nUsage:  dtoverlay=imx290,<param>=<val>\n\nParams: 4lane                   Enable 4 CSI2 lanes. This requires a Compute\n                                Module (1, 3, 4, or 5) or Pi 5.\n        clock-frequency         Sets the clock frequency to match that used on\n                                the board.\n        mono                    Denote that the module is a mono sensor.\n        orientation             Sensor orientation (0 = front, 1 = rear,\n                                2 = external, default external)\n        rotation                Mounting rotation of the camera sensor (0 or\n                                180, default 0)\n        media-controller        Configure use of Media Controller API for\n                                configuring the sensor (default on)\n        cam0                    Adopt the default configuration for CAM0 on a\n                                Compute Module (CSI0, i2c_vc, and cam0_reg).\n";

/// The CM5 bench's two imx290 nodes (`CM5_CAMERA_LIST`, `/base` stripped).
pub const CAM0_DT: &str = "axi/pcie@1000120000/rp1/i2c@88000/imx290@1a";
pub const CAM1_DT: &str = "axi/pcie@1000120000/rp1/i2c@70000/imx290@1a";

const R0: &str = "00000000";
const R180: &str = "000000b4";

/// What the scenario adds to the meta.
#[derive(Default)]
pub struct P4Meta {
    pub rig: Map<String, Value>,
    pub home: Option<Map<String, Value>>,
    pub hardware: Option<Value>,
    /// Hardware block but NO kvs channels (no masters, no media upload).
    pub no_channels: bool,
}

fn topic(s: &str) -> String {
    format!("xorgate/devices/{SYNTH_DEVICE_ID}/{s}")
}

fn cfg(b: &mut Builder, t: u64, doc: &Value) {
    b.push(
        t,
        TapeEvent::mqtt_in(&topic("config"), doc.to_string().as_bytes(), true, 1),
    );
}

fn cmd(b: &mut Builder, t: u64, id: &str, name: &str, args: Option<Value>) -> Result<()> {
    let mut c: Value = serde_json::from_str(&command(id, name, t, 60_000))?;
    if let Some(a) = args {
        c["args"] = a;
    }
    b.push(
        t,
        TapeEvent::mqtt_in(&topic("command"), c.to_string().as_bytes(), false, 1),
    );
    Ok(())
}

/// One NMEA burst per second from t = 0.9 s; `fix_from` (`u64::MAX` = never);
/// embedded times `ahead_s` ahead of the tape clock.
fn nmea(b: &mut Builder, dur_ms: u64, fix_from: u64, ahead_s: i64) {
    let mut step = 0u64;
    let mut sec = 0u64;
    loop {
        let t0 = 900 + sec * 1000;
        if t0 >= dur_ms {
            break;
        }
        let fix = t0 >= fix_from;
        let embed = (t0 as i64 + ahead_s * 1000).max(0) as u64;
        for (i, line) in nmea_burst(embed, fix, step).into_iter().enumerate() {
            b.push(t0 + i as u64 * 8, TapeEvent::Nmea { line });
        }
        if fix {
            step += 1;
        }
        sec += 1;
    }
}

/// The modem daemon's file every 10 s from 80 ms, its `modem_clock` replaced
/// by `clock(t) → (cclk, reg)` (`None` = the daemon's own: now, reg 1).
fn lte_with_clock(b: &mut Builder, dur_ms: u64, clock: &dyn Fn(u64) -> Option<(String, i64)>) {
    let mut t = 80;
    let mut k = 0i64;
    while t < dur_ms {
        let mut doc = lte_doc(t, 18 + k % 7, true, &cellular_block(None));
        if let Some((cclk, reg)) = clock(t) {
            let start = doc
                .find("\"modem_clock\": {")
                .expect("lte_doc has modem_clock");
            let end = start + doc[start..].find('}').expect("modem_clock closes") + 1;
            doc.replace_range(
                start..end,
                &format!(
                    "\"modem_clock\": {{\"cclk\": \"{cclk}\", \"read_at\": {}, \"reg\": {reg}}}",
                    wall_secs(t)
                ),
            );
        }
        b.push(t, TapeEvent::lte_file(Some(doc.as_bytes())));
        t += 10_000;
        k += 1;
    }
}

/// `+CCLK` text (local time, zone −16 quarter hours like the daemon's) for
/// the tape time `t` shifted by `ahead_s`.
fn cclk_at(t: u64, ahead_s: i64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_epoch(wall_secs(t) + ahead_s - 4 * 3600);
    format!(
        "{:02}/{mo:02}/{d:02},{h:02}:{mi:02}:{s:02}-16",
        y.rem_euclid(100)
    )
}

/// The Pi identity + boot config + both cameras' DT rotation at 0.
fn pi_rig(dt_writes: Vec<Value>) -> Value {
    json!({
        "configTxt": CONFIG_TXT,
        "dt": { format!("{CAM0_DT}/rotation"): R0, format!("{CAM1_DT}/rotation"): R0 },
        "dtWrites": dt_writes,
    })
}

/// The hardware block's camera inputs: the bench listing + the overlay probe.
fn camera_inputs(b: &mut Builder) {
    b.push(
        40,
        TapeEvent::Exec {
            argv: vec!["rpicam-hello".into(), "--list-cameras".into()],
            stdout: CM5_CAMERA_LIST.into(),
            code: Some(0),
            stderr: None,
        },
    );
    b.push(
        45,
        TapeEvent::Exec {
            argv: vec!["dtoverlay".into(), "-h".into(), "imx290".into()],
            stdout: DTOVERLAY_IMX290.into(),
            code: Some(0),
            stderr: None,
        },
    );
}

/// The power-shed inputs of `p4-led-ladder`: get_throttled clean, the rail
/// (pmic EXT5V) clean except `low` (constant on each side of the threshold,
/// like `exec-rail`), every 2 s; the crossings for `meta.rig.powerCrossings`.
fn rail(b: &mut Builder, dur_ms: u64, low: std::ops::Range<u64>) -> Vec<u64> {
    let thr = vec!["vcgencmd".to_string(), "get_throttled".to_string()];
    let pmic = vec!["vcgencmd".to_string(), "pmic_read_adc".to_string()];
    let mut crossings = Vec::new();
    let mut last: Option<bool> = None;
    let mut t = 60;
    while t < dur_ms {
        let is_low = low.contains(&t);
        if last.is_some_and(|l| l != is_low) {
            crossings.push(t + 10);
        }
        last = Some(is_low);
        b.push(
            t,
            TapeEvent::Exec {
                argv: thr.clone(),
                stdout: "throttled=0x0\n".into(),
                code: Some(0),
                stderr: None,
            },
        );
        let volts = if is_low { 5.10 } else { 5.30 };
        b.push(
            t + 10,
            TapeEvent::Exec {
                argv: pmic.clone(),
                stdout: PMIC_TEMPLATE.replace("@EXT5V@", &format!("{volts:.8}")),
                code: Some(0),
                stderr: None,
            },
        );
        t += 2_000;
    }
    crossings
}

/// The Phase 4 records of `scenario` (every Phase 4 scenario carries `p4`).
pub fn add(scenario: &str, b: &mut Builder, dur_ms: u64, fixtures: &Path) -> Result<P4Meta> {
    let mut m = P4Meta::default();
    match scenario {
        // LED ladder (E2/E3, CF57): starting → gps-wait → healthy → network
        // (cloud cut) → healthy → config-applied flash2 → undervoltage (the
        // Phase 3 power seam: liveBlock.reason first) → healthy.
        "p4-led-ladder" => {
            // The bench's LED wiring declared (a block without `statusLed` is
            // the inert "model declares no status LED", worker.ts:611, CF61).
            let mut hw = media_hardware_block();
            hw["statusLed"] = json!({"r": 12, "g": 21, "b": 16});
            m.hardware = Some(hw);
            m.rig.insert("pi".into(), pi_rig(vec![]));
            camera_inputs(b);
            nmea(b, dur_ms, 15_000, 0);
            // The rec-shed-uv recording namespace (video + the rail ladder);
            // the telemetry recorder off (not this tape's subject).
            let mut rec = rec_namespace("rec-shed-uv");
            rec["telemetry"]["enabled"] = json!(false);
            cfg(b, 400, &json!({"v": 1, "rev": 1, "recording": rec}));
            b.push(
                40_300,
                TapeEvent::Conn {
                    state: ConnState::Closed,
                },
            );
            b.push(
                60_300,
                TapeEvent::Conn {
                    state: ConnState::Connected,
                },
            );
            // A REAL change (the flash is gated on `changed`, worker.ts:866-872)
            // in a namespace nothing else on this tape reads (no IMU).
            let mount = json!({"enabled": true, "rollDeg": 90, "pitchDeg": 0, "yawDeg": 0});
            let rev2 = json!({"v": 1, "rev": 2, "recording": rec, "imuMount": mount});
            cfg(b, 75_700, &rev2);
            // The same rev again (a retained redelivery: no flash).
            cfg(b, 85_700, &rev2);
            let crossings = rail(b, dur_ms, 100_000..140_000);
            m.rig.insert("powerCrossings".into(), json!(crossings));
        }
        // SMS (E5, CF29 rig half): the bench inbox + a concat UCS2 + a SUBMIT
        // on a FakeModem pty, starting in text mode; a read, a reused read
        // inside the 5 s window, a later fresh read with a smaller limit.
        "p4-sms" => {
            let path = fixtures.join("replay/p4-sms-modem.json");
            let doc: Value = serde_json::from_slice(
                &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
            )?;
            m.rig.insert("modem".into(), doc["modem"].clone());
            cmd(
                b,
                8_200,
                "synth-p4-sms-1",
                "read_sms",
                Some(json!({"limit": 10})),
            )?;
            cmd(b, 10_200, "synth-p4-sms-2", "read_sms", None)?;
            cmd(
                b,
                30_200,
                "synth-p4-sms-3",
                "read_sms",
                Some(json!({"limit": 3})),
            )?;
        }
        // Clock stepper from GPS (E7): NTP unsynced; GPS +600 s ahead with a
        // fix from the start (NITZ = the host clock: GPS is preferred); NTP
        // synced at 90 s → stood down.
        "p4-clock-gps" => {
            b.push(20, TapeEvent::NtpProbe { synced: false });
            nmea(b, dur_ms, 0, CLOCK_AHEAD_S);
            lte_with_clock(b, dur_ms, &|_| None);
            b.push(90_000, TapeEvent::NtpProbe { synced: true });
        }
        // Clock stepper from NITZ (E7): no GPS fix; the modem clock +600 s
        // ahead but NOT registered (reg 0, fail-closed) for the first 30 s,
        // then registered (reg 5) and fresh → a NITZ step; NTP at 100 s.
        "p4-clock-nitz" => {
            b.push(20, TapeEvent::NtpProbe { synced: false });
            nmea(b, dur_ms, u64::MAX, 0);
            lte_with_clock(b, dur_ms, &|t| {
                Some((cclk_at(t, CLOCK_AHEAD_S), if t < 30_000 { 0 } else { 5 }))
            });
            b.push(100_000, TapeEvent::NtpProbe { synced: true });
        }
        // Clock stepper cap + implausible source (E7): maxStepsPerBoot 1
        // (seeded settings.json); an implausible NITZ (the SIM7600's factory
        // 80/01/06, registered) → warn + 5 min hold-off; GPS (+600 s) from
        // 200 s → step 1/1 after the hold-off; the next attempt after the
        // second hold-off → the cap warning.
        "p4-clock-cap" => {
            b.push(20, TapeEvent::NtpProbe { synced: false });
            nmea(b, dur_ms, 200_000, CLOCK_AHEAD_S);
            lte_with_clock(b, dur_ms, &|_| {
                Some(("80/01/06,00:00:00+00".to_string(), 1))
            });
            let mut home = Map::new();
            home.insert(
                "settings.json".into(),
                json!(json!({"timeSync": {"maxStepsPerBoot": 1}}).to_string()),
            );
            m.home = Some(home);
        }
        // Camera mount (E12): cloud cameraMount cam0 0→180 (helper writes
        // config.txt → pending-reboot), simulated reboot (DT rotation bytes =
        // 180 → applied on the 60 s poll), →0 (pending again), DT → 0.
        "p4-cam-mount" => {
            m.hardware = Some(media_hardware_block());
            m.no_channels = true;
            camera_inputs(b);
            let rot = format!("{CAM0_DT}/rotation");
            m.rig.insert(
                "pi".into(),
                pi_rig(vec![
                    json!({"t": 100_000, "rel": rot, "hex": R180}),
                    json!({"t": 230_000, "rel": rot, "hex": R0}),
                ]),
            );
            let base = json!({
                "telemetry": {"enabled": false, "rateHz": 1},
                "video": {"enabled": false},
                "upload": {"mode": "off"},
            });
            cfg(b, 400, &json!({"v": 1, "rev": 1, "recording": base}));
            let mount = |deg: u32| json!({"cameras": {"cam0": {"rotationDeg": deg}}});
            cfg(
                b,
                20_700,
                &json!({"v": 1, "rev": 2, "recording": base, "cameraMount": mount(180)}),
            );
            cfg(
                b,
                170_700,
                &json!({"v": 1, "rev": 3, "recording": base, "cameraMount": mount(0)}),
            );
        }
        // Remote rail (E10/E16): cloud update → the updater unit argv;
        // reboot / power_off argv unchanged from Phase 2 (the sudo shim never
        // runs them, so the agents keep running).
        "p4-update-cmd" => {
            cmd(b, 10_200, "synth-p4-update-1", "update", None)?;
            cmd(b, 20_200, "synth-p4-reboot-1", "reboot", None)?;
            cmd(b, 30_200, "synth-p4-poweroff-1", "power_off", None)?;
        }
        _ => {}
    }
    Ok(m)
}
