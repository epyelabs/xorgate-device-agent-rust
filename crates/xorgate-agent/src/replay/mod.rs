//! Replay mode (Phase 7e): the port of `src/agent/replay.ts` @ 09da38d, the
//! device simulator's third run mode. Sensors come from a recorded trace and
//! the camera from a recorded clip (the scenario asset pack,
//! `demo-assets/v1/<scenarioId>/`). Everything else is the unchanged agent:
//! MQTT, config, commands, recording, sidecars, uploads and the KVS masters.
//!
//! One LAP = one pass over the scenario. The capture supervisor plays
//! `cam0/lap.h264` at real time through the normal capture pipeline (a file
//! head in front of the unchanged tee). This source answers `read()` /
//! `sample_groups()` from the trace at the same offset. The lap clock starts
//! at gst-launch's PLAYING line (`New clock:`) and ends at its EOS. Then comes
//! a parked GAP (`simulation.gapSeconds`), then the next lap, or parked for
//! good when `simulation.loop` is false. Without a capture child the lap clock
//! runs on a 1 s self-timer (telemetry-only replay).
//!
//! # Threads and locks (P1)
//! The TS is single-threaded; here the self-timer, the capture supervisor's
//! stdout reader and exit handler, the config path and the publish loop can
//! call in at once. Two locks:
//! - `st` (the player state) is held only for in-memory work. Settings are
//!   read (a file read) BEFORE it is taken; scenarios are loaded (a file
//!   read and a gunzip) without it; the `replay:` log lines (a flushed
//!   `state.json` write) and every call into the recording manager are
//!   queued as [`After`] items and run, in order, after it is released.
//! - `reapply_gate` serialises whole `reapply` calls (read → load → switch →
//!   queued calls), so a slow, stale load can never land after a newer one
//!   (review-wp1 F1). It is taken before `st` and never by anything a
//!   manager call reaches.
//!
//! The capture supervisor calls `source_head` with ITS lock held (capture →
//! `st`, never the reverse; shown by code reading: `st` is never held across
//! a call out). It calls `on_ended` and then `respawn_hold_ms` with no lock
//! held, in the TS order (review-wp1 F5).
//!
//! # Errors the TS leaves uncaught
//! `host.rotateTelemetrySession` → `stopRecorder` → `recorder.stop()` can
//! throw (`manager.ts:811-818`). From the 1 s timer or gst's stdout callback
//! the throw is uncaught and Node exits 1; from the config path it lands in
//! `cloud config apply failed`. [`ReplaySource::reapply`] returns the error
//! to its caller (the config path); the timer and `on_playing` hand it to
//! [`ReplayHost::fatal`] (the manager's fatal route, exit 1).
//!
//! # Rust-only (accepted divergences, `phase-1/evidence/accepted-divergences.md`)
//! - D70: a trace leaf that is not `{value: number, unit?: string}` (or a
//!   group that is not an object) is left out of the live view and the
//!   recorder, with ONE warn per scenario load naming the paths; a unit-first
//!   leaf is re-serialised value-first. Seeding writes the raw JSON, as the
//!   TS does.
//! - D73: a malformed pack (not `build-scenario.mjs` output) fails with Rust
//!   text where the TS message is V8/zlib's (JSON syntax, gzip, type errors).

pub mod seed;

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Value;
use tokio::time::Instant;
use xorgate_core::settings::SimulationSettings;
use xorgate_core::stream_guard::StreamGuard;
use xorgate_core::telemetry::{
    ActivityLevel, JsNumber, MetricGroup, MetricLeaf, Metrics, TelemetryMedia, TelemetryReading,
};
use xorgate_core::video_capture::CaptureCamera;

use crate::ctx::{BoxFut, GroupSample, MediaControl, TelemetrySource};
use crate::recording::capture::ReplayCaptureHooks;

// ---------------------------------------------------------------- assets ----

/// `catalog.json` at the root of the asset pack (`replay.ts:38-44`): the
/// fields replay reads.
#[derive(Debug, Clone, Default)]
pub struct ReplayCatalog {
    /// `seedSet` (an array of ids, else `[]`, `replay.ts:89`).
    pub seed_set: Vec<String>,
    /// `scenarios[].id`, in catalog order (`None` = not a string).
    pub scenarios: Vec<Option<String>>,
}

impl ReplayCatalog {
    fn has(&self, id: &str) -> bool {
        self.scenarios.iter().any(|s| s.as_deref() == Some(id))
    }
    /// `catalog.scenarios[0].id` as a template string prints it.
    fn first_id(&self) -> String {
        self.scenarios
            .first()
            .cloned()
            .flatten()
            .unwrap_or_else(|| "undefined".into())
    }
}

fn nan() -> f64 {
    f64::NAN
}

