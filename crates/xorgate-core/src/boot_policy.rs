//! Boot power policy + runtime load shedding — 1:1 port of
//! `src/recording/boot-policy.ts` (943 lines) minus its I/O.
//!
//! Two independent evaluators answer "may heavy encoders start / keep
//! running?" (agent-anatomy §4, invariant 7):
//!  - [`BootDeferPolicy`] — one-shot boot staging (stage 1 modem-settle defer,
//!    stage 2 GPS/LTE video gate); LATCHES its first allow for the process
//!    lifetime. The pure decision is [`decide_boot_defer`].
//!  - [`LoadShedPolicy`] — reversible runtime undervoltage / thermal shedding
//!    fed ~1 Hz power samples: rung 1 parks live, rung 2 (power cause only —
//!    thermal caps at rung 1) stops video recording; telemetry recording is
//!    never shed. Pure core: [`compute_shed_signals`] + [`decide_shed_transition`].
//!
//! TIME (invariant 2): every interval is measured on the MONOTONIC clock. The
//! wall clock is used for exactly one thing — comparing telemetry.json's own
//! wall-clock stamp — and even there staleness additionally requires that the
//! file stopped changing for the stale window (monotonic).
//!
//! Left OUT (I/O, Phase 2 HAL): `readBudgetMa` (`boot-policy.ts:377-385`,
//! reads `/proc/device-tree/chosen/power/max_current`), `readTelemetry`
//! (`:387-395`, reads `LTE_TELEMETRY_FILE`), the `Date.now`/`monotonicMs`
//! defaults (`:60, :324-325`). The readers are injected through
//! [`BootDeferSources`]; their pure decode halves are
//! [`parse_budget_ma`] and [`parse_policy_telemetry`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::clock::{Clock, MonoMs};
use crate::js::{date_parse, number_to_string, to_fixed};
use crate::round::js_math_round;
pub use crate::settings::BootDeferMode;
use crate::types::{GnssView, PowerSampleReading, ShedConfig, SkyState};

// ---------------------------------------------------------------------------
// Boot defer (Phase 2 stage 1 + Phase 7 stage 2) — `boot-policy.ts:63-396`
// ---------------------------------------------------------------------------

/// `BootDeferConfig` (`boot-policy.ts:68-101`): resolved from settings per
/// evaluation (`recording/manager.ts:497-504`). Integer knobs, as the
/// settings clamps produce them (WP1 `recording_config::VideoConfig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootDeferConfig {
    pub mode: BootDeferMode,
    /// Hard cap on the stage-1 defer, seconds; 0 = stage 1 off.
    pub max_secs: u32,
    /// `auto` arms only below this negotiated budget (mA); shared by both stages.
    pub min_budget_ma: u32,
    /// Stage 2 arming (`recording.video.gpsGate`).
    pub gate_mode: BootDeferMode,
    /// Stage-2 give-up cap, seconds; 0 = no time cap.
    pub gate_max_secs: u32,
    /// Stage-2 no-progress escape hatch, seconds; 0 = off.
    pub gate_no_progress_secs: u32,
}

/// `BootDeferState` (`boot-policy.ts:103`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootDeferState {
    Off,
    Skipped,
    Deferring,
    Gating,
    Released,
}

impl BootDeferState {
    pub fn as_str(self) -> &'static str {
        match self {
            BootDeferState::Off => "off",
            BootDeferState::Skipped => "skipped",
            BootDeferState::Deferring => "deferring",
            BootDeferState::Gating => "gating",
            BootDeferState::Released => "released",
        }
    }
}

/// `BootDeferVerdict` (`boot-policy.ts:105-123`). Key order = the TS object
/// literals: allow, state, why, holdLive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootDeferVerdict {
    /// May heavy encoders start yet?
    pub allow: bool,
    pub state: BootDeferState,
    /// Stable detail: release reason, skip/arm rationale, or "disabled".
    pub why: String,
    /// Hold live streaming too? True only while STAGE 2 is deferring.
    pub hold_live: bool,
}

impl BootDeferVerdict {
    fn new(allow: bool, state: BootDeferState, why: impl Into<String>, hold_live: bool) -> Self {
        Self {
            allow,
            state,
            why: why.into(),
            hold_live,
        }
    }
    fn released(why: &str) -> Self {
        Self::new(true, BootDeferState::Released, why, false)
    }
}

/// `TELEMETRY_STALE_MS` (`boot-policy.ts:126`): telemetry.json older (or
/// absent longer) than this = daemon dead → fail open.
pub const TELEMETRY_STALE_MS: f64 = 60_000.0;

/// `PolicyTelemetry` (`boot-policy.ts:132-136`): the subset of the daemon's
/// telemetry.json this policy reads. Every field is `unknown` in the TS, so
/// each is kept as the raw JSON value; JSON `null` and absence behave the
/// same in every comparison the decider makes.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PolicyTelemetry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reg: Option<Value>,
}

