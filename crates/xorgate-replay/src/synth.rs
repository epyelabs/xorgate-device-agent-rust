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
//! - `lte-apn`     (re-review R2) the `cellular` (APN) block of the `lte` rewrites
//!   changes for 40 s (t 10 080 .. 50 080: another `apn_applied` +
//!   `changed_at`) and reverts, in a stretch with no other report trigger, so
//!   the 30 s APN watch is the ONLY path to a report: each agent must log
//!   `modem APN state changed — re-reporting` and publish a `boot` report
//!   once per change (twice);
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
//! - `clock`       every 10 s;
//! - `config-envelope` (review-wp7 fix 5; additive, after every older track's
//!   config) envelope + rev-guard paths: rev 3 retained with a valid `hardware`
//!   block + `uploadBucket` (kvs.json backfill, hardware.json persist), rev 4
//!   with the SAME envelope (unchanged / equal-block no-ops, the redelivery
//!   semantics), rev 5 with an INVALID `hardware` block, a non-JSON payload, a
//!   `v:2` payload, a stale rev 2 doc (rev guard skip), then rev 5 again (a
//!   redelivery of the applied rev). Without the `config` track and with a
//!   `conn-drop`, rev 3 is published during the cut, so the agents get it as a
//!   retained delivery on reconnect, and rev 4 waits [`RECONNECT_WORST_MS`]
//!   after the restore so BOTH agents (each retries 2 s after a close)
//!   have reconnected and seen rev 3 first (series 5 item 4);
//! - `commands-sms` one `read_sms` (`args.limit` 5) and one without args;
//! - `sim-mode`    meta env `MODE=sim`: the player runs both agents in sim mode,
//!   so `read_sms` takes the fixture sweep (TS `worker.ts:930`
//!   `config.mode === "real" ? reader : fixtureSmsSweep`; Rust
//!   `command_flow.rs` `SmsSource::Fixture` when mode ≠ real).

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use xorgate_hal::tape::{ConnState, KvsInfo, Tape, TapeEvent, TapeMeta, TapeRecord};

use crate::rewrite::{civil_from_epoch, iso_z, nmea_sentence};

/// Phase 4 scenarios (WP6): records added by scenario name for the `p4` track.
#[path = "synth_p4.rs"]
mod p4tracks;

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
            "lte-apn",
            "exec",
            "dt",
            "imu",
            "config",
            "conn-drop",
            "commands",
            "cli",
            "clock",
            "config-envelope",
            "commands-sms",
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
            "lte-apn",
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
            "config-envelope",
            "commands-sms",
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
    (
        "config-envelope",
        240,
        // `nmea` (the calibration track): with a silent tty the TS gps
        // watchdog reopens every 30 s and each reopen leaks a blocked libuv
        // read; after 3 Node's 4-thread pool is exhausted and the JS agent
        // wedges (wp7-rerun.md series 4/5) — a TS defect, not this scenario's
        // subject. `lte` (the calibration track): without the daemon's LTE
        // file `gnssAntBias` (modem-poll) stays `adjusted`, so each agent
        // re-reports on its OWN 40 s settle chain (config-sync.ts:1639-1650,
        // sync.rs:592) anchored on its post-cut report — 7 s apart with the
        // old Rust 1→60 s backoff, ≤ ~2 s with the 2 s reconnect — and a chain tick merging with a tape delivery on
        // one agent only splits the report count (series 5, 175133Z).
        &["config-envelope", "conn-drop", "clock", "nmea", "lte"],
        "hardware/uploadBucket envelope (rev 3 retained across a cut), malformed, stale rev",
    ),
    (
        "sms-sim",
        60,
        &["sim-mode", "commands-sms", "clock"],
        "read_sms in MODE=sim: the fixture sweep on …/sms (telemetry is O7 sim)",
    ),
    // ---- Phase 3 (WP6): recording + media. Every one carries `media-hw`
    // (CM5 dual-imx290 hardware block, KVS channels cam0/cam1, the upload
    // identity, the `rpicam-hello --list-cameras` answer) and `rec` (the
    // recording config, retained, at t = 400 ms; scenario variants below).
    (
        "media",
        60,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-quiet", "clock"],
        "≤60 s media tape: both cameras record (720p15, 10 s fragments) + KVS masters + uploads",
    ),
    (
        "rec-boot-defer",
        150,
        &["media-hw", "rec", "nmea-late-fix", "lte-gate", "exec-quiet", "clock"],
        "boot defer + GPS/LTE gate held (LTE connecting, no fix) then released at 80 s",
    ),
    (
        "rec-shed-uv",
        180,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-rail", "clock"],
        "rail 5.10 V < railLowVolts 5.2 → rung 1 → rung 2 (bridge-only), restored from 110 s",
    ),
    (
        "rec-shed-thermal",
        150,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-thermal", "clock"],
        "firmware soft-temp bit 0x8 held 90 s: rung 1 only, recording never stops",
    ),
    (
        "rec-disk",
        120,
        &["media-hw", "rec", "nmea-fixonly", "nmea-prefix", "lte", "exec-quiet", "clock"],
        "600 MiB filler session + maxBytes 512 MiB, uploads off: oldest-first eviction",
    ),
    (
        "rec-upload-outage",
        180,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-quiet", "rec-cut", "clock"],
        "MQTT cut 50→90 s (uploader pauses) + S3 503 window 100→130 s (backoff), then resume",
    ),
    (
        "rec-cameras",
        150,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-quiet", "clock"],
        "cam1 disabled by settings at 50 s (capture + master parked), re-enabled at 100 s",
    ),
    (
        "rec-preset",
        150,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-quiet", "clock"],
        "cam0 capture SIGKILLed (external) at 35 s, cam1 master exits 1 at 45 s, preset → 1080p15 at 70 s",
    ),
    // ---- Phase 6 P3a: cross-agent takeover (P3a-takeover.md §2). Each
    // carries `meta.rig.handoff` (the instant + the inherited state it must
    // leave, `takeover_rig`); the player hands off only with
    // `--handoff-signal`, otherwise the tape plays as an ordinary media run.
    (
        "p6-takeover-open",
        170,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-quiet", "clock"],
        "P3a (a): both recorders + healthy uploads; handoff at 100 s mid-fragment / mid-segment (open sessions, no backlog)",
    ),
    (
        "p6-takeover-vbacklog",
        170,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-quiet", "clock"],
        "P3a (b): video only; fake-S3 503 from 30 s to handoff + 10 s; handoff at 100 s (closed fragments never uploaded + the active one)",
    ),
    (
        "p6-takeover-tbacklog",
        230,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-quiet", "clock"],
        "P3a (c): telemetry only; fake-S3 503 from 20 s to handoff + 10 s; handoff at 150 s (2 closed segments never uploaded + the open one)",
    ),
    (
        "p6-takeover-inflight",
        170,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-quiet", "clock"],
        "P3a in-flight: both recorders; an mp4 PUT arriving in [92 s, 100 s) is held mid-transfer (its sidecar already PUT); SIGKILL handoff at 100 s aborts it",
    ),
    (
        "p6-takeover-reap",
        800,
        &["media-hw", "rec", "nmea-fixonly", "lte", "exec-quiet", "clock"],
        "P3a (a)+(b)+(c) with a 650 s tail: the drained X-era session dirs pass REAP_MIN_AGE_MS (10 min) under the next agent",
    ),
    // ---- Phase 4 (WP6, phase-4-operations.md §8): every one carries `p4`
    // (records by scenario name, synth_p4.rs); the rig side is play.rs/p4.rs.
    (
        "p4-led-ladder",
        180,
        &["media-hw", "p4", "lte", "clock"],
        "LED: starting → gps-wait → healthy → network (cut 40→60 s) → flash2 (real config change at 75.7 s; the 85.7 s redelivery does not flash) → undervoltage (rail 100→140 s, liveBlock first) → healthy",
    ),
    (
        "p4-sms",
        60,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "read_sms on a FakeModem pty: bench inbox + concat UCS2 + SUBMIT, text mode → PDU → restored; reuse window; limit",
    ),
    (
        "p4-clock-gps",
        120,
        &["p4", "clock"],
        "clock stepper: NTP unsynced, GPS +600 s with a fix → one gps step; NTP synced at 90 s → stood down",
    ),
    (
        "p4-clock-nitz",
        120,
        &["p4", "clock"],
        "clock stepper: no GPS fix, NITZ +600 s unregistered (reg 0) then registered (reg 5) → one nitz step; NTP at 100 s",
    ),
    (
        "p4-clock-cap",
        700,
        &["p4", "clock"],
        "clock stepper: implausible NITZ (80/01/06) → warn + hold-off, GPS +600 s from 200 s → step 1/1 (maxStepsPerBoot 1), then the cap warning",
    ),
    (
        "p4-cam-mount",
        300,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "camera mount: cameraMount cam0 0→180 (pending-reboot), DT rotation 180 at 100 s (applied), →0 at 170.7 s, DT 0 at 230 s",
    ),
    (
        "p4-update-cmd",
        60,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "remote rail: cloud update / reboot / power_off → sudo argv (the shim never runs them)",
    ),
    // ---- Phase 5b (WP4, phase-5b-js-catchup.md H8): also on the `p4` track
    // (records by scenario name, synth_p4.rs `add_p5b`).
    (
        "p5b-sms-clear",
        150,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "clear_sms on a FakeModem with allowClear: SM+ME full → cleared; same requestId again = ledger duplicate (no 2nd CMGD); read during a slow clear waits; clear during a slow read queues; ME delete hang → 25 s timeout, mem1 restored, error; no SIM; empty stores",
    ),
    (
        "p5b-led",
        280,
        &["media-hw", "p4", "clock"],
        "LED telemetry on an LED-less host: starting → gps-wait + gps-gate (0-15 s) → healthy → network (cut 40→60 s) → undervoltage shed (rail 90→130 s) → LTE connecting (150→170 s) → dead_zone (180→200 s) → thermal shed (210→250 s) → healthy",
    ),
    (
        "p5b-clock-hold",
        120,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "clock trust: NTP unsynced until 60 s (no GPS/NITZ step: the sources agree with the host) → live publish withheld, seq gap, telemetry recorder held (clockHold) → trusted at 60 s, recorder starts",
    ),
    (
        "p5b-clock-fallback",
        360,
        &["p4", "clock"],
        "clock trust: no NTP, no GPS fix, NITZ unregistered for 360 s → held, the 300 s fallback warning, publishing + recording resume flagged (hints.clockTrust fallback)",
    ),
    (
        "p5b-unheld",
        60,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "config-sync upgrade exception: retained rev 3 already applied (seeded sync baseline) carrying a never-held `obd` namespace → exactly `obd` adopted; a 2nd delivery adopts nothing",
    ),
    (
        "p5b-obd-emulator",
        35,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "OBD: peripherals.obd + an ELM327-emulator per agent (taos-2022) on XORGATE_OBD_SERIAL, the real helper → obd group at link 2 with the Taos values, effective.obd, state.json obd",
    ),
    (
        "p5b-obd-audit",
        150,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "OBD (review-wp4 R4-3e): taos-2022 for 150 s in ONE helper run → the helper's 60 s audits #1 and #2 → the agent's audit-MEASURED request rate (ts:src/agent/collectors/obd.ts:270-278, 666-677) in state.json obd.reqPerSec, JS vs Rust",
    ),
    (
        "p5b-obd-crank",
        35,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "OBD: taos-2022-crank (start-stop auto-stop + one crank voltage dip) → the helper must not stop",
    ),
    (
        "p5b-obd-badproto",
        90,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "OBD: taos-2022-badproto (ATDPN A3) → helper exit 4 → parked 600 s",
    ),
    (
        "p5b-obd-config",
        150,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "OBD namespace: env seam (no settings.obd) → set → adapter change (helper restart) → enabled:false (link 0, disabled) → null (back to the env seam) → bad MAC dropped (adjusted)",
    ),
    (
        "p5b-obd-cmds",
        100,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "OBD commands with the peripheral: obd_scan via the bluetoothctl shim (candidates in effective.obd.scan), obd_forget → bond removed, not_configured",
    ),
    (
        "p5b-obd-cmds-noperiph",
        40,
        &["p4", "nmea-fixonly", "lte", "clock"],
        "OBD commands without peripherals.obd: obd_scan and obd_forget → rejected (no OBD peripheral)",
    ),
];