/// `<scenarioId>/scenario.json` (`replay.ts:47-65`): the fields replay
/// reads. Fields `loadScenario` itself never touches default the way an
/// absent JS property reads (`NaN` for numbers) instead of failing the load.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioManifest {
    #[serde(default)]
    pub id: String,
    #[serde(default = "nan")]
    pub duration_ms: f64,
    #[serde(default = "nan")]
    pub original_start_ts: f64,
    #[serde(default = "nan")]
    pub original_end_ts: f64,
    #[serde(default)]
    pub streams: Vec<ScenarioStream>,
    #[serde(default)]
    pub telemetry: ScenarioTelemetry,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioStream {
    /// `None` = absent (a JS `undefined`; F6).
    #[serde(default)]
    pub stream_key: Option<String>,
    #[serde(default)]
    pub lap: Option<String>,
    #[serde(default = "nan")]
    pub fps: f64,
    #[serde(default)]
    pub fragments: Option<String>,
    #[serde(default)]
    pub fragment_count: Option<f64>,
    #[serde(default)]
    pub media_duration_ms: Option<f64>,
}

impl ScenarioStream {
    /// `streamKey` (`None` when absent: it then equals no camera's key).
    pub fn key(&self) -> Option<&str> {
        self.stream_key.as_deref()
    }
    /// `streamKey` as a template string prints it.
    pub fn key_text(&self) -> &str {
        self.key().unwrap_or("undefined")
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioTelemetry {
    #[serde(default)]
    pub trace: Option<String>,
    /// `None` = absent: the seeded header omits the key (F6).
    #[serde(default)]
    pub rate_hz: Option<f64>,
}

/// One normalized trace sample: `t` = ms since the lap's first frame.
#[derive(Debug, Clone)]
pub struct TraceSample {
    pub t: f64,
    /// The raw line's `seq` (`None` = absent: `JSON.stringify` omits it).
    pub seq: Option<Value>,
    /// The raw line's `metrics`, in V8 key order (seeding writes it as is).
    pub raw_metrics: Value,
    /// The live view of `metrics` (D70: wire leaves only).
    pub metrics: Metrics,
}

#[derive(Debug)]
pub struct LoadedScenario {
    pub manifest: ScenarioManifest,
    /// The scenario directory (`resolve(assetsDir, scenarioId)`).
    pub dir: PathBuf,
    /// Sorted by `t` (stable).
    pub samples: Vec<TraceSample>,
    /// One lap of video (`mediaDurationMs` when > 0, else `durationMs`).
    pub lap_ms: f64,
    /// D70: the trace paths (`group` or `group.field`) left out of the live view.
    pub dropped_leaves: BTreeSet<String>,
}

/// `JSON.parse` + V8 own-key order. The error text is serde's (D73).
fn parse_json(text: &str) -> Result<Value, String> {
    serde_json::from_str::<Value>(text)
        .map(xorgate_core::js::parsed_key_order)
        .map_err(|e| e.to_string())
}

/// `readFileSync(path, "utf8")` with Node's error message.
fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .map_err(|e| xorgate_hal::io::node_fs_message(&e, "open", path))
}

/// `loadCatalog(assetsDir)` (`replay.ts:84-90`).
pub fn load_catalog(assets_dir: &Path) -> Result<ReplayCatalog, String> {
    let p = assets_dir.join("catalog.json");
    let raw = parse_json(&read_text(&p)?)?;
    let not_v1 = || format!("{}: not a v1 catalog", p.display());
    if raw.get("v").and_then(Value::as_f64) != Some(1.0) {
        return Err(not_v1());
    }
    let Some(scenarios) = raw.get("scenarios").and_then(Value::as_array) else {
        return Err(not_v1());
    };
    let seed_set = raw
        .get("seedSet")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(ReplayCatalog {
        seed_set,
        scenarios: scenarios
            .iter()
            .map(|s| s.get("id").and_then(Value::as_str).map(str::to_string))
            .collect(),
    })
}

/// A leaf the live payload may carry: exactly `{value: number}` or
/// `{value: number, unit: string}` (agent-anatomy §10.11). D70: a unit-first
/// leaf passes and is re-serialised value-first (`MetricLeaf`'s order).
fn wire_leaf(leaf: &Value) -> Option<MetricLeaf> {
    let o = leaf.as_object()?;
    if o.keys().any(|k| k != "value" && k != "unit") {
        return None;
    }
    let value = o.get("value")?.as_f64()?;
    let unit = match o.get("unit") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    Some(MetricLeaf {
        value: JsNumber(value),
        unit,
    })
}

/// The live view of one sample's `metrics` (D70), plus the dropped paths.
fn metrics_of(v: &Value, dropped: &mut BTreeSet<String>) -> Metrics {
    let mut out = Metrics::new();
    let Some(groups) = v.as_object() else {
        // An array `metrics` (JS `typeof [] === "object"` lets it in).
        if v.as_array().is_some_and(|a| !a.is_empty()) {
            dropped.insert("metrics".into());
        }
        return out;
    };
    for (g, fields) in groups {
        let Some(fields) = fields.as_object() else {
            dropped.insert(g.clone());
            continue;
        };
        let mut grp = MetricGroup::new();
        for (f, leaf) in fields {
            match wire_leaf(leaf) {
                Some(l) => {
                    grp.insert(f.clone(), l);
                }
                None => {
                    dropped.insert(format!("{g}.{f}"));
                }
            }
        }
        out.insert(g.clone(), grp);
    }
    out
}

/// `path.join(dir, x)` when `x` is undefined: Node's TypeError text.
fn path_arg(x: Option<&String>) -> Result<&str, String> {
    x.map(String::as_str).ok_or_else(|| {
        "The \"path\" argument must be of type string. Received undefined".to_string()
    })
}

/// `loadScenario(assetsDir, scenarioId)` (`replay.ts:92-117`).
pub fn load_scenario(assets_dir: &Path, scenario_id: &str) -> Result<LoadedScenario, String> {
    let dir = std::path::absolute(assets_dir.join(scenario_id))
        .unwrap_or_else(|_| assets_dir.join(scenario_id));
    let mp = dir.join("scenario.json");
    if !mp.exists() {
        return Err(format!("scenario {scenario_id}: {} missing", mp.display()));
    }
    let raw = parse_json(&read_text(&mp)?)?;
    let not_v1 =
        || format!("scenario {scenario_id}: scenario.json is not a v1 manifest for {scenario_id}");
    let v1 = raw.get("v").and_then(Value::as_f64) == Some(1.0)
        && raw.get("id").and_then(Value::as_str) == Some(scenario_id)
        && raw
            .get("streams")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
    if !v1 {
        return Err(not_v1());
    }
    let manifest: ScenarioManifest = serde_json::from_value(raw).map_err(|e| e.to_string())?;
    for st in &manifest.streams {
        let lap = path_arg(st.lap.as_ref())?;
        if !dir.join(lap).exists() {
            return Err(format!("scenario {scenario_id}: {lap} missing"));
        }
    }
    let trace = dir.join(path_arg(manifest.telemetry.trace.as_ref())?);
    let gz =
        std::fs::read(&trace).map_err(|e| xorgate_hal::io::node_fs_message(&e, "open", &trace))?;
    let mut bytes = Vec::new();
    flate2::read::MultiGzDecoder::new(&gz[..])
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let mut samples = Vec::new();
    let mut dropped = BTreeSet::new();
    for line in text.split('\n') {
        if line.is_empty() {
            continue;
        }
        let o = parse_json(line)?;
        if o.is_null() {
            return Err("Cannot read properties of null (reading 't')".into());
        }
        let (Some(t), Some(m)) = (
            o.get("t").filter(|t| t.is_number()).and_then(Value::as_f64),
            o.get("metrics"),
        ) else {
            continue;
        };
        if !(m.is_object() || m.is_array()) {
            continue;
        }
        samples.push(TraceSample {
            t,
            seq: o.get("seq").cloned(),
            raw_metrics: m.clone(),
            metrics: metrics_of(m, &mut dropped),
        });
    }
    samples.sort_by(|a, b| a.t.total_cmp(&b.t));
    if samples.is_empty() {
        return Err(format!("scenario {scenario_id}: empty trace"));
    }
    let first = &manifest.streams[0];
    let lap_ms = match first.media_duration_ms {
        Some(d) if d > 0.0 => d,
        _ => manifest.duration_ms,
    };
    Ok(LoadedScenario {
        manifest,
        dir,
        samples,
        lap_ms,
        dropped_leaves: dropped,
    })
}

// --------------------------------------------------------------- sampler ----

/// `sampleAt`: nearest sample at or before `offset_ms`, or `None` before the first.
pub fn sample_at(samples: &[TraceSample], offset_ms: f64) -> Option<usize> {
    if samples.is_empty() || offset_ms < samples[0].t {
        return None;
    }
    Some(samples.partition_point(|s| s.t <= offset_ms) - 1)
}

/// `parkedMetrics`: same position, `gps.speed` `{value: 0}` with the old
/// unit when it was a non-empty string (`speed?.unit ? {unit} : {}`).
pub fn parked_metrics(m: &Metrics) -> Metrics {
    let mut out = m.clone();
    if let Some(gps) = out.get_mut("gps") {
        let unit = gps
            .get("speed")
            .and_then(|l| l.unit.clone())
            .filter(|u| !u.is_empty());
        gps.insert(
            "speed".into(),
            MetricLeaf {
                value: JsNumber(0.0),
                unit,
            },
        );
    }
    out
}

/// `replayCaptureHead(lapPath, fps)`: the Phase 0 recipe, token for token.
pub fn replay_capture_head(lap_path: &Path, fps: f64) -> Vec<String> {
    vec![
        "filesrc".into(),
        format!("location={}", lap_path.display()),
        "!".into(),
        "h264parse".into(),
        "!".into(),
        format!(
            "video/x-h264,framerate={}/1",
            xorgate_core::js::number_to_string(fps)
        ),
        "!".into(),
        "avdec_h264".into(),
        "!".into(),
        "identity".into(),
        "sync=true".into(),
        "!".into(),
        "videoconvert".into(),
        "!".into(),
        "videorate".into(),
    ]
}

// ------------------------------------------------------------- lap clock ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayPhase {
    Idle,
    Lap,
    Gap,
    Parked,
}