impl PolicyTelemetry {
    /// `parsed as PolicyTelemetry` (`boot-policy.ts:391-392`): any JSON value
    /// with `typeof === "object"` and truthy — an object or an ARRAY (whose
    /// named fields then read as `undefined`).
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Object(m) => Some(Self {
                timestamp: m.get("timestamp").cloned(),
                status: m.get("status").cloned(),
                reg: m.get("reg").cloned(),
            }),
            Value::Array(_) => Some(Self::default()),
            _ => None,
        }
    }

    fn status_str(&self) -> Option<&str> {
        self.status.as_ref().and_then(Value::as_str)
    }

    /// `t.reg === 1 || t.reg === 5` (`boot-policy.ts:266`): a strict number compare.
    fn reg_is(&self, n: f64) -> bool {
        self.reg.as_ref().and_then(Value::as_f64) == Some(n)
    }
}

/// Pure half of `readTelemetry` (`boot-policy.ts:387-395`): `JSON.parse`,
/// then `!parsed || typeof parsed !== "object"` → `None`. A read error is the
/// caller's `None`.
pub fn parse_policy_telemetry(text: &str) -> Option<PolicyTelemetry> {
    let v: Value = serde_json::from_str(text).ok()?;
    PolicyTelemetry::from_value(&v)
}