/// The Phase 3 scenarios (all carry `media-hw`).
#[cfg(test)]
pub fn is_phase3(scenario: &str) -> bool {
    SCENARIOS
        .iter()
        .any(|s| s.0 == scenario && s.2.contains(&"media-hw") && !s.2.contains(&"p4"))
}

thread_local! {
    /// Start wall of the tape being built: [`SYNTH_START_WALL`], or the base
    /// tape's when splicing (`--base`), so embedded NMEA/command/LTE times line
    /// up with the meta `startWall` the player shifts from.
    static W0: std::cell::Cell<i64> = const { std::cell::Cell::new(SYNTH_START_WALL) };
}

/// exec-rail: the rail goes low here (≥ 3 s after boot: no shed crossing in
/// the boot sequence) and clean again at RAIL_CLEAN_FROM_MS.
const RAIL_LOW_FROM_MS: u64 = 10_000;
const RAIL_CLEAN_FROM_MS: u64 = 110_000;

fn w0() -> i64 {
    W0.with(|c| c.get())
}

/// After a cut ends, the latest any agent can be reconnected: both retry a
/// FIXED 2000 ms after every close (mqtt.js 5.15.1 `_setupReconnect`,
/// `reconnectPeriod: 2000` at `mqtt.ts:63` ↔ `xorgate-cloud` backoff.rs
/// `BackoffConfig::fixed`), and the rig's proxy refuses an attempt inside a
/// span by closing it at once (`play.rs` `proxy`), so no attempt is still
/// hanging at the restore (no 30 s connect timeout): the worst case is an
/// attempt refused just before the restore → 2000 ms → a connect that
/// succeeds. + 5 s (kept from the old bound) for TCP + TLS + CONNACK,
/// resubscribe and the retained delivery on a loaded VM (JS measured 0.4 s
/// from the restore to `connected`, wp6.md rec-upload-outage).
pub const RECONNECT_WORST_MS: u64 = 2_000 + 5_000;

/// `rec-boot-defer`: when the GPS fix appears (LTE turns `connected` 10 s
/// earlier, on the 10 s rewrite cadence).
pub const REC_GATE_RELEASE_MS: u64 = 80_000;

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
/// `nmea-prefix` (rec-disk only; review-wp5 R-WP5-3): the first fix burst
/// at this tape time; the bursts before it carry no fix. On rec-disk the
/// first recorder sample lands early (~1.2-1.4 s, still the LED's `starting`
/// telemetry), so with a fix from the first burst whether the next sample's
/// LED tick saw `gps_no_fix` (health_flags 16, ts:src/lib/led-state.ts:249,
/// :286) was a coin flip per side — a one-sample transient on one side only
/// (`rec-disk.jsrs.fin`, `.cal.fin`, review run `.jsrs.rev1`). The gps port
/// opens at the agent's first read (~1.9 s), so a fix from 4.9 s leaves ≥ 2
/// recorder ticks of `gps_no_fix` on BOTH sides before the transition; only
/// how many samples carry it may differ, which the per-leaf value sequence
/// (run-length deduplicated) does not see.
const NMEA_PREFIX_MS: u64 = 4_900;

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

