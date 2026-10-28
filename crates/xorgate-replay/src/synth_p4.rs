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

/// The SIM7600's un-NITZ'd factory clock `t` ms after the modem's boot
/// (80/01/06 00:00:00 = the GPS epoch, zone +00).
fn factory_cclk(t: u64) -> String {
    const GPS_EPOCH_S: i64 = 315_964_800;
    let (y, mo, d, h, mi, s) = civil_from_epoch(GPS_EPOCH_S + (t / 1000) as i64);
    format!("{:02}/{mo:02}/{d:02},{h:02}:{mi:02}:{s:02}+00", y.rem_euclid(100))
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
            // One camera (half the x264 load: a JS-vs-Rust run on a loaded VM
            // showed Node's 125 ms blink legs landing up to 300 ms late).
            rec["video"]["cameras"] = json!({"cam1": false});
            // The full rec-shed-uv ladder (rung 1 → rung 2 restarts the capture,
            // then the restore restarts it again): the transition-started
            // session's cut fragment is judged by T12/cut-fragment
            // (review-wp6 M3).
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
            // The 300 s trust grace is checked AFTER each 5 s poll's NTP probe
            // (ts:src/agent/clock-trust.ts:147-152), and 300 s is also the
            // 300th 1 s telemetry tick: with an instant probe the two fall on
            // the same instant and the held count is 298 or 299 in the TS
            // itself (stepC p5b-clock-fallback.cal09 298/298, .jsrs 299).
            // The probe latency moves the check off the tick, but every
            // trust text prints its seconds with toFixed(0)
            // (ts:src/agent/clock-trust.ts:104-106): at 500 ms the elapsed
            // 300.5 s rounded to 300 or 301 run to run (stepC
            // p4-clock-cap.clk.jsrs2 300 / 301). 250 ms sits midway between
            // the tick (.0) and the rounding edge (.5).
            m.rig.insert("ntpProbeDelayMs".into(), json!(250));
            b.push(20, TapeEvent::NtpProbe { synced: false });
            nmea(b, dur_ms, 200_000, CLOCK_AHEAD_S);
            // From 15 s (review-wp4 R4-5): the implausible NITZ lands on
            // check tick 1, so the D40 step race (Rust ~330 s, JS ~360 s)
            // falls AFTER the 300 s trust grace on both sides instead of
            // coinciding with it (a ±1-tick knife edge on the held count).
            // The factory clock RUNS from 80/01/06 00:00:00 (the modem's
            // boot), like the SIM7600's: every snapshot implies the same
            // offset, so which 10 s snapshot a check reads cannot change the
            // NITZ target (a constant `cclk` with an advancing `read_at` made
            // two reads 62 ms apart straddling a release 10 s apart,
            // stepC p4-clock-cap.clk.jsrs).
            lte_with_clock(b, dur_ms, &|t| (t >= 15_000).then(|| (factory_cclk(t), 1)));
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
        name if name.starts_with("p5b-") => return add_p5b(name, b, dur_ms, fixtures, m),
        _ => {}
    }
    Ok(m)
}