/// Pure half of `readBudgetMa` (`boot-policy.ts:377-385`): fewer than 4 bytes
/// → `None`; else the big-endian u32 at offset 0 (device-tree u32s are BE).
pub fn parse_budget_ma(buf: &[u8]) -> Option<u32> {
    if buf.len() < 4 {
        return None;
    }
    Some(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

/// `BootDeferInputs` (`boot-policy.ts:138-161`). Times are JS numbers (ms);
/// `f64` so the arithmetic against `Date.parse` (which may be `NaN`) is the
/// TS arithmetic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootDeferInputs {
    /// Negotiated supply budget in mA; `None` = unreadable → keep the defer.
    pub budget_ma: Option<u32>,
    /// Parsed telemetry.json; `None` = file missing or unparseable.
    pub telemetry: Option<PolicyTelemetry>,
    /// Wall clock (telemetry.json's own stamp only).
    pub now_ms: f64,
    /// MONOTONIC time since the policy started watching.
    pub elapsed_ms: f64,
    /// MONOTONIC time since telemetry.json's `timestamp` last CHANGED.
    pub telemetry_unchanged_ms: Option<f64>,
    pub stale_ms: f64,
    /// GNSS view for stage 2; `None` = no GNSS collector → the gate cannot arm.
    pub gnss: Option<GnssView>,
}

/// `decideBootDefer` (`boot-policy.ts:179-280`): one tick of the pure
/// decision; latching is the caller's job. Stage 2 (the gate), when armed,
/// SUPERSEDES stage 1 outright.
pub fn decide_boot_defer(cfg: &BootDeferConfig, input: &BootDeferInputs) -> BootDeferVerdict {
    let budget_text = match input.budget_ma {
        None => "psu budget unknown".to_string(),
        Some(b) => format!("psu budget {b}mA"),
    };
    let high_budget = input.budget_ma.is_some_and(|b| b >= cfg.min_budget_ma);

    let settle_off = cfg.mode == BootDeferMode::Off || cfg.max_secs == 0;
    let settle_armed = !settle_off && !(cfg.mode == BootDeferMode::Auto && high_budget);
    let gate_off = cfg.gate_mode == BootDeferMode::Off || input.gnss.is_none();
    let gate_armed = !gate_off && !(cfg.gate_mode == BootDeferMode::Auto && high_budget);

    if !settle_armed && !gate_armed {
        return if settle_off && gate_off {
            BootDeferVerdict::new(true, BootDeferState::Off, "disabled", false)
        } else {
            BootDeferVerdict::new(
                true,
                BootDeferState::Skipped,
                format!("{budget_text} >= {}", cfg.min_budget_ma),
                false,
            )
        };
    }

    let t = input.telemetry.as_ref();
    // "Stale" needs BOTH arms (`boot-policy.ts:200-218`): the stamp reads old
    // in wall-clock terms AND the file has not been rewritten for the stale
    // window (monotonic). With no file at all, the same grace on elapsed time.
    let stale = match t {
        Some(t) => {
            let ts = match t.timestamp.as_ref().and_then(Value::as_str) {
                Some(s) => date_parse(s),
                None => f64::NAN,
            };
            !ts.is_finite()
                || (input.now_ms - ts > input.stale_ms
                    && input.telemetry_unchanged_ms.unwrap_or(f64::INFINITY) > input.stale_ms)
        }
        None => input.elapsed_ms > input.stale_ms,
    };

    let arm_text = |mode: BootDeferMode| {
        if mode == BootDeferMode::Always {
            "mode=always".to_string()
        } else {
            format!("{budget_text} < {}", cfg.min_budget_ma)
        }
    };
    let arm_text = if gate_armed {
        arm_text(cfg.gate_mode)
    } else {
        arm_text(cfg.mode)
    };

    let status = t.and_then(PolicyTelemetry::status_str);
    let fix = input.gnss.is_some_and(|g| g.fix);

    if gate_armed {
        // Release on the FIRST of (Q8, in order) — `boot-policy.ts:225-256`.
        if status == Some("dead_zone") {
            return BootDeferVerdict::released("dead_zone");
        }
        if status == Some("connected") && fix {
            return BootDeferVerdict::released("gps-fix");
        }
        if stale {
            return BootDeferVerdict::released("stale");
        }
        if input.gnss.is_some_and(|g| g.sky == SkyState::NoRf) {
            return BootDeferVerdict::released("no-rf");
        }
        // No-progress escape hatch: nothing on EITHER side has moved.
        if cfg.gate_no_progress_secs > 0
            && input.elapsed_ms >= f64::from(cfg.gate_no_progress_secs) * 1000.0
            && !fix
            && status != Some("connected")
        {
            return BootDeferVerdict::released("no-progress");
        }
        if cfg.gate_max_secs > 0 && input.elapsed_ms >= f64::from(cfg.gate_max_secs) * 1000.0 {
            return BootDeferVerdict::released("gate-timeout");
        }
        return BootDeferVerdict::new(false, BootDeferState::Gating, arm_text, true);
    }

    match t {
        Some(t) => {
            // A terminal status wins over freshness either way.
            if let Some(s @ ("connected" | "dead_zone")) = status {
                return BootDeferVerdict::released(s);
            }
            if stale {
                return BootDeferVerdict::released("stale");
            }
            if status == Some("connecting") && (t.reg_is(1.0) || t.reg_is(5.0)) {
                return BootDeferVerdict::released("reg");
            }
        }
        None => {
            if stale {
                return BootDeferVerdict::released("stale");
            }
        }
    }

    if input.elapsed_ms >= f64::from(cfg.max_secs) * 1000.0 {
        return BootDeferVerdict::released("timeout");
    }

    BootDeferVerdict::new(false, BootDeferState::Deferring, arm_text, false)
}

/// The injected readers of `BootDeferPolicy` (`boot-policy.ts:282-296`
/// options + the private `readBudgetMa`/`readTelemetry`). The HAL implements
/// them over `/proc/device-tree/chosen/power/max_current` and
/// `LTE_TELEMETRY_FILE`, using [`parse_budget_ma`] / [`parse_policy_telemetry`].
pub trait BootDeferSources {
    /// Negotiated PSU budget; `None` = unreadable. Called once per policy.
    fn read_budget_ma(&mut self) -> Option<u32>;
    /// telemetry.json; `None` = missing or unparseable. Called every evaluation.
    fn read_telemetry(&mut self) -> Option<PolicyTelemetry>;
    /// GNSS view for stage 2; hosts without a GPS collector return `None`
    /// (the TS default `() => null`).
    fn gnss(&mut self) -> Option<GnssView>;
}

/// `BootDeferPolicy` (`boot-policy.ts:304-396`): reads the budget once,
/// re-reads telemetry.json on every evaluation (the manager's 5 s settings
/// poll is the tick), and latches the first allow verdict for the life of
/// the process.
#[derive(Debug, Clone, PartialEq)]
pub struct BootDeferPolicy {
    stale_ms: f64,
    start_mono: MonoMs,
    /// `None` = not read yet; `Some(None)` = unreadable.
    budget_ma: Option<Option<u32>>,
    /// `None` = undefined (no file seen); `Some(None)` = file with a
    /// non-string stamp.
    last_stamp: Option<Option<String>>,
    last_stamp_mono: MonoMs,
    latched: Option<BootDeferVerdict>,
    last: Option<BootDeferVerdict>,
}

impl BootDeferPolicy {
    /// `new BootDeferPolicy()` (`boot-policy.ts:320-328`); `stale_ms` = the
    /// `staleMs` option, `None` = [`TELEMETRY_STALE_MS`].
    pub fn new(clock: &dyn Clock, stale_ms: Option<f64>) -> Self {
        Self {
            stale_ms: stale_ms.unwrap_or(TELEMETRY_STALE_MS),
            start_mono: clock.mono_ms(),
            budget_ma: None,
            last_stamp: None,
            last_stamp_mono: 0,
            latched: None,
            last: None,
        }
    }

    /// `evaluate` (`boot-policy.ts:334-351`). Same read/clock order as the TS:
    /// budget (first call only), mono, telemetry, wall, gnss.
    pub fn evaluate(
        &mut self,
        cfg: &BootDeferConfig,
        clock: &dyn Clock,
        src: &mut dyn BootDeferSources,
    ) -> BootDeferVerdict {
        if let Some(v) = &self.latched {
            return v.clone();
        }
        if self.budget_ma.is_none() {
            self.budget_ma = Some(src.read_budget_ma());
        }
        let mono = clock.mono_ms();
        let telemetry = src.read_telemetry();
        let now_ms = clock.wall_ms() as f64;
        let telemetry_unchanged_ms = self.note_stamp(telemetry.as_ref(), mono);
        let verdict = decide_boot_defer(
            cfg,
            &BootDeferInputs {
                budget_ma: self.budget_ma.flatten(),
                telemetry,
                now_ms,
                elapsed_ms: mono as f64 - self.start_mono as f64,
                telemetry_unchanged_ms,
                stale_ms: self.stale_ms,
                gnss: src.gnss(),
            },
        );
        self.last = Some(verdict.clone());
        if verdict.allow {
            self.latched = Some(verdict.clone());
        }
        verdict
    }

    /// `noteStamp` (`boot-policy.ts:358-370`): how long (monotonically)
    /// telemetry.json has carried the SAME stamp. A file only just seen
    /// counts as fresh.
    fn note_stamp(&mut self, t: Option<&PolicyTelemetry>, mono: MonoMs) -> Option<f64> {
        let t = match t {
            None => {
                self.last_stamp = None;
                return None;
            }
            Some(t) => t,
        };
        let stamp = t
            .timestamp
            .as_ref()
            .and_then(Value::as_str)
            .map(str::to_string);
        if self.last_stamp.as_ref() != Some(&stamp) {
            self.last_stamp = Some(stamp);
            self.last_stamp_mono = mono;
        }
        Some(mono as f64 - self.last_stamp_mono as f64)
    }

    /// `statusString` (`boot-policy.ts:372-375`): `"released:<why>"` or the
    /// state; `None` before the first evaluation.
    pub fn status_string(&self) -> Option<String> {
        let last = self.last.as_ref()?;
        Some(if last.state == BootDeferState::Released {
            format!("released:{}", last.why)
        } else {
            last.state.as_str().to_string()
        })
    }
}

// ---------------------------------------------------------------------------
// Runtime load shedding (Phase 3 + thermal arm) — `boot-policy.ts:398-943`
// ---------------------------------------------------------------------------

/// `ShedLevel` (`boot-policy.ts:411`): 0 = everything on, 1 = live KVS off,
/// 2 = video recording off too. Serialises as the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ShedLevel {
    Full = 0,
    NoLive = 1,
    BridgeOnly = 2,
}