/// The daemon's `cellular` block (APN machinery): the steady one, or the
/// `lte-apn` track's changed one (another applied APN since `changed_at`).
fn cellular_block(changed_at: Option<u64>) -> String {
    match changed_at {
        None => {
            r#"{"state": "in-force", "apn_applied": "synth.apn", "fails": 0, "changed_at": null}"#
                .to_string()
        }
        Some(c) => format!(
            r#"{{"state": "in-force", "apn_applied": "{LTE_APN_ALT}", "fails": 0, "changed_at": "{}"}}"#,
            iso_z(wall_secs(c))
        ),
    }
}

/// `lte-apn`: the APN the `cellular` block carries while changed.
pub const LTE_APN_ALT: &str = "synth-b.apn";
/// `lte-apn`: the LTE rewrites (every 10 s from t=80) with the changed block:
/// k = 1..=4, i.e. t 10 080 .. 40 080, reverted by the k = 5 rewrite at
/// 50 080 — held 40 s (> 35 s, so the 30 s APN watch sees it on both agents),
/// inside the first quiet stretch of calibration/full (boot report ≈ 3 s,
/// the first refresh at 72 s / 180 s): no other report trigger between the
/// change and the watch tick that sees the revert (≈ 60.5 s + 2 s debounce).
pub const LTE_APN_CHANGED_K: std::ops::RangeInclusive<i64> = 1..=4;

fn lte_doc(t: u64, csq: i64, ok: bool, cellular: &str) -> String {
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
        "{{\n  \"timestamp\": \"{ts}\",\n  \"csq\": {csq},\n  \"rssi_dbm\": {rssi},\n  \"csq_interval\": 30,\n  \"status\": \"{status}\",\n  \"reg\": 1,\n  \"iface\": \"usb0\",\n  \"ip\": \"192.0.2.37\",\n  \"xtra_valid_min\": 8622,\n  \"gnss_ant_bias\": 1,\n  \"modem_temp\": {temp},\n  \"modem_clock\": {{\"cclk\": \"{cclk}\", \"read_at\": {now}, \"reg\": 1}},\n  \"gnss\": {{\n    \"engine_on\": 1,\n    \"has_fix\": 1,\n    \"fix_checked_at\": \"{fixat}\",\n    \"ttff_s\": 32,\n    \"last_engine_start\": \"{start}\",\n    \"last_engine_restart\": null,\n    \"standalone_fallback\": 0,\n    \"no_fix_since\": null,\n    \"wedge_restarts_1h\": 0,\n    \"health\": \"ok\"\n  }},\n  \"recovery\": {{\n    \"connect_fails\": 0,\n    \"radio_cycles_since_ok\": 0,\n    \"resets_24h\": 0,\n    \"last_reset\": null,\n    \"last_gate_skip\": null,\n    \"last_lte_ok\": \"{ts}\"\n  }},\n  \"sim\": {{\"present\": 1, \"state\": \"ready\", \"iccid\": \"8900000000000000000\", \"msisdn\": \"+10000000000\", \"carrier\": \"Synth Mobile\", \"act\": \"lte\", \"registration\": \"home\", \"checked_at\": \"{fixat}\"}},\n  \"cellular\": {cellular},\n  \"uplink\": {{\n    \"primary\": \"wlan0\",\n    \"modem_only\": 0\n  }}\n}}\n",
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

/// The valid hardware block the `config-envelope` track delivers: the legacy
/// CM5 wiring (sw encoder, rails, IMU kept, no LED, no cameras), so nothing the
/// rig feeds changes meaning (`hardware-config.ts` `parseHardwareBlock`).
pub fn synth_hardware_block() -> Value {
    json!({
        "v": 2, "platform": "rpi-cm5", "encoder": "sw-h264", "rails": true,
        "statusLed": null, "imu": "bno085", "video": [],
    })
}

/// The bucket the `config-envelope` track backfills into kvs.json.
pub const SYNTH_UPLOAD_BUCKET: &str = "xorgate-synth-uploads";

/// [`config_doc`] with extra envelope keys merged in at the end.
fn config_doc_with(rev: i64, step: u32, enabled_mount: bool, extra: Value) -> String {
    let mut v: Value =
        serde_json::from_str(&config_doc(rev, step, enabled_mount)).expect("config_doc is JSON");
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, x) in e {
            o.insert(k.clone(), x.clone());
        }
    }
    v.to_string()
}

/// The CM5 bench's `rpicam-hello --list-cameras` (fixtures/cameras
/// enumeration.json case 0: RP1 i2c@88000 = csi0, i2c@70000 = csi1).
pub const CM5_CAMERA_LIST: &str = "Available cameras\n-----------------\n0 : imx290 [1920x1080 12-bit RGGB] (/base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a)\n    Modes: 'SRGGB10_CSI2P' : 1280x720 [30.00 fps]\n1 : imx290 [1920x1080 12-bit RGGB] (/base/axi/pcie@1000120000/rp1/i2c@70000/imx290@1a)\n    Modes: 'SRGGB10_CSI2P' : 1280x720 [60.00 fps]\n";

/// The bench's hardware block (dual imx290 on the CM5, sw encoder, no IMU so
/// the media tapes never probe I2C).
pub fn media_hardware_block() -> Value {
    json!({
        "v": 2, "platform": "rpi-cm5", "encoder": "sw-h264", "rails": true,
        "statusLed": null, "imu": null,
        "video": [
            {"key": "cam0", "connector": "csi0", "sensor": "imx290"},
            {"key": "cam1", "connector": "csi1", "sensor": "imx290"},
        ],
    })
}

/// The tape's KVS channels (the player substitutes the rig deviceId).
pub fn media_channels() -> Vec<Value> {
    ["cam0", "cam1"]
        .iter()
        .map(|k| json!({"streamKey": k, "channelName": format!("xorgate-{SYNTH_DEVICE_ID}-{k}")}))
        .collect()
}

/// The Phase 3 base recording config (cloud `recording` namespace): both
/// recorders on, 10 s fragments (the clamp floor) so a short tape closes
/// several, no boot defer / gate, uploads on, retention never triggered by
/// the rig VM's own free space (`minFreeBytes: 0`).
pub fn rec_namespace(scenario: &str) -> Value {
    let mut rec = json!({
        "telemetry": {"enabled": true, "rateHz": 1},
        "video": {
            "enabled": true, "preset": "720p15-1mbps", "segmentSeconds": 10,
            "bootDefer": "off", "gpsGate": "off", "spawnStaggerSecs": 4,
        },
        "upload": {"mode": "auto"},
        "retention": {"minFreeBytes": 0},
    });
    match scenario {
        "rec-boot-defer" => {
            rec["video"]["bootDefer"] = json!("always");
            rec["video"]["bootDeferMaxSecs"] = json!(20);
            rec["video"]["gpsGate"] = json!("always");
            rec["video"]["gpsGateNoProgressSecs"] = json!(120);
        }
        "rec-shed-uv" => {
            rec["powerShedding"] = json!({
                "enabled": true, "railLowVolts": 5.2, "railLowSecs": 5,
                "escalateDwellSecs": 20, "escalateDutyPct": 70, "restoreSecs": 20,
                "railRestoreVolts": 5.25,
            });
        }
        "rec-shed-thermal" => {
            rec["powerShedding"] = json!({
                "enabled": true, "escalateDwellSecs": 20, "restoreSecs": 20,
            });
        }
        "rec-disk" => {
            rec["upload"] = json!({"mode": "off"});
            rec["retention"] = json!({"maxBytes": 536_870_912u64, "minFreeBytes": 0});
        }
        "p6-takeover-vbacklog" => {
            rec["telemetry"]["enabled"] = json!(false);
        }
        "p6-takeover-tbacklog" => {
            rec["video"]["enabled"] = json!(false);
        }
        _ => {}
    }
    rec
}