/// The Phase 5b scenarios (H8; `phase-5b-js-catchup.md` §8 step B).
fn add_p5b(
    scenario: &str,
    b: &mut Builder,
    dur_ms: u64,
    fixtures: &Path,
    mut m: P4Meta,
) -> Result<P4Meta> {
    match scenario {
        // SMS clear (H8, §6): the p4-sms inbox (SM 7/40 + ME 23/23 full + a
        // concat + a SUBMIT) on a FakeModem with `allowClear`; the modem is
        // refilled between the cases (`changes`). Per case the TS reader's
        // queue semantics (ts:src/hardware/sms-modem.ts:598-733 @09da38d).
        "p5b-sms-clear" => {
            let path = fixtures.join("replay/p4-sms-modem.json");
            let doc: Value = serde_json::from_slice(
                &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
            )?;
            let mut modem = doc["modem"].clone();
            let full = modem["stores"].clone();
            let empty = json!({
                "SM": {"total": full["SM"]["total"], "slots": {}},
                "ME": {"total": full["ME"]["total"], "slots": {}},
            });
            modem["allowClear"] = json!(true);
            modem["changes"] = json!([
                // case 3: a slow clear (each delete 4 s) with a read arriving mid-clear
                {"t": 20_000, "stores": full, "delayMs": {"AT+CMGD": 4_000}},
                // case 4: a slow read (each CMGRO 250 ms) with a clear arriving mid-read
                {"t": 45_000, "stores": full, "delayMs": {"AT+CMGRO": 250}},
                // case 5: the ME delete never answers → the 25 s per-store timeout
                {"t": 75_000, "stores": full, "delayMs": {}, "cmgdHang": ["ME"]},
                // case 6: no SIM
                {"t": 115_000, "cpmsError": "+CME ERROR: 10", "cmgdHang": []},
                // case 7: SIM back, both stores empty
                {"t": 125_000, "cpmsError": "", "stores": empty},
            ]);
            m.rig.insert("modem".into(), modem);
            let lim = |n: u32| Some(json!({"limit": n}));
            // case 1: clear the full stores; case 2: the same requestId again
            cmd(b, 8_200, "synth-p5b-clear-1", "clear_sms", lim(10))?;
            cmd(b, 14_200, "synth-p5b-clear-1", "clear_sms", lim(10))?;
            // case 3
            cmd(b, 25_200, "synth-p5b-clear-2", "clear_sms", lim(10))?;
            cmd(b, 26_200, "synth-p5b-read-1", "read_sms", lim(5))?;
            // case 4
            cmd(b, 50_200, "synth-p5b-read-2", "read_sms", lim(50))?;
            cmd(b, 51_200, "synth-p5b-clear-3", "clear_sms", lim(10))?;
            // case 5
            cmd(b, 80_200, "synth-p5b-clear-4", "clear_sms", lim(10))?;
            // case 6
            cmd(b, 118_200, "synth-p5b-clear-5", "clear_sms", lim(10))?;
            // case 7
            cmd(b, 128_200, "synth-p5b-clear-6", "clear_sms", lim(10))?;
        }
        // LED telemetry (H8, X1): every rung's `system.led_state` code and
        // `health_flags` bits (ts:src/lib/led-state.ts:136-262 @09da38d) on a
        // host whose model declares NO status LED (the inert driver: the
        // ladder still resolves, ts:src/agent/led-manager.ts:100-118).
        "p5b-led" => {
            let mut hw = media_hardware_block();
            if let Some(o) = hw.as_object_mut() {
                o.remove("statusLed");
            }
            m.hardware = Some(hw);
            camera_inputs(b);
            // No fix for 15 s: gps-wait (+ the gps gate's liveBlock).
            nmea(b, dur_ms, 15_000, 0);
            let mut rec = rec_namespace("rec-shed-uv");
            rec["telemetry"]["enabled"] = json!(false);
            rec["video"]["cameras"] = json!({"cam1": false});
            rec["video"]["gpsGate"] = json!("always");
            rec["video"]["gpsGateNoProgressSecs"] = json!(120);
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
            let crossings = rail_thermal(b, dur_ms, 90_000..130_000, 210_000..250_000);
            m.rig.insert("powerCrossings".into(), json!(crossings));
            // LTE daemon status: connected, `connecting` 150-170 s (lte_down),
            // `dead_zone` 180-200 s (a settled verdict: no network rung).
            let mut t = 80;
            let mut k = 0i64;
            while t < dur_ms {
                let mut doc = lte_doc(
                    t,
                    18 + k % 7,
                    !(150_000..170_000).contains(&t),
                    &cellular_block(None),
                );
                if (180_000..200_000).contains(&t) {
                    doc = doc.replace("\"status\": \"connected\"", "\"status\": \"dead_zone\"");
                }
                b.push(t, TapeEvent::lte_file(Some(doc.as_bytes())));
                t += 10_000;
                k += 1;
            }
        }
        // Clock trust held, then NTP (H8): the rig's NTP shim says no until
        // 60 s; GPS and NITZ agree with the host clock (no step), so only NTP
        // can establish trust. Telemetry recording on (the recorder is held:
        // `recording.clockHold`, ts:src/recording/manager.ts:439-450).
        "p5b-clock-hold" => {
            // The probe answers 3.5 s after it is asked (timedatectl shim).
            // The held recorder starts on the RecordingManager's first 5 s
            // settings poll after the trust instant (ts:src/recording/
            // manager.ts:327, 353-360); that poll grid sits δ ≈ 1.5-2 s after
            // the clock-trust probe grid (it starts when the retained config
            // is applied, ~1.8 s after boot here). With a fast probe or a
            // 1.5 s one the trust instant fell next to a poll and the recorder
            // started a poll (5 s) apart run to run (JS-vs-JS 1/3; JS-vs-Rust
            // at 1.5 s: 61.77 vs 66.68 s). 3.5 s puts it mid-slot.
            m.rig.insert("ntpProbeDelayMs".into(), json!(3500));
            b.push(20, TapeEvent::NtpProbe { synced: false });
            b.push(60_000, TapeEvent::NtpProbe { synced: true });
            cfg(b, 400, &clock_rec_doc());
        }
        // Clock trust fallback (H8): no sync source for 360 s → the 300 s
        // grace ends (ts:src/agent/clock-trust.ts:126-137), publishing and
        // recording resume on an untrusted clock.
        "p5b-clock-fallback" => {
            // 250 ms NTP probe latency (see p4-clock-cap): the grace check
            // runs after the probe, so it lands 250 ms past tick 300 and
            // 250 ms short of toFixed(0)'s rounding edge.
            m.rig.insert("ntpProbeDelayMs".into(), json!(250));
            b.push(20, TapeEvent::NtpProbe { synced: false });
            nmea(b, dur_ms, u64::MAX, 0);
            lte_with_clock(b, dur_ms, &|t| Some((cclk_at(t, 0), 0)));
            cfg(b, 400, &clock_rec_doc());
        }
        // The rev-guard upgrade exception (H8, ts:src/agent/config-sync.ts:
        // 323-347, ts:src/worker.ts:941-966): the device already applied
        // rev 3 (seeded config-sync.json with a namespace baseline) of a
        // document that also carries `obd`, which this device never held
        // (absent from settings.json AND the baseline) → exactly `obd` is
        // adopted. The redelivery at 30 s adopts nothing.
        "p5b-unheld" => {
            let ts = json!({"stepThresholdSecs": 15});
            let mut home = Map::new();
            home.insert(
                "settings.json".into(),
                json!(json!({"timeSync": ts}).to_string()),
            );
            home.insert(
                "config-sync.json".into(),
                json!(json!({"lastAppliedRev": 3, "namespaces": {"timeSync": ts}}).to_string()),
            );
            m.home = Some(home);
            let obd = json!({"enabled": true, "adapterMac": "00:1D:A5:68:98:8B", "pollHz": 1});
            let doc = json!({"v": 1, "rev": 3, "timeSync": ts, "obd": obd});
            cfg(b, 400, &doc);
            cfg(b, 30_400, &doc);
        }
        // OBD (H8, H12's rig half): the real helper on an emulator per side.
        // 35 s (+ the player's 20 s settle = 55 s < 60 s): the helper's first
        // audit line (60 s after ITS start, python/obd/session.py:75,191)
        // would otherwise race a report on its own phase (JS-vs-JS: 62.6 s vs
        // a 62.48 s report; JS-vs-Rust with a 55 s tape: 62.3 vs 62.16 s).
        // The audit counters are compared on p5b-obd-config (restarts,
        // T8/obd-audit-cycle).
        "p5b-obd-emulator" | "p5b-obd-audit" | "p5b-obd-crank" | "p5b-obd-badproto" => {
            let sc = match scenario {
                "p5b-obd-crank" => "taos-2022-crank",
                "p5b-obd-badproto" => "taos-2022-badproto",
                _ => "taos-2022",
            };
            obd_device(b, &mut m, true);
            m.rig.insert("obd".into(), json!({"scenario": sc}));
            seed_obd_settings(&mut m, &json!({"enabled": true, "adapterMac": OBD_MAC_A}));
        }
        // The `obd` namespace (H8): starts on the env seam (no settings.obd;
        // ts:src/agent/collectors/obd.ts settings precedence), then the cloud
        // sets it, changes the adapter, disables it, reverts it (null) and
        // sends a malformed MAC. SERIAL overrides the adapter throughout.
        "p5b-obd-config" => {
            obd_device(b, &mut m, true);
            m.rig.insert(
                "obd".into(),
                json!({"scenario": "taos-2022", "env": {"XORGATE_OBD_ADAPTER": OBD_MAC_ENV}}),
            );
            let base = obd_base_rec();
            let doc = |rev: i64, obd: Value| {
                let mut d = json!({"v": 1, "rev": rev, "recording": base});
                d["obd"] = obd;
                d
            };
            cfg(b, 400, &json!({"v": 1, "rev": 1, "recording": base}));
            cfg(
                b,
                30_400,
                &doc(2, json!({"enabled": true, "adapterMac": OBD_MAC_A})),
            );
            cfg(
                b,
                60_400,
                &doc(3, json!({"enabled": true, "adapterMac": OBD_MAC_B})),
            );
            cfg(
                b,
                85_400,
                &doc(4, json!({"enabled": false, "adapterMac": OBD_MAC_B})),
            );
            cfg(b, 105_400, &doc(5, Value::Null));
            cfg(
                b,
                125_400,
                &doc(6, json!({"enabled": true, "adapterMac": "zz:bad:mac"})),
            );
        }
        // OBD commands (H8): a scan through the bluetoothctl shim (the real
        // obd_scan.py → obd/scan.py transcript parser), then a forget.
        "p5b-obd-cmds" | "p5b-obd-cmds-noperiph" => {
            let periph = scenario == "p5b-obd-cmds";
            obd_device(b, &mut m, periph);
            let obd = json!({"bluetoothctl": {
                "scan": [
                    format!("[NEW] Device {OBD_MAC_A} OBDII"),
                    format!("[CHG] Device {OBD_MAC_A} RSSI: -58"),
                    "[NEW] Device 66:55:44:33:22:11 66-55-44-33-22-11",
                    "[NEW] Device 00:1D:A5:00:00:09 V-LINK",
                    "[CHG] Device 00:1D:A5:00:00:09 RSSI: -71",
                ],
                "devices": [
                    format!("Device {OBD_MAC_A} OBDII"),
                    "Device 00:1D:A5:00:00:09 V-LINK",
                    "Device 00:1D:A5:00:00:77 OLD-BONDED",
                ],
            }});
            if periph {
                // NO emulator: the adapter is a Bluetooth MAC the rig can never
                // reach (no BlueZ; the shim refuses `pair`), so `obd_forget`
                // lands on the TS's own not_configured latch
                // (ts:src/agent/collectors/obd.ts:758-777) — with a SERIAL
                // seam the re-resolution would restart the helper instead.
                seed_obd_settings(&mut m, &json!({"enabled": true, "adapterMac": OBD_MAC_A}));
            }
            m.rig.insert("obd".into(), obd);
            // No config message: a cloud document landing while the helper
            // starts rewrites settings.json and restarts the helper on one
            // side only (calibration 2026-10-23: the restarted helper then met
            // the emulator mid-session, CF81). The seeded settings carry it all.
            cmd(b, 10_200, "synth-p5b-obd-scan-1", "obd_scan", None)?;
            // Off the 30 s APN-watch grid (ts:src/agent/config-sync.ts:1655-
            // 1675, `if (this.timer) return`): a forget at 60.2 s requested its
            // report within ~50 ms of the agents' 2nd APN tick, so whether that
            // tick logged `modem APN state changed — re-reporting` depended on
            // which landed first on each side (stepC/p5b-obd-cmds.jsrs2). 52.2 s
            // is ≥ 7 s from any APN tick and its debounced report (54.2 s)
            // lands before the next one.
            let forget_at = if periph { 52_200 } else { 20_200 };
            cmd(b, forget_at, "synth-p5b-obd-forget-1", "obd_forget", None)?;
        }
        _ => {}
    }
    Ok(m)
}