impl ShedLevel {
    pub fn index(self) -> usize {
        self as usize
    }
    /// `SHED_LEVELS[level]`.
    pub fn name(self) -> &'static str {
        SHED_LEVELS[self.index()]
    }
    fn from_index(i: u64) -> Option<Self> {
        match i {
            0 => Some(ShedLevel::Full),
            1 => Some(ShedLevel::NoLive),
            2 => Some(ShedLevel::BridgeOnly),
            _ => None,
        }
    }
}

impl Serialize for ShedLevel {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for ShedLevel {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = u64::deserialize(d)?;
        ShedLevel::from_index(n).ok_or_else(|| serde::de::Error::custom("shed level 0|1|2"))
    }
}

/// `SHED_LEVELS` (`boot-policy.ts:414`).
pub const SHED_LEVELS: [&str; 3] = ["full", "no-live", "bridge-only"];

/// `POWER_SAMPLE_STALE_MS` (`boot-policy.ts:500`): rail/thermal-level
/// freshness guard.
pub const POWER_SAMPLE_STALE_MS: i64 = 5_000;

/// `PowerSample` (`boot-policy.ts:487-493`): a pushed reading normalised
/// once — `tempC ?? null`, `softTemp ?? false` — and stamped on the
/// MONOTONIC clock.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerSample {
    pub uv: bool,
    pub rail_v: Option<f64>,
    pub temp_c: Option<f64>,
    pub soft_temp: bool,
    pub t_ms: i64,
}

impl PowerSample {
    /// The normalisation in `LoadShedPolicy.push` (`boot-policy.ts:882-891`).
    pub fn from_reading(r: &PowerSampleReading, t_ms: i64) -> Self {
        Self {
            uv: r.uv,
            rail_v: r.rail_v,
            temp_c: r.temp_c,
            soft_temp: r.soft_temp.unwrap_or(false),
            t_ms,
        }
    }
}

/// `ShedCause` (`boot-policy.ts:511`): `power` = UV bit or rail arm,
/// `thermal` = sustained SoC heat or firmware bit 0x8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShedCause {
    Power,
    Thermal,
}

/// `ShedBlockReason` (`boot-policy.ts:520`): `ShedCause` under its PUBLISHED
/// name (`liveBlock.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShedBlockReason {
    Undervoltage,
    Thermal,
}

impl ShedBlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ShedBlockReason::Undervoltage => "undervoltage",
            ShedBlockReason::Thermal => "thermal",
        }
    }
}