impl ReplayPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Lap => "lap",
            Self::Gap => "gap",
            Self::Parked => "parked",
        }
    }
}

/// `player.state()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerState {
    pub phase: ReplayPhase,
    pub lap_index: u64,
    /// Trace position in the lap; 0 outside `lap`.
    pub offset_ms: f64,
    pub lap_start_wall: i64,
}

/// `ReplayPlayer` (`replay.ts:184-302`): the lap/gap state machine over
/// tokio's monotonic clock (`monoMs` and `hrtime` in one; tests pause it).
pub struct ReplayPlayer {
    scenario: Arc<LoadedScenario>,
    phase: ReplayPhase,
    lap_start: Instant,
    lap_start_wall: i64,
    lap_index: u64,
    last_sample: Option<Metrics>,
    phase_since: Instant,
}

impl ReplayPlayer {
    pub fn new(scenario: Arc<LoadedScenario>) -> Self {
        let now = Instant::now();
        Self {
            scenario,
            phase: ReplayPhase::Idle,
            lap_start: now,
            lap_start_wall: 0,
            lap_index: 0,
            last_sample: None,
            phase_since: now,
        }
    }
    pub fn current(&self) -> &Arc<LoadedScenario> {
        &self.scenario
    }
    /// Swap the scenario; the running lap (if any) keeps playing until ended.
    pub fn set_scenario(&mut self, s: Arc<LoadedScenario>) {
        self.scenario = s;
    }
    pub fn state(&self) -> PlayerState {
        PlayerState {
            phase: self.phase,
            lap_index: self.lap_index,
            offset_ms: if self.phase == ReplayPhase::Lap {
                self.offset_ms()
            } else {
                0.0
            },
            lap_start_wall: self.lap_start_wall,
        }
    }
    pub fn phase(&self) -> ReplayPhase {
        self.phase
    }
    pub fn lap_index(&self) -> u64 {
        self.lap_index
    }
    pub fn phase_age_ms(&self) -> f64 {
        self.phase_since.elapsed().as_secs_f64() * 1000.0
    }
    pub fn offset_ms(&self) -> f64 {
        self.lap_start.elapsed().as_secs_f64() * 1000.0
    }
    pub fn lap_elapsed(&self) -> bool {
        self.phase == ReplayPhase::Lap && self.offset_ms() >= self.scenario.lap_ms
    }
    pub fn start_lap(&mut self, wall_ms: i64) {
        self.lap_start = Instant::now();
        self.lap_start_wall = wall_ms;
        self.lap_index += 1;
        self.phase = ReplayPhase::Lap;
        self.phase_since = self.lap_start;
    }
    pub fn end_lap(&mut self, loop_: bool) {
        if self.phase == ReplayPhase::Lap {
            if let Some(i) = sample_at(&self.scenario.samples, self.offset_ms()) {
                self.last_sample = Some(self.scenario.samples[i].metrics.clone());
            }
        }
        self.phase = if loop_ {
            ReplayPhase::Gap
        } else {
            ReplayPhase::Parked
        };
        self.phase_since = Instant::now();
    }
    pub fn read_metrics(&mut self) -> Metrics {
        match self.phase {
            ReplayPhase::Lap => match sample_at(&self.scenario.samples, self.offset_ms()) {
                Some(i) => {
                    let m = self.scenario.samples[i].metrics.clone();
                    self.last_sample = Some(m.clone());
                    m
                }
                None => Metrics::new(),
            },
            ReplayPhase::Gap | ReplayPhase::Parked => self
                .last_sample
                .as_ref()
                .map(parked_metrics)
                .unwrap_or_default(),
            ReplayPhase::Idle => Metrics::new(),
        }
    }
    /// Recorder view: a REAL `updated_mono` per group (the sample's own
    /// time in the lap's frame) so the freshness gate writes it once.
    pub fn sample_groups(&mut self) -> IndexMap<String, GroupSample> {
        let mut out = IndexMap::new();
        match self.phase {
            ReplayPhase::Lap => {
                let Some(i) = sample_at(&self.scenario.samples, self.offset_ms()) else {
                    return out;
                };
                let s = &self.scenario.samples[i];
                self.last_sample = Some(s.metrics.clone());
                let at = if s.t >= 0.0 {
                    self.lap_start + Duration::from_secs_f64(s.t / 1000.0)
                } else {
                    self.lap_start
                        .checked_sub(Duration::from_secs_f64(-s.t / 1000.0))
                        .unwrap_or(self.lap_start)
                };
                for (g, fields) in &s.metrics {
                    out.insert(
                        g.clone(),
                        GroupSample {
                            fields: fields.clone(),
                            updated_mono: at,
                        },
                    );
                }
            }
            ReplayPhase::Gap | ReplayPhase::Parked => {
                if let Some(m) = &self.last_sample {
                    let now = Instant::now();
                    for (g, fields) in parked_metrics(m) {
                        out.insert(
                            g,
                            GroupSample {
                                fields,
                                updated_mono: now,
                            },
                        );
                    }
                }
            }
            ReplayPhase::Idle => {}
        }
        out
    }
}