/// P3a handoff instants (tape ms), off every rotation: cam0's 10 s fragments,
/// measured on the rig at ≈ 3.2 s + 10k (tape), cam1's at ≈ 6.7 s + 10k (L1
/// 2026-11-07: 95 s cut cam0 1.8 s into a fragment → INVALID; a multiple of
/// 10 s is ≈ 6.8 s / 3.3 s into them),
/// cam1's (staggered 4 s), the 60 s telemetry rotation (`segments.ts:45`)
/// and the fault-window edges. `takeover.rs` `edge` re-checks on each run.
pub const TAKEOVER_OPEN_AT_MS: u64 = 100_000;
pub const TAKEOVER_VBACKLOG_AT_MS: u64 = 100_000;
pub const TAKEOVER_TBACKLOG_AT_MS: u64 = 150_000;
pub const TAKEOVER_REAP_AT_MS: u64 = 150_000;
pub const TAKEOVER_INFLIGHT_AT_MS: u64 = 100_000;
/// The in-flight tape's hold window opens this long before the handoff: two
/// cameras close a fragment every ≤ 6 s (10 s fragments, 4 s stagger), so at
/// least one mp4 PUT starts inside it, and the single-flight uploader sends
/// nothing after it (`uploader.ts:2,11,136-213` @09da38d), so exactly one is held.
pub const TAKEOVER_INFLIGHT_LEAD_MS: u64 = 8_000;
/// Held far past the handoff (the SIGKILL lands inside it; both agents' PUT
/// timeouts are 60 s, `s3-put.ts:23-24,115`).
pub const TAKEOVER_INFLIGHT_HOLD_MS: u64 = 20_000;
/// How far past the handoff (tape time is frozen across it) the outage
/// lasts. 0 (L5 2026-11-07): the bucket is healthy again by the time the next
/// agent boots, so its first drain of the inherited backlog lands inside the
/// boot's startup window. With a 10 s tail each side drained ~12 s after the
/// restart on its OWN retry timer and its 1 Hz tick sampled the ~100 ms drain
/// at a different point (`recording.backlogSegments` 12 vs 9, both ticks
/// `uploading: true`) — a timing race no row covers, so the tape avoids it.
/// The cost: the next agent never meets 503s on the inherited backlog (an
/// Unverified item of P3a-takeover.md).
pub const TAKEOVER_OUTAGE_TAIL_MS: u64 = 0;

/// `meta.rig` of a P3a tape: `handoff {atMs, segMs, expect}` + the fake-S3
/// outage. `expect` names `takeover::classes` aggregates (signal-independent:
/// a SIGTERM lets X finalize what a SIGKILL leaves open): `min` / `max`
/// counts per side, and `reaped` (no X-era video file may be left at the end).
pub fn takeover_rig(name: &str) -> Value {
    let outage = |from: u64, at: u64| json!([{"fromMs": from, "toMs": at + TAKEOVER_OUTAGE_TAIL_MS, "status": 503}]);
    let (at, faults, expect) = match name {
        "p6-takeover-open" => (
            TAKEOVER_OPEN_AT_MS,
            None,
            // An open session per camera (its manifest) + cam0's active mp4
            // (≈ 6.8 s old at the cut, non-empty). cam1's (≈ 3.3 s) may still
            // be 0 bytes — gst holds it — and then the drop-empty path of the
            // agent that sweeps it (X at a SIGTERM, the next one after a
            // SIGKILL) removes it: legitimate either way (L1 2026-11-07).
            json!({
                "min": {"video/cam0: mp4": 1, "video/cam0: session.json": 1, "video/cam1: session.json": 1, "telemetry: segment": 1},
                "max": {"video/cam0: mp4": 1, "video/cam1: mp4": 1, "telemetry: segment": 1},
            }),
        ),
        "p6-takeover-vbacklog" => (
            TAKEOVER_VBACKLOG_AT_MS,
            Some(outage(30_000, TAKEOVER_VBACKLOG_AT_MS)),
            json!({
                "min": {"video/cam0: mp4": 5, "video/cam1: mp4": 5},
                "max": {"telemetry: segment": 0},
            }),
        ),
        "p6-takeover-tbacklog" => (
            TAKEOVER_TBACKLOG_AT_MS,
            Some(outage(20_000, TAKEOVER_TBACKLOG_AT_MS)),
            json!({
                "min": {"telemetry: segment": 3},
                "max": {"video/cam0: mp4": 0, "video/cam1: mp4": 0},
            }),
        ),
        "p6-takeover-inflight" => (
            TAKEOVER_INFLIGHT_AT_MS,
            None,
            json!({
                "min": {"video/cam0: mp4": 1, "video/cam0: session.json": 1, "video/cam1: session.json": 1, "telemetry: segment": 1},
                "inflight": 1,
            }),
        ),
        _ => (
            TAKEOVER_REAP_AT_MS,
            Some(outage(30_000, TAKEOVER_REAP_AT_MS)),
            json!({
                "min": {"video/cam0: mp4": 10, "video/cam1: mp4": 10, "telemetry: segment": 3},
                "reaped": true,
            }),
        ),
    };
    let mut rig = json!({"handoff": {"atMs": at, "segMs": 10_000, "expect": expect}});
    if let Some(f) = faults {
        rig["s3Faults"] = f;
    }
    if name == "p6-takeover-inflight" {
        rig["s3Holds"] = json!([{
            "fromMs": at - TAKEOVER_INFLIGHT_LEAD_MS,
            "toMs": at,
            "holdMs": TAKEOVER_INFLIGHT_HOLD_MS,
            "suffix": ".mp4",
        }]);
    }
    rig
}