/// `ShedSignals` (`boot-policy.ts:523-545`). Key order = the return literal
/// (`:627`): trigger, dirty, why, cause, lastDirtyMs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShedSignals {
    pub trigger: bool,
    /// Is the NEWEST sample itself unhealthy by the trigger thresholds?
    pub dirty: bool,
    /// Trigger rationale for the transition log; `None` when not triggering.
    pub why: Option<String>,
    pub cause: Option<ShedCause>,
    /// Newest sample dirty by the RESTORE thresholds; the restore anchor.
    pub last_dirty_ms: Option<i64>,
}

/// `computeShedSignals` (`boot-policy.ts:547-628`).
pub fn compute_shed_signals(samples: &[PowerSample], cfg: &ShedConfig, now_ms: i64) -> ShedSignals {
    let newest = samples.last();
    let fresh = newest.is_some_and(|n| now_ms - n.t_ms <= POWER_SAMPLE_STALE_MS);

    let uv_window_ms = i64::from(cfg.uv_window_secs) * 1000;
    let uv_count = samples
        .iter()
        .filter(|s| s.uv && now_ms - s.t_ms <= uv_window_ms)
        .count();

    // Sustained rail low: the low run since the last healthy reading.
    let mut low_run_start_ms: Option<i64> = None;
    let mut last_rail: Option<f64> = None;
    for s in samples.iter().rev() {
        let Some(r) = s.rail_v else { continue };
        if last_rail.is_none() {
            last_rail = Some(r);
        }
        if r >= cfg.rail_low_volts {
            break;
        }
        low_run_start_ms = Some(s.t_ms);
    }
    let rail_low = fresh
        && low_run_start_ms
            .is_some_and(|start| now_ms - start >= i64::from(cfg.rail_low_secs) * 1000);

    // Sustained SoC heat — the same run-walk, mirrored; null temps skipped.
    let mut high_run_start_ms: Option<i64> = None;
    let mut last_temp: Option<f64> = None;
    for s in samples.iter().rev() {
        let Some(t) = s.temp_c else { continue };
        if last_temp.is_none() {
            last_temp = Some(t);
        }
        if t < cfg.cpu_temp_c {
            break;
        }
        high_run_start_ms = Some(s.t_ms);
    }
    let temp_high = fresh
        && high_run_start_ms
            .is_some_and(|start| now_ms - start >= i64::from(cfg.cpu_temp_secs) * 1000);
    // The firmware's own thermal verdict (0x8), behind the freshness guard.
    let soft_temp = fresh && newest.is_some_and(|n| n.soft_temp);

    let mut last_dirty_ms: Option<i64> = None;
    for s in samples {
        if s.uv
            || s.rail_v.is_some_and(|r| r < cfg.rail_restore_volts)
            || s.temp_c.is_some_and(|t| t >= cfg.cpu_temp_restore_c)
            || s.soft_temp
        {
            last_dirty_ms = Some(s.t_ms);
        }
    }

    let dirty = fresh
        && newest.is_some_and(|n| {
            n.uv || n.rail_v.is_some_and(|r| r < cfg.rail_low_volts)
                || n.temp_c.is_some_and(|t| t >= cfg.cpu_temp_c)
                || n.soft_temp
        });

    // Precedence: undervoltage, then rail, then thermal (`boot-policy.ts:613-626`).
    // `lastRail?.toFixed(2)` / `lastTemp?.toFixed(1)` are always defined when
    // their arm fires (the run start implies a newest value).
    let uv_fires = uv_count >= cfg.uv_samples as usize;
    let why = if uv_fires {
        Some(format!(
            "undervoltage {uv_count}x in {}s",
            cfg.uv_window_secs
        ))
    } else if rail_low {
        Some(format!(
            "rail {}V < {}V for {}s",
            last_rail.map_or_else(|| "undefined".to_string(), |r| to_fixed(r, 2)),
            number_to_string(cfg.rail_low_volts),
            cfg.rail_low_secs
        ))
    } else if temp_high {
        Some(format!(
            "cpu {}°C >= {}°C for {}s",
            last_temp.map_or_else(|| "undefined".to_string(), |t| to_fixed(t, 1)),
            number_to_string(cfg.cpu_temp_c),
            cfg.cpu_temp_secs
        ))
    } else if soft_temp {
        Some("cpu soft temp limit (throttle bit 0x8)".to_string())
    } else {
        None
    };
    let cause = why.as_ref().map(|_| {
        if uv_fires || rail_low {
            ShedCause::Power
        } else {
            ShedCause::Thermal
        }
    });
    ShedSignals {
        trigger: why.is_some(),
        dirty,
        why,
        cause,
        last_dirty_ms,
    }
}

/// `TriggerEdge` (`boot-policy.ts:635-638`): the trigger became `on` at `tMs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerEdge {
    pub t_ms: i64,
    pub on: bool,
}