// ---------------------------------------------------------------- source ----

/// What the replay source needs from the recording manager (`ReplayHost`,
/// `replay.ts:309-314`).
pub trait ReplayHost: Send + Sync {
    /// End every camera's lap cleanly now; respawn after `respawn_hold_ms`.
    fn cycle_video(&self);
    /// Close the telemetry session and open a new one (one pair per lap).
    /// `Err` = the TS throw out of `stopRecorder`.
    fn rotate_telemetry_session(&self, why: &str) -> Result<(), String>;
    /// A throw the TS leaves uncaught (timer or stdout callback): Node exits 1.
    fn fatal(&self, msg: String);
}

/// `VIDEO_GRACE_MS`: grace before the self-timer assumes no capture child.
pub const VIDEO_GRACE_MS: f64 = 45_000.0;
const SETTINGS_POLL_TICKS: u64 = 5;

/// `() => loadSettings().simulation`.
pub type SettingsFn = Arc<dyn Fn() -> Option<SimulationSettings> + Send + Sync>;
/// `onEvent(level, "replay: …")`.
pub type ReplayEventFn = Arc<dyn Fn(ActivityLevel, &str) + Send + Sync>;
/// `Date.now()`.
pub type WallFn = Arc<dyn Fn() -> i64 + Send + Sync>;