const OBD_MAC_A: &str = "00:1D:A5:68:98:8B";
const OBD_MAC_B: &str = "00:1D:A5:00:00:02";
const OBD_MAC_ENV: &str = "00:1D:A5:00:00:01";

/// An OBD tape's device: the media hardware block (cameras listed, NO kvs
/// channels, video off) + `peripherals.obd` when `periph`.
fn obd_device(b: &mut Builder, m: &mut P4Meta, periph: bool) {
    let mut hw = media_hardware_block();
    if periph {
        hw["peripherals"] = json!({"obd": {"adapter": "elm327", "transport": "bt-spp"}});
    }
    m.hardware = Some(hw);
    m.no_channels = true;
    camera_inputs(b);
}

fn obd_base_rec() -> Value {
    json!({
        "telemetry": {"enabled": false, "rateHz": 1},
        "video": {"enabled": false},
        "upload": {"mode": "off"},
    })
}

/// Seed settings.json's `obd` (the namespace the collector reads).
fn seed_obd_settings(m: &mut P4Meta, obd: &Value) {
    let home = m.home.get_or_insert_with(Map::new);
    home.insert(
        "settings.json".into(),
        json!(json!({"obd": obd, "recording": obd_base_rec()}).to_string()),
    );
}

/// `p5b-clock-*`: the telemetry recorder on (video off), so the hold shows on
/// `recording.clockHold` and the recorder start.
fn clock_rec_doc() -> Value {
    json!({"v": 1, "rev": 1, "recording": {
        "telemetry": {"enabled": true, "rateHz": 1},
        "video": {"enabled": false},
        "upload": {"mode": "off"},
        "retention": {"minFreeBytes": 0},
    }})
}