/// `ShedState` (`boot-policy.ts:641-665`) — plain data so the transition core
/// stays pure. Key order = `initialShedState`'s literal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShedState {
    pub level: ShedLevel,
    /// Cycle cap exhausted: no more restores until reboot/settings change.
    pub parked: bool,
    /// Last level transition (restore/escalate interval anchor).
    pub last_change_ms: i64,
    /// Trigger history over (at least) the trailing escalation window.
    pub trigger_log: Vec<TriggerEdge>,
    /// Shed cycle starts (level 0 → 1) within the rolling hour.
    pub shed_starts_ms: Vec<i64>,
    /// LATCHED cause of the shed in force; `None` at full operation.
    pub cause: Option<ShedCause>,
    /// The `ShedSignals.why` that latched `cause`.
    pub cause_detail: Option<String>,
}

/// `initialShedState` (`boot-policy.ts:667-677`).
pub fn initial_shed_state(now_ms: i64) -> ShedState {
    ShedState {
        level: ShedLevel::Full,
        parked: false,
        last_change_ms: now_ms,
        trigger_log: Vec::new(),
        shed_starts_ms: Vec::new(),
        cause: None,
        cause_detail: None,
    }
}

/// `appendTrigger` (`boot-policy.ts:684-696`): append this tick's trigger
/// state coalescing runs, then drop history older than the window — keeping
/// the one entry that straddles the window start.
fn append_trigger(log: &[TriggerEdge], on: bool, now_ms: i64, window_ms: i64) -> Vec<TriggerEdge> {
    let mut out = log.to_vec();
    if out.last().is_none_or(|e| e.on != on) {
        out.push(TriggerEdge { t_ms: now_ms, on });
    }
    let cutoff = now_ms - window_ms;
    let mut keep_from = 0;
    for (i, e) in out.iter().enumerate() {
        if e.t_ms <= cutoff {
            keep_from = i;
        }
    }
    if keep_from > 0 {
        out.drain(..keep_from);
    }
    out
}

/// `triggerDutyCycle` (`boot-policy.ts:705-722`): time-weighted fraction of
/// the trailing window the trigger was on; `None` while the window is not yet
/// fully covered since `since_ms` (the rung change).
pub fn trigger_duty_cycle(
    log: &[TriggerEdge],
    since_ms: i64,
    now_ms: i64,
    window_ms: i64,
) -> Option<f64> {
    if window_ms <= 0 {
        return Some(1.0);
    }
    let start = now_ms - window_ms;
    if start < since_ms {
        return None;
    }
    let mut on_ms: i64 = 0;
    for (i, e) in log.iter().enumerate() {
        let from = e.t_ms.max(start);
        let to = log.get(i + 1).map_or(now_ms, |n| n.t_ms);
        // Time before the first edge counts as "not triggered".
        if to > from && e.on {
            on_ms += to - from;
        }
    }
    Some(on_ms as f64 / window_ms as f64)
}

/// `ShedEvent.level` (`boot-policy.ts:725`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShedEventLevel {
    Info,
    Warn,
}

/// `ShedEvent` (`boot-policy.ts:724-727`): a log-worthy transition. The
/// `msg` wording is consumer-visible (activity log).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShedEvent {
    pub level: ShedEventLevel,
    pub msg: String,
}

/// The `{ state, event }` return of `decideShedTransition`.
#[derive(Debug, Clone, PartialEq)]
pub struct ShedTransition {
    pub state: ShedState,
    pub event: Option<ShedEvent>,
}

const HOUR_MS: i64 = 3_600_000;