/// The repo fixtures (tests of other modules build synth tapes too).
#[cfg(test)]
pub fn test_fixtures() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn rec_doc(rev: i64, rec: &Value) -> String {
    json!({"v": 1, "rev": rev, "recording": rec}).to_string()
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
    let base_owned = ["lte", "lte-apn", "lte-fixtures", "exec", "dt", "clock"];
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

    // ---- config-envelope (additive; see the module doc)
    if has("config-envelope") {
        let standalone = !has("config");
        let env_ok =
            json!({"uploadBucket": SYNTH_UPLOAD_BUCKET, "hardware": synth_hardware_block()});
        let mut bad_hw = synth_hardware_block();
        bad_hw["encoder"] = json!("hw-h265"); // unknown enum → parse null
        let env_bad = json!({"uploadBucket": SYNTH_UPLOAD_BUCKET, "hardware": bad_hw});
        let doc3 = config_doc_with(3, 13, true, env_ok.clone());
        let doc5 = config_doc_with(5, 14, true, env_bad);
        let cfg = |b: &mut Builder, t: u64, doc: &[u8], retain: bool| {
            b.push(t, TapeEvent::mqtt_in(&topic("config"), doc, retain, 1));
        };
        let e = if standalone && drop {
            // rev 3 while the broker path is cut: the broker holds it and the
            // agents get it as a retained delivery when they resubscribe;
            // rev 4 only once both have (RECONNECT_WORST_MS).
            cfg(&mut b, drop_at + 5_000, doc3.as_bytes(), true);
            cfg(&mut b, drop_end + 300, doc3.as_bytes(), true);
            drop_end + RECONNECT_WORST_MS
        } else {
            let e = pct(75) + 400;
            cfg(&mut b, e, doc3.as_bytes(), true);
            e + 6_000
        };
        // Same envelope, new rev: uploadBucket "unchanged", equal block.
        cfg(
            &mut b,
            e,
            config_doc_with(4, 14, true, env_ok).as_bytes(),
            false,
        );
        cfg(&mut b, e + 6_000, doc5.as_bytes(), false);
        cfg(&mut b, e + 12_000, br#"{"v":1,"rev":6,"timeSync":{"#, false);
        cfg(&mut b, e + 15_000, br#"{"v":2,"rev":7}"#, false);
        // Stale: would revert imuMount + the step if applied.
        cfg(
            &mut b,
            e + 18_000,
            config_doc(2, 11, false).as_bytes(),
            false,
        );
        cfg(&mut b, e + 21_000, doc5.as_bytes(), true);
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
                stderr: None,
            },
        );
        b.push(
            uv_at,
            TapeEvent::Exec {
                argv: thr.clone(),
                stdout: "throttled=0x50005\n".into(),
                code: Some(0),
                stderr: None,
            },
        );
        b.push(
            uv_at + 10_000,
            TapeEvent::Exec {
                argv: thr,
                stdout: "throttled=0x50000\n".into(),
                code: Some(0),
                stderr: None,
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
                    stderr: None,
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
            let first_changed = 80 + 10_000 * *LTE_APN_CHANGED_K.start() as u64;
            let cellular = cellular_block(
                (has("lte-apn") && LTE_APN_CHANGED_K.contains(&k)).then_some(first_changed),
            );
            b.push(
                t,
                TapeEvent::lte_file(Some(lte_doc(t, csq, true, &cellular).as_bytes())),
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
    } else if has("nmea-late-fix") {
        Some("late")
    } else {
        None
    };
    if let Some(mode) = nmea_mode {
        let fix_from = match mode {
            // review-wp5 R-WP5-3: a no-fix lead-in (see NMEA_PREFIX_MS).
            "fix" if has("nmea-prefix") => NMEA_PREFIX_MS,
            "fix" => 0,
            "nofix" => u64::MAX,
            // rec-boot-defer: the GPS half of the gate's release.
            "late" => REC_GATE_RELEASE_MS,
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

    // ---- commands-sms: read_sms with and without args.limit
    if has("commands-sms") {
        let at = pct(45) + 200;
        let mut c: Value = serde_json::from_str(&command("synth-sms-1", "read_sms", at, 60_000))?;
        c["args"] = json!({"limit": 5});
        b.push(
            at,
            TapeEvent::mqtt_in(&topic("command"), c.to_string().as_bytes(), false, 1),
        );
        let t2 = at + 6_000;
        b.push(
            t2,
            TapeEvent::mqtt_in(
                &topic("command"),
                command("synth-sms-2", "read_sms", t2, 60_000).as_bytes(),
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

    // ---- Phase 3 tracks
    let mut rig: Option<Value> = None;
    let mut home: Option<serde_json::Map<String, Value>> = None;
    if has("media-hw") {
        b.push(
            40,
            TapeEvent::Exec {
                argv: vec!["rpicam-hello".into(), "--list-cameras".into()],
                stdout: CM5_CAMERA_LIST.into(),
                code: Some(0),
                stderr: None,
            },
        );
    }
    if has("rec") {
        let rec = rec_namespace(name);
        b.push(
            400,
            TapeEvent::mqtt_in(&topic("config"), rec_doc(1, &rec).as_bytes(), true, 1),
        );
        let later = |b: &mut Builder, t: u64, rev: i64, f: &dyn Fn(&mut Value)| {
            let mut r = rec.clone();
            f(&mut r);
            b.push(
                t,
                TapeEvent::mqtt_in(&topic("config"), rec_doc(rev, &r).as_bytes(), true, 1),
            );
        };
        match name {
            "rec-cameras" => {
                later(&mut b, 50_000, 2, &|r| {
                    r["video"]["cameras"] = json!({"cam1": false})
                });
                later(&mut b, 100_000, 3, &|r| {
                    r["video"]["cameras"] = json!({"cam1": true})
                });
            }
            "rec-preset" => {
                later(&mut b, 70_000, 2, &|r| {
                    r["video"]["preset"] = json!("1080p15-2mbps")
                });
                b.push(
                    35_000,
                    TapeEvent::ChildExit(xorgate_hal::tape::ChildExitRec {
                        role: xorgate_hal::tape::ROLE_CAPTURE.into(),
                        key: "cam0".into(),
                        pid: 0,
                        code: None,
                        signal: Some(9),
                        external: true,
                    }),
                );
                b.push(
                    45_000,
                    TapeEvent::ChildExit(xorgate_hal::tape::ChildExitRec {
                        role: xorgate_hal::tape::ROLE_KVS.into(),
                        key: "cam1".into(),
                        pid: 0,
                        code: Some(1),
                        signal: None,
                        external: true,
                    }),
                );
            }
            "rec-disk" => {
                // `trimLastBytes` (2026-10-19, JS-vs-Rust `replay-20261019T153028Z`
                // + `…T134429Z`): with the full 600 MiB the 10th eviction's
                // decisive scan is the cam0 rotation at ~t 92 s, where the
                // closed backlog sits 72 946 B under maxBytes and the OPEN
                // files decide — mostly cam1's growing mp4 (~6.0 s old,
                // ≤ ~71 KB of content, flushed in filesink blocks) + the
                // active telemetry .jsonl (~19 KB). JS (cam1 6.07 s old)
                // went over, Rust (5.98 s) did not, and evicted one rotation
                // later (TS `disk.ts:113-168` ↔ `disk_plan.rs:120-176` sum
                // every scanned file, a faithful port): ~0.1 s of gst
                // pipeline start jitter decided a contract value. 40 KiB off
                // the newest filler fragment moves the crossing to the next
                // (cam1) rotation: the cam0 one stays ≥ ~21 KB under even
                // with all of cam1's content on disk, the cam1 one is over on
                // its closed backlog alone (+3 KB) plus the active telemetry
                // segment (~22 KB). Eviction ORDER / counts are unchanged in
                // kind; only the instant moves off the knife edge.
                rig = Some(
                    json!({"filler": {"mib": 600, "fragmentMib": 10, "ageDays": 7, "key": "cam0", "trimLastBytes": 40_960}}),
                );
                // Both agents BOOT with the scenario's recording namespace
                // already in settings.json (as a fielded device does after
                // its first config). Without it the boot runs on the
                // defaults (upload auto) until the rev-1 config lands at
                // t = 400 ms, and how many filler fragments get uploaded in
                // that window depends on each agent's boot speed (JS-vs-Rust
                // 2026-10-19 `replay-20261019T133846Z`: JS 1 pair, Rust 5 —
                // a race, not a parity difference), which then shifts the
                // eviction set and `droppedSegments`.
                let mut files = serde_json::Map::new();
                files.insert(
                    "settings.json".into(),
                    Value::String(json!({ "recording": rec }).to_string()),
                );
                home = Some(files);
            }
            "rec-upload-outage" => {
                rig = Some(
                    json!({"s3Faults": [{"fromMs": 100_000, "toMs": 130_000, "status": 503}]}),
                );
            }
            n if n.starts_with("p6-takeover-") => {
                rig = Some(takeover_rig(n));
            }
            _ => {}
        }
    }
    if has("rec-cut") {
        b.push(
            50_000,
            TapeEvent::Conn {
                state: ConnState::Closed,
            },
        );
        b.push(
            90_000,
            TapeEvent::Conn {
                state: ConnState::Connected,
            },
        );
    }
    if has("lte-gate") {
        // LTE `connecting` until the release, `connected` after (the LTE half
        // of the gate; D12's seam on the bench).
        let mut t = 80;
        let mut k = 0i64;
        while t < dur_ms {
            let ok = t >= REC_GATE_RELEASE_MS - 10_000;
            b.push(
                t,
                TapeEvent::lte_file(Some(
                    lte_doc(t, 18 + k % 7, ok, &cellular_block(None)).as_bytes(),
                )),
            );
            t += 10_000;
            k += 1;
        }
    }
    let mut crossings: Vec<u64> = Vec::new();
    let exec_kind = ["exec-quiet", "exec-rail", "exec-thermal"]
        .into_iter()
        .find(|k| has(k));
    if let Some(kind) = exec_kind {
        let thr = vec!["vcgencmd".to_string(), "get_throttled".to_string()];
        let pmic = vec!["vcgencmd".to_string(), "pmic_read_adc".to_string()];
        let throttled = |t: u64| match kind {
            "exec-thermal" if (20_000..110_000).contains(&t) => "throttled=0x80008\n",
            "exec-thermal" if t >= 110_000 => "throttled=0x80000\n",
            _ => "throttled=0x0\n",
        };
        // exec-rail (review-wp6 re-check, orchestrator: a RIG knife edge, like
        // rec-disk's trimLastBytes): a shed decision reads a 1 s cache whose
        // owner (shed tick or recorder) flips at random in the TS (D30), so a
        // read may be up to one tick stale. The rail is therefore CONSTANT on
        // each side of a threshold (no ±0.03 V wobble: a stale read returns
        // the same value and the same event text), clean (5.30 V) for the
        // first RAIL_LOW_FROM_MS (no crossing at boot), low (5.10 V) until
        // RAIL_CLEAN_FROM_MS, clean after. Every crossing record is listed in
        // `meta.rig.powerCrossings`; the player releases it mid-period (≥ 300
        // ms from both agents' predicted ticks, play.rs).
        let volts = |t: u64, k: u64| match kind {
            "exec-rail" if (RAIL_LOW_FROM_MS..RAIL_CLEAN_FROM_MS).contains(&t) => 5.10,
            "exec-rail" => 5.30,
            _ => 5.10 + (k % 4) as f64 * 0.01,
        };
        let mut t = 60;
        let mut k = 0u64;
        let (mut last_thr, mut last_v) = (None, None);
        while t < dur_ms {
            // A record whose value differs from the previous one of a
            // threshold-crossing kind = a crossing (exec-rail: the rail,
            // exec-thermal: the throttled bits).
            if kind != "exec-quiet" {
                let (th, v) = (throttled(t), format!("{:.2}", volts(t, k)));
                if kind == "exec-thermal" && last_thr.is_some_and(|l| l != th) {
                    crossings.push(t);
                }
                if kind == "exec-rail" && last_v.as_ref().is_some_and(|l: &String| *l != v) {
                    crossings.push(t + 10);
                }
                (last_thr, last_v) = (Some(th), Some(v));
            }
            b.push(
                t,
                TapeEvent::Exec {
                    argv: thr.clone(),
                    stdout: throttled(t).into(),
                    code: Some(0),
                    stderr: None,
                },
            );
            b.push(
                t + 10,
                TapeEvent::Exec {
                    argv: pmic.clone(),
                    stdout: PMIC_TEMPLATE.replace("@EXT5V@", &format!("{:.8}", volts(t, k))),
                    code: Some(0),
                    stderr: None,
                },
            );
            t += 2_000;
            k += 1;
        }
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

    // ---- Phase 4 (synth_p4.rs)
    let p4m = if has("p4") {
        Some(p4tracks::add(name, &mut b, dur_ms, fixtures)?)
    } else {
        None
    };

    if let Some(base) = base {
        for r in &base.records {
            if r.t <= dur_ms && BASE_KINDS.contains(&r.ev.kind()) {
                b.recs.push(r.clone());
            }
        }
    }
    b.recs.sort_by_key(|r| r.t);
    let mut meta = TapeMeta::new(SYNTH_DEVICE_ID, "synth", w0());
    if has("media-hw") {
        meta.hardware = media_hardware_block();
        meta.kvs = KvsInfo {
            channels: media_channels(),
            upload: true,
        };
    } else {
        meta.kvs = KvsInfo {
            channels: vec![],
            upload: false,
        };
    }
    if !crossings.is_empty() {
        let r = rig.get_or_insert_with(|| json!({}));
        r["powerCrossings"] = json!(crossings);
    }
    if let Some(p) = p4m {
        if !p.rig.is_empty() {
            let r = rig.get_or_insert_with(|| json!({}));
            for (k, v) in p.rig {
                r[k] = v;
            }
        }
        if let Some(h) = p.home {
            home.get_or_insert_with(serde_json::Map::new).extend(h);
        }
        if let Some(hw) = p.hardware {
            meta.hardware = hw;
        }
        if p.no_channels {
            meta.kvs = KvsInfo {
                channels: vec![],
                upload: false,
            };
        }
    }
    meta.rig = rig;
    meta.home = home;
    meta.source = Some(match base {
        Some(b) => format!(
            "{}+synth:{name}",
            b.meta.source.as_deref().unwrap_or("bench")
        ),
        None => format!("synth:{name}"),
    });
    meta.synthetic = tracks
        .iter()
        .filter(|t| base.is_none() || !base_owned.contains(t))
        .map(|t| t.to_string())
        .collect();
    let mode = if has("sim-mode") { "sim" } else { "real" };
    meta.env = TapeMeta::redacted_env(&|k| match k {
        "MODE" => Some(mode.into()),
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
    fn phase3_scenarios_carry_the_media_inputs() {
        let p3: Vec<&str> = SCENARIOS
            .iter()
            .map(|s| s.0)
            .filter(|n| is_phase3(n))
            .collect();
        // 8 Phase 3 scenarios + the 5 Phase 6 P3a takeover tapes.
        assert_eq!(p3.len(), 13, "{p3:?}");
        for name in p3 {
            let tape = build(name, None, &fixtures()).unwrap();
            assert_eq!(tape.meta.hardware, media_hardware_block(), "{name}");
            assert!(
                tape.meta.kvs.upload && tape.meta.kvs.channels.len() == 2,
                "{name}"
            );
            let rpicam = tape.records.iter().find(
                |r| matches!(&r.ev, TapeEvent::Exec { argv, .. } if argv[0] == "rpicam-hello"),
            );
            assert!(
                rpicam.is_some_and(|r| r.t < 1000),
                "{name}: prefilled camera list"
            );
            let cfg = mqtt_ins(&tape, "/config");
            let first: Value = serde_json::from_str(&cfg[0].1).unwrap();
            assert_eq!(cfg[0].0, 400, "{name}");
            assert!(cfg[0].2, "{name}: retained");
            assert_eq!(first["recording"], rec_namespace(name), "{name}");
            assert_eq!(first["recording"]["video"]["segmentSeconds"], 10);
        }
        let preset = build("rec-preset", None, &fixtures()).unwrap();
        let ext: Vec<(u64, &str, &str)> = preset
            .records
            .iter()
            .filter_map(|r| match &r.ev {
                TapeEvent::ChildExit(x) if x.external => {
                    Some((r.t, x.role.as_str(), x.key.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(ext, [(35_000, "capture", "cam0"), (45_000, "kvs", "cam1")]);
        let disk = build("rec-disk", None, &fixtures()).unwrap();
        assert_eq!(disk.meta.rig.as_ref().unwrap()["filler"]["mib"], 600);
        // review-wp5 R-WP5-3: rec-disk's NMEA has a no-fix lead-in (bursts at
        // 0.9..3.9 s carry no GGA fix), every other recording tape fixes at once.
        let ggas = |t: &Tape| -> Vec<(u64, bool)> {
            t.records
                .iter()
                .filter_map(|r| match &r.ev {
                    TapeEvent::Nmea { line } if line.starts_with("$GPGGA") => {
                        Some((r.t, line.split(',').nth(6) != Some("0")))
                    }
                    _ => None,
                })
                .collect()
        };
        let g = ggas(&disk);
        assert!(
            g.iter().filter(|(t, _)| *t < NMEA_PREFIX_MS).count() >= 3,
            "{g:?}"
        );
        assert!(
            g.iter().all(|(t, fix)| *fix == (*t >= NMEA_PREFIX_MS)),
            "{g:?}"
        );
        let other = ggas(&build("rec-shed-uv", None, &fixtures()).unwrap());
        assert!(
            other.iter().all(|(_, fix)| *fix),
            "fixonly tapes fix from the first burst"
        );
        let seeded: Value = serde_json::from_str(
            disk.meta.home.as_ref().unwrap()["settings.json"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(seeded["recording"]["upload"]["mode"], "off");
        assert_eq!(seeded["recording"]["retention"]["maxBytes"], 536_870_912u64);
        let outage = build("rec-upload-outage", None, &fixtures()).unwrap();
        assert_eq!(crate::aws::faults_of(outage.meta.rig.as_ref()).len(), 1);
        // Power-threshold crossings (review-wp6 re-check): listed for the
        // player, none in the first 3 s, inputs stable ≥ 2.5 s around each.
        let crossings = |name: &str| {
            let t = build(name, None, &fixtures()).unwrap();
            t.meta
                .rig
                .as_ref()
                .and_then(|r| r.get("powerCrossings"))
                .cloned()
        };
        assert_eq!(crossings("rec-shed-uv"), Some(json!([10_070, 110_070])));
        assert_eq!(
            crossings("rec-shed-thermal"),
            Some(json!([20_060, 110_060]))
        );
        assert_eq!(crossings("media"), None);
        let uv = build("rec-shed-uv", None, &fixtures()).unwrap();
        let rails: Vec<(u64, String)> = uv
            .records
            .iter()
            .filter_map(|r| match &r.ev {
                TapeEvent::Exec { argv, stdout, .. } if argv[1] == "pmic_read_adc" => {
                    Some((r.t, stdout.clone()))
                }
                _ => None,
            })
            .collect();
        for w in rails.windows(2) {
            if w[0].1 != w[1].1 {
                assert!(w[1].0 >= 3_000, "no crossing at boot");
                let around = rails
                    .iter()
                    .filter(|(t, _)| t.abs_diff(w[1].0) <= 2_500 && *t != w[1].0);
                for (t, v) in around {
                    let want = if *t < w[1].0 { &w[0].1 } else { &w[1].1 };
                    assert_eq!(v, want, "stable ±2.5 s around the crossing at {}", w[1].0);
                }
            }
        }
        assert!(outage.records.iter().any(|r| r.t == 50_000
            && matches!(
                r.ev,
                TapeEvent::Conn {
                    state: ConnState::Closed
                }
            )));
        let gate = build("rec-boot-defer", None, &fixtures()).unwrap();
        let first_fix = gate.records.iter().find(|r| {
            matches!(&r.ev, TapeEvent::Nmea { line } if line.starts_with("$GPGGA") && line.contains(",1,08,"))
        });
        assert!(first_fix.is_some_and(|r| r.t >= REC_GATE_RELEASE_MS));
        // Phase 2 scenarios are unchanged: no media inputs.
        let smoke = build("smoke", None, &fixtures()).unwrap();
        assert!(
            !smoke.meta.kvs.upload
                && smoke.meta.rig.is_none()
                && smoke.meta.kvs.channels.is_empty()
        );
    }

    /// `(t, payload)` of every config / command `mqtt_in` record.
    fn mqtt_ins(tape: &Tape, suffix: &str) -> Vec<(u64, String, bool)> {
        tape.records
            .iter()
            .filter_map(|r| match &r.ev {
                TapeEvent::MqttIn { topic, retain, .. } if topic.ends_with(suffix) => Some((
                    r.t,
                    String::from_utf8_lossy(&r.ev.mqtt_payload().unwrap_or_default()).to_string(),
                    *retain,
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn config_envelope_track_drives_every_order_path() {
        for name in ["calibration", "full", "config-envelope"] {
            let tape = build(name, None, &fixtures()).unwrap();
            let cfg = mqtt_ins(&tape, "/config");
            let json = |p: &str| serde_json::from_str::<Value>(p).ok();
            let revs: Vec<Option<i64>> = cfg
                .iter()
                .map(|(_, p, _)| json(p).and_then(|v| v["rev"].as_i64()))
                .collect();
            // valid hardware + uploadBucket, retained
            assert!(
                cfg.iter().any(|(_, p, r)| *r
                    && json(p).is_some_and(|v| v["hardware"] == synth_hardware_block()
                        && v["uploadBucket"] == SYNTH_UPLOAD_BUCKET)),
                "{name}"
            );
            // invalid hardware (unknown encoder)
            assert!(cfg.iter().any(|(_, p, _)| json(p)
                .is_some_and(|v| v["hardware"]["encoder"] == "hw-h265")), "{name}");
            // malformed: not JSON, and v != 1
            assert!(cfg.iter().any(|(_, p, _)| json(p).is_none()), "{name}");
            assert!(
                cfg.iter()
                    .any(|(_, p, _)| json(p).is_some_and(|v| v["v"] == 2)),
                "{name}"
            );
            // a stale rev after a newer one
            let max_before_last_2 = revs
                .iter()
                .rposition(|r| *r == Some(2))
                .map(|i| revs[..i].iter().flatten().max().copied().unwrap_or(0));
            assert!(max_before_last_2.is_some_and(|m| m > 2), "{name}: {revs:?}");
            // every new-track record lands after the older config track's last one
            if name != "config-envelope" {
                let old = [config_doc(1, 10, false), config_doc(2, 12, true)];
                let last_old = cfg.iter().filter(|c| old.contains(&c.1)).map(|c| c.0).max();
                let first_new = cfg
                    .iter()
                    .filter(|c| !old.contains(&c.1))
                    .map(|c| c.0)
                    .min();
                assert!(first_new > last_old, "{name}");
            }
        }
        // standalone: rev 3 published during the cut (retained delivery on reconnect)
        let tape = build("config-envelope", None, &fixtures()).unwrap();
        let closed = tape
            .records
            .iter()
            .find(|r| {
                matches!(
                    r.ev,
                    TapeEvent::Conn {
                        state: ConnState::Closed
                    }
                )
            })
            .unwrap()
            .t;
        let cfg = mqtt_ins(&tape, "/config");
        let first = &cfg[0];
        assert!(first.0 > closed && first.1.contains("\"rev\":3"));
        // rev 4 only after the worst-case reconnect, and the whole track fits.
        let restored = tape
            .records
            .iter()
            .filter(|r| {
                matches!(
                    r.ev,
                    TapeEvent::Conn {
                        state: ConnState::Connected
                    }
                )
            })
            .map(|r| r.t)
            .max()
            .unwrap();
        let rev4 = cfg.iter().find(|c| c.1.contains("\"rev\":4")).unwrap();
        assert!(
            rev4.0 >= restored + RECONNECT_WORST_MS,
            "{} vs {restored}",
            rev4.0
        );
        assert_eq!(cfg.len(), 8, "every envelope record inside the tape");
        // NMEA flows the whole run (no silent-tty reopen: the TS wedge).
        let nmea: Vec<u64> = tape
            .records
            .iter()
            .filter(|r| matches!(r.ev, TapeEvent::Nmea { .. }))
            .map(|r| r.t)
            .collect();
        assert!(nmea.first().is_some_and(|t| *t < 2_000));
        assert!(nmea.windows(2).all(|w| w[1] - w[0] < 2_000));
        assert!(nmea.last().is_some_and(|t| *t + 2_000 > 240_000));
    }

    #[test]
    fn lte_apn_track_changes_the_cellular_block_in_a_report_quiet_stretch() {
        for name in ["calibration", "full"] {
            let tape = build(name, None, &fixtures()).unwrap();
            assert!(tape.meta.synthetic.contains(&"lte-apn".to_string()));
            let apn = |r: &TapeRecord| match &r.ev {
                TapeEvent::LteFile { text: Some(t), .. } => {
                    let v: Value = serde_json::from_str(t).unwrap();
                    v["cellular"]["apn_applied"].as_str().map(str::to_string)
                }
                _ => None,
            };
            let lte: Vec<(u64, String)> = tape
                .records
                .iter()
                .filter_map(|r| Some((r.t, apn(r)?)))
                .take(7)
                .collect();
            let alt: Vec<u64> = lte
                .iter()
                .filter(|(_, a)| a == LTE_APN_ALT)
                .map(|(t, _)| *t)
                .collect();
            assert_eq!(alt, vec![10_080, 20_080, 30_080, 40_080], "{name}");
            assert_eq!(lte[5], (50_080, "synth.apn".to_string()), "reverted");
            // Held > 35 s, and no other report trigger (config/command
            // delivery, CLI edit, conn change) from 5 s before the change
            // until the watch tick that sees the revert (≤ 60 s + 2 s
            // debounce) + margin.
            assert!(lte[5].0 - alt[0] > 35_000, "held > 35 s");
            let triggers: Vec<(u64, &str)> = tape
                .records
                .iter()
                .filter(|r| matches!(r.ev.kind(), "mqtt_in" | "cli" | "conn"))
                .filter(|r| r.t > 5_000 && r.t < 70_000)
                .map(|r| (r.t, r.ev.kind()))
                .collect();
            assert!(triggers.is_empty(), "{name}: {triggers:?}");
        }
        // Not in the other scenarios (smoke stays as committed).
        let smoke = build("smoke", None, &fixtures()).unwrap();
        assert!(!smoke.meta.synthetic.contains(&"lte-apn".to_string()));
    }

    #[test]
    fn sms_tracks_send_read_sms_and_sim_mode_sets_the_env() {
        for name in ["calibration", "full", "sms-sim"] {
            let tape = build(name, None, &fixtures()).unwrap();
            let cmds = mqtt_ins(&tape, "/command");
            let sms: Vec<&(u64, String, bool)> =
                cmds.iter().filter(|c| c.1.contains("read_sms")).collect();
            assert_eq!(sms.len(), 2, "{name}");
            assert!(sms[0].1.contains(r#""args":{"limit":5}"#), "{name}");
            let mode = tape.meta.env.get("MODE").and_then(Value::as_str);
            assert_eq!(mode, Some(if name == "sms-sim" { "sim" } else { "real" }));
        }
    }

    #[test]
    fn takeover_tapes_carry_their_handoff_state_and_outage() {
        for (name, at, outage, video, telemetry) in [
            ("p6-takeover-open", TAKEOVER_OPEN_AT_MS, None, true, true),
            (
                "p6-takeover-vbacklog",
                TAKEOVER_VBACKLOG_AT_MS,
                Some(30_000),
                true,
                false,
            ),
            (
                "p6-takeover-tbacklog",
                TAKEOVER_TBACKLOG_AT_MS,
                Some(20_000),
                false,
                true,
            ),
            (
                "p6-takeover-reap",
                TAKEOVER_REAP_AT_MS,
                Some(30_000),
                true,
                true,
            ),
            (
                "p6-takeover-inflight",
                TAKEOVER_INFLIGHT_AT_MS,
                None,
                true,
                true,
            ),
        ] {
            let tape = build(name, None, &fixtures()).unwrap();
            let rig = tape.meta.rig.as_ref().unwrap();
            assert_eq!(rig["handoff"]["atMs"], at, "{name}");
            assert!(rig["handoff"]["expect"]["min"].is_object(), "{name}");
            assert!(
                at < tape.duration_ms(),
                "{name}: the handoff is inside the tape"
            );
            let faults = crate::aws::faults_of(Some(rig));
            match outage {
                None => assert!(faults.is_empty(), "{name}"),
                Some(from) => {
                    assert_eq!(faults.len(), 1, "{name}");
                    assert_eq!(faults[0].from_ms, from, "{name}");
                    // The outage spans the handoff and ends 10 s (tape) after it.
                    assert_eq!(
                        faults[0].to_ms,
                        (at + TAKEOVER_OUTAGE_TAIL_MS) as i64,
                        "{name}"
                    );
                    assert_eq!(faults[0].status, 503);
                }
            }
            let rec = rec_namespace(name);
            assert_eq!(rec["video"]["enabled"], video, "{name}");
            assert_eq!(rec["telemetry"]["enabled"], telemetry, "{name}");
            assert_eq!(rec["upload"]["mode"], "auto", "{name}");
            // Off the 60 s telemetry rotation by ≥ 10 s (recorder start ≈ 1-2 s).
            let ph = at % 60_000;
            assert!((10_000..=50_000).contains(&ph), "{name}: {ph}");
            // Determinism.
            let (mut x, mut y) = (Vec::new(), Vec::new());
            tape.write_to(&mut x).unwrap();
            build(name, None, &fixtures())
                .unwrap()
                .write_to(&mut y)
                .unwrap();
            assert_eq!(x, y, "{name}");
        }
        // The in-flight tape holds mp4 PUTs only, in the 8 s before the
        // handoff, for longer than the stop takes.
        let inf = build("p6-takeover-inflight", None, &fixtures()).unwrap();
        let holds = crate::aws::holds_of(inf.meta.rig.as_ref());
        assert_eq!(holds.len(), 1);
        assert_eq!(holds[0].suffix, ".mp4");
        assert_eq!(holds[0].to_ms, TAKEOVER_INFLIGHT_AT_MS as i64);
        assert_eq!(
            holds[0].from_ms,
            (TAKEOVER_INFLIGHT_AT_MS - TAKEOVER_INFLIGHT_LEAD_MS) as i64
        );
        assert!(holds[0].hold_ms > TAKEOVER_INFLIGHT_LEAD_MS + 5_000);
        assert_eq!(
            inf.meta.rig.as_ref().unwrap()["handoff"]["expect"]["inflight"],
            1
        );
        // The reap tape's tail outlives REAP_MIN_AGE_MS (video-source.ts:35).
        let reap = build("p6-takeover-reap", None, &fixtures()).unwrap();
        assert!(reap.duration_ms() >= 600_000 + 60_000);
        assert_eq!(
            reap.meta.rig.as_ref().unwrap()["handoff"]["expect"]["reaped"],
            true
        );
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
        let lte: Vec<_> = t
            .records
            .iter()
            .filter(|r| r.ev.kind() == "lte_file")
            .collect();
        let base_lte: Vec<_> = base
            .records
            .iter()
            .filter(|r| r.ev.kind() == "lte_file")
            .collect();
        assert_eq!(lte, base_lte);
        assert!(t.records.iter().any(|r| r.ev.kind() == "nmea"));
        // Synthetic records are stamped on the base's wall clock.
        assert!(t
            .records
            .iter()
            .all(|r| r.w >= 1_791_000_000_000 || BASE_KINDS.contains(&r.ev.kind())));
        // And the thread-local is restored.
        assert_eq!(
            build("smoke", Some(5), &fixtures())
                .unwrap()
                .meta
                .start_wall,
            SYNTH_START_WALL
        );
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