/// `ReplaySourceOptions` (`replay.ts:316-329`).
pub struct ReplayOptions {
    pub assets_dir: PathBuf,
    pub default_scenario_id: Option<String>,
    pub settings: SettingsFn,
    /// The KVS master supervisor (the media block), or `None` without media.
    pub media: Option<Arc<dyn MediaControl>>,
    pub on_event: Option<ReplayEventFn>,
    pub now: WallFn,
    /// Self-timer period; 0 disables it (tests drive `tick()`).
    pub tick_ms: u64,
}

/// `source.status()`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayStatus {
    pub scenario_id: String,
    pub phase: ReplayPhase,
    pub lap_index: u64,
    pub offset_ms: f64,
    pub video_driven: bool,
}

struct St {
    scenario_id: String,
    player: ReplayPlayer,
    /// A capture child has reported PLAYING at least once this process.
    video_driven: bool,
    /// A scenario switch is waiting for the respawn: hold 0 instead of the gap.
    switch_pending: bool,
    ticks: u64,
    stopped: bool,
}

/// Work queued under the state lock, run in order after it is released:
/// the `replay:` log lines (a `state.json` write) and the manager calls.
enum After {
    Log(ActivityLevel, String),
    Rotate(String),
    Cycle,
}

/// `loop()` / `gapMs()` from one read of the `simulation` settings.
#[derive(Clone, Copy)]
struct Sim {
    loop_: bool,
    gap_ms: f64,
}

pub struct ReplaySource {
    assets_dir: PathBuf,
    default_scenario_id: Option<String>,
    settings: SettingsFn,
    catalog: ReplayCatalog,
    media: Option<Arc<dyn MediaControl>>,
    on_event: Option<ReplayEventFn>,
    now: WallFn,
    st: Mutex<St>,
    /// F1 (review-wp1): serialises whole `reapply` calls (settings read →
    /// load → switch → its manager calls), so a slow, stale load can never
    /// land after a newer one (the single-threaded TS order). Taken before
    /// `st`, never by anything a manager call can reach.
    reapply_gate: Mutex<()>,
    host: OnceLock<Arc<dyn ReplayHost>>,
}