/// `decideShedTransition` (`boot-policy.ts:742-836`): one evaluation tick of
/// the ladder. Escalation is duty-cycle based and scoped to cause `power`
/// (thermal caps at rung 1, D4/Q2); restore is one rung per clean
/// `restoreSecs` anchored on the last dirty sample; the rolling-hour cycle cap
/// parks.
pub fn decide_shed_transition(
    cfg: &ShedConfig,
    prev: &ShedState,
    sig: &ShedSignals,
    now_ms: i64,
) -> ShedTransition {
    let dwell_ms = i64::from(cfg.escalate_dwell_secs) * 1000;
    let mut s = ShedState {
        shed_starts_ms: prev
            .shed_starts_ms
            .iter()
            .copied()
            .filter(|t| now_ms - t <= HOUR_MS)
            .collect(),
        trigger_log: append_trigger(
            &prev.trigger_log,
            sig.trigger || sig.dirty,
            now_ms,
            dwell_ms,
        ),
        ..prev.clone()
    };

    if sig.trigger {
        // Latch the cause on the 0 → 1 edge; re-latch only when it changes.
        if sig.cause.is_some() && sig.cause != s.cause {
            s.cause = sig.cause;
            s.cause_detail = sig.why.clone();
        }
        let why = sig.why.as_deref().unwrap_or("null");
        if s.level == ShedLevel::Full {
            s.level = ShedLevel::NoLive;
            s.last_change_ms = now_ms;
            s.shed_starts_ms.push(now_ms);
            let mut msg =
                format!("power shed → no-live ({why}): live streaming off, recording continues");
            if s.shed_starts_ms.len() > cfg.max_sheds_per_hour as usize {
                s.parked = true;
                msg.push_str(&format!(
                    "; shed-cycle cap ({}/h) exhausted — parked until reboot or settings change",
                    cfg.max_sheds_per_hour
                ));
            }
            return ShedTransition {
                state: s,
                event: Some(ShedEvent {
                    level: ShedEventLevel::Warn,
                    msg,
                }),
            };
        }
        // D4/Q2: THERMAL CAPS AT RUNG 1 — only a power cause escalates.
        if s.level == ShedLevel::NoLive && sig.cause == Some(ShedCause::Power) {
            let duty = trigger_duty_cycle(&s.trigger_log, s.last_change_ms, now_ms, dwell_ms);
            if let Some(duty) = duty {
                if duty * 100.0 >= f64::from(cfg.escalate_duty_pct) {
                    s.level = ShedLevel::BridgeOnly;
                    s.last_change_ms = now_ms;
                    let msg = format!(
                        "power shed → bridge-only ({why}, triggered {}% of the last {}s past no-live): video recording off, telemetry recording continues",
                        number_to_string(js_math_round(duty * 100.0)),
                        cfg.escalate_dwell_secs
                    );
                    return ShedTransition {
                        state: s,
                        event: Some(ShedEvent {
                            level: ShedEventLevel::Warn,
                            msg,
                        }),
                    };
                }
            }
        }
        return ShedTransition {
            state: s,
            event: None,
        };
    }

    if s.level > ShedLevel::Full && !s.parked {
        // Fail open: with no dirty sample on record, the clean interval alone restores.
        let anchor = s.last_change_ms.max(sig.last_dirty_ms.unwrap_or(0));
        if now_ms - anchor >= i64::from(cfg.restore_secs) * 1000 {
            s.level = match s.level {
                ShedLevel::BridgeOnly => ShedLevel::NoLive,
                _ => ShedLevel::Full,
            };
            s.last_change_ms = now_ms;
            if s.level == ShedLevel::Full {
                s.cause = None;
                s.cause_detail = None;
            }
            let msg = format!(
                "power shed restore → {} (clean for {}s)",
                s.level.name(),
                cfg.restore_secs
            );
            return ShedTransition {
                state: s,
                event: Some(ShedEvent {
                    level: ShedEventLevel::Info,
                    msg,
                }),
            };
        }
    }
    ShedTransition {
        state: s,
        event: None,
    }
}

/// `ShedVerdict` (`boot-policy.ts:838-852`). Key order = `evaluate`'s
/// return literal: level, parked, event, reason, detail (explicit nulls).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShedVerdict {
    pub level: ShedLevel,
    pub parked: bool,
    pub event: Option<ShedEvent>,
    /// LATCHED reason for the shed in force (`liveBlock.reason` input).
    pub reason: Option<ShedBlockReason>,
    /// The trigger rationale that latched `reason`.
    pub detail: Option<String>,
}

/// `LoadShedPolicy` (`boot-policy.ts:871-943`): buffers pushed power samples,
/// evaluates the ladder on the manager's shed tick, never latches. Every
/// timestamp is the caller's MONOTONIC `mono_ms` (the TS `monoMs` option).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadShedPolicy {
    samples: Vec<PowerSample>,
    state: ShedState,
}

/// `push`'s backstop prune (`boot-policy.ts:893`).
const MAX_BUFFERED_SAMPLES: usize = 4000;

impl LoadShedPolicy {
    /// `new LoadShedPolicy()` (`boot-policy.ts:876-879`).
    pub fn new(mono_ms: MonoMs) -> Self {
        Self {
            samples: Vec::new(),
            state: initial_shed_state(mono_ms as i64),
        }
    }

    /// `push` (`boot-policy.ts:882-894`): record one reading (~1 Hz).
    pub fn push(&mut self, reading: &PowerSampleReading, mono_ms: MonoMs) {
        self.samples
            .push(PowerSample::from_reading(reading, mono_ms as i64));
        if self.samples.len() > MAX_BUFFERED_SAMPLES {
            let excess = self.samples.len() - MAX_BUFFERED_SAMPLES;
            self.samples.drain(..excess);
        }
    }