/// [`rail`] plus a thermal window: `get_throttled` 0x80008 (soft temp limit
/// active + occurred) inside `hot`, 0x80000 after (the `exec-thermal` shapes).
fn rail_thermal(
    b: &mut Builder,
    dur_ms: u64,
    low: std::ops::Range<u64>,
    hot: std::ops::Range<u64>,
) -> Vec<u64> {
    let thr = vec!["vcgencmd".to_string(), "get_throttled".to_string()];
    let pmic = vec!["vcgencmd".to_string(), "pmic_read_adc".to_string()];
    let mut crossings = Vec::new();
    let (mut last_low, mut last_th): (Option<bool>, Option<&str>) = (None, None);
    let mut t = 60;
    while t < dur_ms {
        let is_low = low.contains(&t);
        let th = if hot.contains(&t) {
            "throttled=0x80008\n"
        } else if t >= hot.end {
            "throttled=0x80000\n"
        } else {
            "throttled=0x0\n"
        };
        if last_th.is_some_and(|l| l != th) {
            crossings.push(t);
        }
        if last_low.is_some_and(|l| l != is_low) {
            crossings.push(t + 10);
        }
        (last_low, last_th) = (Some(is_low), Some(th));
        b.push(
            t,
            TapeEvent::Exec {
                argv: thr.clone(),
                stdout: th.into(),
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