impl ReplaySource {
    /// `createReplaySource(opts)`: loads the catalog and the starting scenario
    /// (an error = the TS throw at worker start → exit 1), then starts the
    /// self-timer.
    pub fn create(opts: ReplayOptions) -> Result<Arc<Self>, String> {
        let catalog = load_catalog(&opts.assets_dir)?;
        if catalog.scenarios.is_empty() {
            return Err("replay: the catalog has no scenarios".into());
        }
        let mut me = Self {
            assets_dir: opts.assets_dir,
            default_scenario_id: opts.default_scenario_id,
            settings: opts.settings,
            catalog,
            media: opts.media,
            on_event: opts.on_event,
            now: opts.now,
            st: Mutex::new(St {
                scenario_id: String::new(),
                player: ReplayPlayer::new(Arc::new(LoadedScenario {
                    manifest: serde_json::from_value(serde_json::json!({}))
                        .expect("all fields default"),
                    dir: PathBuf::new(),
                    samples: Vec::new(),
                    lap_ms: 0.0,
                    dropped_leaves: BTreeSet::new(),
                })),
                video_driven: false,
                switch_pending: false,
                ticks: 0,
                stopped: false,
            }),
            reapply_gate: Mutex::new(()),
            host: OnceLock::new(),
        };
        let id = me.resolve_scenario_id();
        let sc = Arc::new(load_scenario(&me.assets_dir, &id)?);
        {
            let st = me.st.get_mut().unwrap_or_else(|e| e.into_inner());
            st.scenario_id = id.clone();
            st.player = ReplayPlayer::new(sc.clone());
        }
        me.log(
            ActivityLevel::Info,
            &format!(
                "scenario {id} loaded ({} samples, lap {} s)",
                sc.samples.len(),
                js_round(sc.lap_ms / 1000.0)
            ),
        );
        me.warn_dropped(&sc);
        let me = Arc::new(me);
        if opts.tick_ms > 0 {
            let weak: Weak<Self> = Arc::downgrade(&me);
            let period = Duration::from_millis(opts.tick_ms);
            tokio::spawn(async move {
                let mut iv = crate::timer::SetInterval::new(period);
                loop {
                    iv.tick().await;
                    let Some(s) = weak.upgrade() else { break };
                    if s.lock().stopped {
                        break;
                    }
                    s.tick();
                }
            });
        }
        Ok(me)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `log(level, msg)` → `onEvent(level, "replay: " + msg)`.
    fn log(&self, level: ActivityLevel, msg: &str) {
        if let Some(f) = &self.on_event {
            f(level, &format!("replay: {msg}"));
        }
    }

    /// D70: one warn per scenario load naming the left-out trace paths.
    fn warn_dropped(&self, sc: &LoadedScenario) {
        if let Some(m) = dropped_msg(sc) {
            self.log(ActivityLevel::Warn, &m);
        }
    }
}

/// D70: the one warn per scenario load naming the left-out trace paths.
fn dropped_msg(sc: &LoadedScenario) -> Option<String> {
    {
        if sc.dropped_leaves.is_empty() {
            return None;
        }
        let shown: Vec<&str> = sc
            .dropped_leaves
            .iter()
            .take(10)
            .map(String::as_str)
            .collect();
        let more = sc.dropped_leaves.len() - shown.len();
        Some(format!(
            "scenario {}: {} trace path(s) are not {{value:number, unit?:string}} and are left out of the live view: {}{}",
            sc.manifest.id,
            sc.dropped_leaves.len(),
            shown.join(", "),
            if more > 0 { format!(" (+{more} more)") } else { String::new() }
        ))
    }
}

impl ReplaySource {
    /// One read of `simulation` (`loop() ?? true`, `(gapSeconds ?? 30) * 1000`).
    /// Read BEFORE the state lock is taken (F4): it is a file read.
    fn sim(&self) -> Sim {
        let s = (self.settings)();
        Sim {
            loop_: s.as_ref().and_then(|s| s.loop_).unwrap_or(true),
            gap_ms: f64::from(s.and_then(|s| s.gap_seconds).unwrap_or(30)) * 1000.0,
        }
    }

    /// `resolveScenarioId()` (`replay.ts:369-374`): `settings.scenarioId ??
    /// config.defaultScenarioId`, an empty string being falsy.
    fn resolve_scenario_id(&self) -> String {
        let wanted = (self.settings)()
            .and_then(|s| s.scenario_id)
            .or_else(|| self.default_scenario_id.clone())
            .filter(|w| !w.is_empty());
        if let Some(w) = &wanted {
            if self.catalog.has(w) {
                return w.clone();
            }
            self.log(
                ActivityLevel::Warn,
                &format!(
                    "scenario \"{w}\" is not in the catalog — using {}",
                    self.catalog.first_id()
                ),
            );
        }
        self.catalog.first_id()
    }

    pub fn attach_host(&self, host: Arc<dyn ReplayHost>) {
        let _ = self.host.set(host);
    }

    pub fn catalog(&self) -> &ReplayCatalog {
        &self.catalog
    }

    pub fn assets_dir(&self) -> &Path {
        &self.assets_dir
    }

    pub fn scenario(&self) -> Arc<LoadedScenario> {
        self.lock().player.current().clone()
    }

    /// `clockStream()`: the scenario's first stream drives the lap clock.
    fn clock_stream(st: &St) -> Option<String> {
        st.player.current().manifest.streams[0].stream_key.clone()
    }