    /// `evaluate` (`boot-policy.ts:897-929`): evaluate the ladder for this tick.
    pub fn evaluate(&mut self, cfg: &ShedConfig, mono_ms: MonoMs) -> ShedVerdict {
        let now_ms = mono_ms as i64;
        if !cfg.enabled {
            let was_active = self.state.level > ShedLevel::Full || self.state.parked;
            self.reset(mono_ms);
            return ShedVerdict {
                level: ShedLevel::Full,
                parked: false,
                event: was_active.then(|| ShedEvent {
                    level: ShedEventLevel::Info,
                    msg: "power shedding disabled by settings — restoring full operation"
                        .to_string(),
                }),
                reason: None,
                detail: None,
            };
        }
        let horizon_ms = i64::from(
            cfg.uv_window_secs
                .max(cfg.rail_low_secs)
                .max(cfg.cpu_temp_secs)
                .max(cfg.restore_secs),
        ) * 1000
            + POWER_SAMPLE_STALE_MS;
        self.samples.retain(|s| now_ms - s.t_ms <= horizon_ms);
        let signals = compute_shed_signals(&self.samples, cfg, now_ms);
        let ShedTransition { state, event } =
            decide_shed_transition(cfg, &self.state, &signals, now_ms);
        self.state = state;
        ShedVerdict {
            level: self.state.level,
            parked: self.state.parked,
            event,
            // The one place the internal discriminant is renamed for the wire.
            reason: self.state.cause.map(|c| match c {
                ShedCause::Power => ShedBlockReason::Undervoltage,
                ShedCause::Thermal => ShedBlockReason::Thermal,
            }),
            detail: self.state.cause_detail.clone(),
        }
    }

    /// `reset` (`boot-policy.ts:932-935`): forget everything (settings change).
    pub fn reset(&mut self, mono_ms: MonoMs) {
        self.samples.clear();
        self.state = initial_shed_state(mono_ms as i64);
    }

    /// `statusString` (`boot-policy.ts:938-942`): `None` at full operation,
    /// else the rung name, `parked:`-prefixed when parked.
    pub fn status_string(&self) -> Option<String> {
        if self.state.level == ShedLevel::Full && !self.state.parked {
            return None;
        }
        let name = self.state.level.name();
        Some(if self.state.parked {
            format!("parked:{name}")
        } else {
            name.to_string()
        })
    }

    /// Read-only view of the ladder state (diagnostics / tests).
    pub fn state(&self) -> &ShedState {
        &self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_trigger_coalesces_and_keeps_the_straddling_edge() {
        let log = append_trigger(&[], true, 1000, 60_000);
        assert_eq!(
            log,
            vec![TriggerEdge {
                t_ms: 1000,
                on: true
            }]
        );
        let log = append_trigger(&log, true, 2000, 60_000);
        assert_eq!(log.len(), 1, "same state coalesces");
        let log = append_trigger(&log, false, 3000, 60_000);
        let log = append_trigger(&log, true, 70_000, 60_000);
        // cutoff = 10_000: edge@3000 straddles the window start and is kept.
        assert_eq!(
            log,
            vec![
                TriggerEdge {
                    t_ms: 3000,
                    on: false
                },
                TriggerEdge {
                    t_ms: 70_000,
                    on: true
                }
            ]
        );
    }

    #[test]
    fn duty_cycle_window_and_since() {
        let log = [
            TriggerEdge { t_ms: 0, on: true },
            TriggerEdge {
                t_ms: 30_000,
                on: false,
            },
        ];
        assert_eq!(trigger_duty_cycle(&log, 0, 60_000, 60_000), Some(0.5));
        assert_eq!(trigger_duty_cycle(&log, 1, 60_000, 60_000), None);
        assert_eq!(trigger_duty_cycle(&log, 0, 60_000, 0), Some(1.0));
        // A negative window start (early boot) is not an underflow.
        assert_eq!(trigger_duty_cycle(&log, 5_000, 3_000, 60_000), None);
    }

    #[test]
    fn decode_helpers() {
        assert_eq!(parse_budget_ma(&[0, 0, 0x0b, 0xb8]), Some(3000));
        assert_eq!(parse_budget_ma(&[0, 0, 0x13, 0x88, 0xff]), Some(5000));
        assert_eq!(parse_budget_ma(&[0, 0, 1]), None);
        assert_eq!(parse_policy_telemetry("{not json"), None);
        assert_eq!(parse_policy_telemetry("null"), None);
        assert_eq!(parse_policy_telemetry("42"), None);
        assert_eq!(parse_policy_telemetry("\"x\""), None);
        assert_eq!(
            parse_policy_telemetry("[1,2]"),
            Some(PolicyTelemetry::default())
        );
        let t = parse_policy_telemetry(
            r#"{"timestamp":"2026-10-09T12:00:00Z","status":"connecting","reg":1.0,"x":1}"#,
        )
        .unwrap();
        assert!(t.reg_is(1.0));
        assert_eq!(t.status_str(), Some("connecting"));
    }

    #[test]
    fn verdict_and_level_serde_shape() {
        let v = BootDeferVerdict::released("gps-fix");
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"allow":true,"state":"released","why":"gps-fix","holdLive":false}"#
        );
        let sv = ShedVerdict {
            level: ShedLevel::NoLive,
            parked: false,
            event: None,
            reason: Some(ShedBlockReason::Thermal),
            detail: Some("cpu 85.0°C >= 80°C for 15s".into()),
        };
        assert_eq!(
            serde_json::to_string(&sv).unwrap(),
            r#"{"level":1,"parked":false,"event":null,"reason":"thermal","detail":"cpu 85.0°C >= 80°C for 15s"}"#
        );
        assert!(serde_json::from_str::<ShedLevel>("3").is_err());
    }
}