    /// `streamGuard`: streams the scenario has no footage for get no master.
    pub fn stream_guard(&self, key: &str) -> StreamGuard {
        let st = self.lock();
        if st
            .player
            .current()
            .manifest
            .streams
            .iter()
            .any(|s| s.key() == Some(key))
        {
            StreamGuard::Ok
        } else {
            StreamGuard::NoPipeline
        }
    }

    fn begin_lap(&self, st: &mut St, wall_ms: i64, how: &str, after: &mut Vec<After>) {
        st.player.start_lap(wall_ms);
        st.switch_pending = false;
        let idx = st.player.lap_index();
        after.push(After::Log(
            ActivityLevel::Info,
            format!(
                "lap {idx} of {} started ({how})",
                st.player.current().manifest.id
            ),
        ));
        after.push(After::Rotate(format!("replay lap {idx}")));
    }

    fn finish_lap(&self, st: &mut St, how: &str, sim: Sim, after: &mut Vec<After>) {
        st.player.end_lap(sim.loop_);
        let msg = if st.switch_pending {
            format!(
                "lap ended ({how}); switching to {}",
                st.player.current().manifest.id
            )
        } else if sim.loop_ {
            format!(
                "lap ended ({how}); parked for {} s",
                js_round(sim.gap_ms / 1000.0)
            )
        } else {
            format!("lap ended ({how}); parked — loop is off")
        };
        after.push(After::Log(ActivityLevel::Info, msg));
    }

    /// Runs the queued work, state lock released, in order. The first
    /// manager error stops the rest (the TS throw). Log lines always run
    /// (`host?.` only guards the manager calls).
    fn run_after(&self, after: Vec<After>) -> Result<(), String> {
        let host = self.host.get();
        for a in after {
            match a {
                After::Log(level, msg) => self.log(level, &msg),
                After::Rotate(why) => {
                    if let Some(h) = host {
                        h.rotate_telemetry_session(&why)?;
                    }
                }
                After::Cycle => {
                    if let Some(h) = host {
                        h.cycle_video();
                    }
                }
            }
        }
        Ok(())
    }

    /// An error the TS would leave uncaught.
    fn fatal(&self, e: String) {
        if let Some(h) = self.host.get() {
            h.fatal(e);
        }
    }

    /// `reapply()`: re-read `simulation`; a changed scenario ends the lap in
    /// place. `Err` = a throw out of the telemetry rotation. Whole calls are
    /// serialised by `reapply_gate` (F1), so the read → load → switch
    /// sequence is atomic against every other `reapply`, as in the TS. The
    /// gate is held across the queued manager calls too: nothing a
    /// `cycle_video` / `rotate_telemetry_session` reaches calls `reapply`.
    pub fn reapply(&self) -> Result<(), String> {
        let _gate = self.reapply_gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.lock().stopped {
            return Ok(());
        }
        let next = self.resolve_scenario_id();
        let current = self.lock().scenario_id.clone();
        if next == current {
            return Ok(());
        }
        let loaded = match load_scenario(&self.assets_dir, &next) {
            Ok(l) => Arc::new(l),
            Err(e) => {
                self.log(
                    ActivityLevel::Warn,
                    &format!("scenario {next} failed to load ({e}); keeping {current}"),
                );
                return Ok(());
            }
        };
        let sim = self.sim();
        let now = (self.now)();
        let mut after = Vec::new();
        {
            let mut st = self.lock();
            if st.stopped {
                return Ok(());
            }
            st.scenario_id = next.clone();
            st.player.set_scenario(loaded.clone());
            st.switch_pending = true;
            after.push(After::Log(
                ActivityLevel::Info,
                format!("scenario switched to {next} — ending the current lap"),
            ));
            if let Some(m) = dropped_msg(&loaded) {
                after.push(After::Log(ActivityLevel::Warn, m));
            }
            if st.video_driven && self.host.get().is_some() {
                // → on_ended → respawn_hold_ms() = 0 → new child → on_playing
                after.push(After::Cycle);
            } else {
                // Telemetry-only: no child to cycle, restart the lap clock now.
                if st.player.phase() == ReplayPhase::Lap {
                    self.finish_lap(&mut st, "scenario switch", sim, &mut after);
                }
                self.begin_lap(&mut st, now, "scenario switch", &mut after);
            }
        }
        self.run_after(after)
    }

    /// One clock tick (the self-timer; public for tests).
    pub fn tick(&self) {
        let poll = {
            let mut st = self.lock();
            if st.stopped {
                return;
            }
            st.ticks += 1;
            st.ticks.is_multiple_of(SETTINGS_POLL_TICKS)
        };
        if poll {
            if let Err(e) = self.reapply() {
                self.fatal(e);
                return;
            }
        }
        let sim = self.sim();
        let now = (self.now)();
        let mut after = Vec::new();
        {
            let mut st = self.lock();
            let phase = st.player.phase();
            if st.video_driven {
                // The capture child owns the lap clock. The timer only steps
                // in when a respawn is overdue (gap + grace, no PLAYING).
                if phase == ReplayPhase::Gap
                    && st.player.phase_age_ms() > sim.gap_ms + VIDEO_GRACE_MS
                {
                    after.push(After::Log(
                        ActivityLevel::Warn,
                        "no capture child came back after the gap — running the lap clock without video".into(),
                    ));
                    st.video_driven = false;
                    self.begin_lap(&mut st, now, "timer fallback", &mut after);
                }
            } else if phase == ReplayPhase::Idle && st.player.phase_age_ms() > VIDEO_GRACE_MS {
                after.push(After::Log(
                    ActivityLevel::Info,
                    "no capture child reported PLAYING — telemetry-only replay".into(),
                ));
                self.begin_lap(&mut st, now, "timer, no video", &mut after);
            } else if phase == ReplayPhase::Lap && st.player.lap_elapsed() {
                self.finish_lap(&mut st, "lap time elapsed", sim, &mut after);
            } else if phase == ReplayPhase::Gap && st.player.phase_age_ms() >= sim.gap_ms {
                self.begin_lap(&mut st, now, "timer", &mut after);
            }
        }
        if let Err(e) = self.run_after(after) {
            self.fatal(e);
        }
    }

    /// `status()` (diagnostics and tests).
    pub fn status(&self) -> ReplayStatus {
        let st = self.lock();
        let p = st.player.state();
        ReplayStatus {
            scenario_id: st.scenario_id.clone(),
            phase: p.phase,
            lap_index: p.lap_index,
            offset_ms: p.offset_ms,
            video_driven: st.video_driven,
        }
    }
}

impl ReplayCaptureHooks for ReplaySource {
    fn source_head(&self, cam: &CaptureCamera) -> Vec<String> {
        let st = self.lock();
        let sc = st.player.current();
        let stream = sc
            .manifest
            .streams
            .iter()
            .find(|s| s.key() == Some(cam.stream_key.as_str()))
            .unwrap_or(&sc.manifest.streams[0]);
        replay_capture_head(
            &sc.dir.join(stream.lap.as_deref().unwrap_or("")),
            stream.fps,
        )
    }

    fn on_playing(&self, key: &str, wall_ms: i64) {
        let mut after = Vec::new();
        {
            let mut st = self.lock();
            if st.stopped || Some(key) != Self::clock_stream(&st).as_deref() {
                return;
            }
            st.video_driven = true;
            self.begin_lap(&mut st, wall_ms, "video playing", &mut after);
        }
        if let Err(e) = self.run_after(after) {
            self.fatal(e);
        }
    }

    fn on_ended(&self, key: &str, clean: bool) {
        let sim = self.sim();
        let mut after = Vec::new();
        {
            let mut st = self.lock();
            if st.stopped || Some(key) != Self::clock_stream(&st).as_deref() {
                return;
            }
            if st.player.phase() != ReplayPhase::Lap {
                return;
            }
            let how = if clean { "video EOS" } else { "capture exited" };
            self.finish_lap(&mut st, how, sim, &mut after);
        }
        let _ = self.run_after(after); // log lines only
    }

    fn respawn_hold_ms(&self, key: &str) -> Option<u64> {
        {
            let st = self.lock();
            if Some(key) != Self::clock_stream(&st).as_deref() || st.switch_pending {
                return Some(0);
            }
        }
        let sim = self.sim();
        sim.loop_.then_some(sim.gap_ms as u64)
    }
}

impl TelemetrySource for ReplaySource {
    fn read(&self) -> BoxFut<'_, Result<TelemetryReading, String>> {
        Box::pin(async move {
            let metrics = self.lock().player.read_metrics();
            let media = match &self.media {
                Some(m) => m.read(),
                None => TelemetryMedia::idle(),
            };
            Ok(TelemetryReading { metrics, media })
        })
    }
    fn sample_groups(&self) -> BoxFut<'_, IndexMap<String, GroupSample>> {
        Box::pin(async move { self.lock().player.sample_groups() })
    }
    fn sample_power(&self) -> BoxFut<'_, Option<xorgate_core::types::PowerSampleReading>> {
        Box::pin(async { None })
    }
    fn gps_clock(&self) -> Option<xorgate_core::types::GpsClockReading> {
        None
    }
    fn gnss_view(&self) -> Option<xorgate_core::types::GnssView> {
        None
    }
    fn stop(&self) {
        self.lock().stopped = true;
        if let Some(m) = &self.media {
            m.stop();
        }
    }
}

/// JS `Math.round` (ties toward +∞) as an integer for log text.
/// JS `Math.round(x)` as a template string prints it (`NaN` stays `NaN`).
pub(crate) fn js_round(x: f64) -> String {
    xorgate_core::js::number_to_string(js_math_round(x))
}

/// JS `Math.round`: ties toward +∞, NaN stays NaN.
pub(crate) fn js_math_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

#[cfg(test)]
mod tests;
