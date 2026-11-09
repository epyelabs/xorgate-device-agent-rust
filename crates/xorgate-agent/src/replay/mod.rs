//! Replay mode — Phase 7e SPIKE port of `src/agent/replay.ts` @ 09da38d
//! (the device simulator's third run mode). Sensors come from a recorded
//! trace and the camera from a recorded clip (the scenario asset pack,
//! `demo-assets/v1/<scenarioId>/`); everything else — MQTT, config,
//! commands, recording, sidecars, uploads, the KVS masters — is the
//! unchanged agent.
//!
//! One LAP = one pass over the scenario: the capture supervisor plays
//! `cam0/lap.h264` at real time through the normal capture pipeline (a file
//! head in front of the unchanged tee), and this source answers `read()` /
//! `sample_groups()` from the trace at the same offset. The lap clock starts
//! at gst-launch's PLAYING line (`New clock:`), ends at its EOS; then a
//! parked GAP (`simulation.gapSeconds`), then the next lap (or parked for
//! good when `simulation.loop` is false). Without a capture child the lap
//! clock runs on a 1 s self-timer (telemetry-only replay).
//!
//! SPIKE status: functionally complete for the sim image's contract; not
//! reviewed line-by-line against the TS, no rig tapes. Known gaps are listed
//! in `plans/device-agent-js-to-rust/phase-7e-sim-replay.md`.

pub mod seed;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;
use tokio::time::Instant;
use tracing::{info, warn};
use xorgate_core::stream_guard::StreamGuard;
use xorgate_core::telemetry::{
    ActivityLevel, MetricGroup, MetricLeaf, Metrics, TelemetryMedia, TelemetryReading,
};
use xorgate_core::video_capture::CaptureCamera;

use crate::ctx::{BoxFut, GroupSample, MediaControl, TelemetrySource};
use crate::recording::capture::ReplayCaptureHooks;
use crate::state::StateHandle;

// ---------------------------------------------------------------- assets ----

/// `catalog.json` at the root of the asset pack (the fields replay reads).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCatalog {
    pub v: u32,
    #[serde(default)]
    pub seed_set: Vec<String>,
    pub scenarios: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub id: String,
}

/// `<scenarioId>/scenario.json` (the fields replay reads).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioManifest {
    pub v: u32,
    pub id: String,
    pub duration_ms: f64,
    pub original_start_ts: f64,
    pub original_end_ts: f64,
    pub streams: Vec<ScenarioStream>,
    pub telemetry: ScenarioTelemetry,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioStream {
    pub stream_key: String,
    pub lap: String,
    pub fps: f64,
    pub fragments: String,
    pub fragment_count: u64,
    #[serde(default)]
    pub media_duration_ms: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioTelemetry {
    pub trace: String,
    pub rate_hz: f64,
}

/// One normalized trace sample: `t` = ms since the lap's first frame.
#[derive(Debug, Clone)]
pub struct TraceSample {
    pub t: f64,
    /// The raw line's `seq` and `metrics`, key order kept (seeding writes them).
    pub seq: Value,
    pub raw_metrics: Value,
    /// The live view of `metrics` (wire leaves only).
    pub metrics: Metrics,
}

#[derive(Debug)]
pub struct LoadedScenario {
    pub manifest: ScenarioManifest,
    pub dir: PathBuf,
    /// Sorted by `t`.
    pub samples: Vec<TraceSample>,
    /// One lap of video (frames / fps when known, else the sidecar window).
    pub lap_ms: f64,
}

pub fn load_catalog(assets_dir: &Path) -> Result<ReplayCatalog, String> {
    let p = assets_dir.join("catalog.json");
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let c: ReplayCatalog =
        serde_json::from_str(&text).map_err(|_| format!("{}: not a v1 catalog", p.display()))?;
    if c.v != 1 {
        return Err(format!("{}: not a v1 catalog", p.display()));
    }
    Ok(c)
}

/// The JS leaf shapes the live payload carries: `{value:number, unit?}`.
fn metrics_of(v: &Value) -> Metrics {
    let mut out = Metrics::new();
    let Some(groups) = v.as_object() else {
        return out;
    };
    for (g, fields) in groups {
        let Some(fields) = fields.as_object() else {
            continue;
        };
        let mut grp = MetricGroup::new();
        for (f, leaf) in fields {
            if let Ok(l) = serde_json::from_value::<MetricLeaf>(leaf.clone()) {
                grp.insert(f.clone(), l);
            }
        }
        out.insert(g.clone(), grp);
    }
    out
}

pub fn load_scenario(assets_dir: &Path, scenario_id: &str) -> Result<LoadedScenario, String> {
    let dir = assets_dir.join(scenario_id);
    let mp = dir.join("scenario.json");
    let text = std::fs::read_to_string(&mp)
        .map_err(|_| format!("scenario {scenario_id}: {} missing", mp.display()))?;
    let manifest: ScenarioManifest = serde_json::from_str(&text).map_err(|e| {
        format!(
            "scenario {scenario_id}: scenario.json is not a v1 manifest for {scenario_id} ({e})"
        )
    })?;
    if manifest.v != 1 || manifest.id != scenario_id || manifest.streams.is_empty() {
        return Err(format!(
            "scenario {scenario_id}: scenario.json is not a v1 manifest for {scenario_id}"
        ));
    }
    for st in &manifest.streams {
        if !dir.join(&st.lap).exists() {
            return Err(format!("scenario {scenario_id}: {} missing", st.lap));
        }
    }
    let gz = std::fs::File::open(dir.join(&manifest.telemetry.trace))
        .map_err(|e| format!("scenario {scenario_id}: trace: {e}"))?;
    let mut body = String::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_string(&mut body)
        .map_err(|e| format!("scenario {scenario_id}: trace: {e}"))?;
    let mut samples = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(o) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(t), Some(m)) = (o.get("t").and_then(Value::as_f64), o.get("metrics")) else {
            continue;
        };
        if !m.is_object() {
            continue;
        }
        samples.push(TraceSample {
            t,
            seq: o.get("seq").cloned().unwrap_or(Value::Null),
            raw_metrics: m.clone(),
            metrics: metrics_of(m),
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

/// `parkedMetrics`: same position, `gps.speed` zero (unit kept).
pub fn parked_metrics(m: &Metrics) -> Metrics {
    let mut out = m.clone();
    if let Some(gps) = out.get_mut("gps") {
        let unit = gps.get("speed").and_then(|l| l.unit.clone());
        let leaf = MetricLeaf {
            value: xorgate_core::telemetry::JsNumber(0.0),
            unit,
        };
        gps.insert("speed".into(), leaf);
    }
    out
}

/// `replayCaptureHead(lapPath, fps)` — the Phase 0 recipe, token for token.
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

/// `ReplayPlayer`: the lap/gap state machine over tokio's monotonic clock.
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
    pub fn set_scenario(&mut self, s: Arc<LoadedScenario>) {
        self.scenario = s;
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

/// What the replay source needs from the recording manager.
pub trait ReplayHost: Send + Sync {
    /// End every camera's lap cleanly now; respawn after `respawn_hold_ms`.
    fn cycle_video(&self);
    /// Close the telemetry session and open a new one (one pair per lap).
    fn rotate_telemetry_session(&self, why: &str);
}

/// `VIDEO_GRACE_MS`: grace before the self-timer assumes no capture child.
const VIDEO_GRACE_MS: f64 = 45_000.0;
const SETTINGS_POLL_TICKS: u64 = 5;

pub struct ReplayOptions {
    pub assets_dir: PathBuf,
    pub default_scenario_id: Option<String>,
    pub settings_path: PathBuf,
    pub media: Option<Arc<dyn MediaControl>>,
    pub state: StateHandle,
    pub tick_ms: u64,
}

struct St {
    scenario_id: String,
    player: ReplayPlayer,
    video_driven: bool,
    switch_pending: bool,
    ticks: u64,
    stopped: bool,
}

/// Deferred work, run after the state lock is released.
enum After {
    Rotate(String),
    Cycle,
}

pub struct ReplaySource {
    assets_dir: PathBuf,
    default_scenario_id: Option<String>,
    settings_path: PathBuf,
    catalog: ReplayCatalog,
    media: Option<Arc<dyn MediaControl>>,
    state: StateHandle,
    st: Mutex<St>,
    host: OnceLock<Arc<dyn ReplayHost>>,
}

fn now_ms() -> i64 {
    crate::now_ms()
}

impl ReplaySource {
    /// `createReplaySource(opts)`: loads the catalog and the starting scenario
    /// (an error = the TS throw at worker start → exit 1), then starts the
    /// 1 s self-timer.
    pub fn create(opts: ReplayOptions) -> Result<Arc<Self>, String> {
        let catalog = load_catalog(&opts.assets_dir)?;
        if catalog.scenarios.is_empty() {
            return Err("replay: the catalog has no scenarios".into());
        }
        let me = Self {
            assets_dir: opts.assets_dir,
            default_scenario_id: opts.default_scenario_id,
            settings_path: opts.settings_path,
            catalog,
            media: opts.media,
            state: opts.state,
            st: Mutex::new(St {
                scenario_id: String::new(),
                // placeholder replaced right below
                player: ReplayPlayer::new(Arc::new(LoadedScenario {
                    manifest: ScenarioManifest {
                        v: 1,
                        id: String::new(),
                        duration_ms: 0.0,
                        original_start_ts: 0.0,
                        original_end_ts: 0.0,
                        streams: Vec::new(),
                        telemetry: ScenarioTelemetry {
                            trace: String::new(),
                            rate_hz: 1.0,
                        },
                    },
                    dir: PathBuf::new(),
                    samples: Vec::new(),
                    lap_ms: 0.0,
                })),
                video_driven: false,
                switch_pending: false,
                ticks: 0,
                stopped: false,
            }),
            host: OnceLock::new(),
        };
        let id = me.resolve_scenario_id();
        let sc = Arc::new(load_scenario(&me.assets_dir, &id)?);
        me.log(
            ActivityLevel::Info,
            &format!(
                "scenario {id} loaded ({} samples, lap {} s)",
                sc.samples.len(),
                (sc.lap_ms / 1000.0).round() as i64
            ),
        );
        {
            let mut st = me.lock();
            st.scenario_id = id;
            st.player = ReplayPlayer::new(sc);
        }
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

    /// `replayEvent(level, msg)`: `[agent] replay: …` + a flushed state event.
    fn log(&self, level: ActivityLevel, msg: &str) {
        let msg = format!("replay: {msg}");
        match level {
            ActivityLevel::Warn => warn!("[agent] {msg}"),
            _ => info!("[agent] {msg}"),
        }
        self.state.note(level, msg, now_ms());
    }

    fn settings(&self) -> Option<xorgate_core::settings::SimulationSettings> {
        xorgate_hal::settings_io::load_settings(&self.settings_path).simulation
    }

    fn loop_(&self) -> bool {
        self.settings().and_then(|s| s.loop_).unwrap_or(true)
    }

    fn gap_ms(&self) -> f64 {
        f64::from(self.settings().and_then(|s| s.gap_seconds).unwrap_or(30)) * 1000.0
    }

    fn resolve_scenario_id(&self) -> String {
        let wanted = self
            .settings()
            .and_then(|s| s.scenario_id)
            .or_else(|| self.default_scenario_id.clone());
        if let Some(w) = &wanted {
            if self.catalog.scenarios.iter().any(|s| &s.id == w) {
                return w.clone();
            }
            self.log(
                ActivityLevel::Warn,
                &format!(
                    "scenario \"{w}\" is not in the catalog — using {}",
                    self.catalog.scenarios[0].id
                ),
            );
        }
        self.catalog.scenarios[0].id.clone()
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

    fn clock_stream(st: &St) -> String {
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
            .any(|s| s.stream_key == key)
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
        let msg = format!(
            "lap {idx} of {} started ({how})",
            st.player.current().manifest.id
        );
        after.push(After::Rotate(format!("replay lap {idx}")));
        self.log(ActivityLevel::Info, &msg);
    }

    fn finish_lap(&self, st: &mut St, how: &str) {
        let will_loop = self.loop_();
        st.player.end_lap(will_loop);
        let msg = if st.switch_pending {
            format!(
                "lap ended ({how}); switching to {}",
                st.player.current().manifest.id
            )
        } else if will_loop {
            format!(
                "lap ended ({how}); parked for {} s",
                (self.gap_ms() / 1000.0).round() as i64
            )
        } else {
            format!("lap ended ({how}); parked — loop is off")
        };
        self.log(ActivityLevel::Info, &msg);
    }

    fn run_after(&self, after: Vec<After>) {
        let Some(host) = self.host.get() else { return };
        for a in after {
            match a {
                After::Rotate(why) => host.rotate_telemetry_session(&why),
                After::Cycle => host.cycle_video(),
            }
        }
    }

    /// `reapply()`: re-read `simulation`; a changed scenario ends the lap in place.
    pub fn reapply(&self) {
        let mut after = Vec::new();
        {
            let mut st = self.lock();
            if st.stopped {
                return;
            }
            drop(st);
            let next = self.resolve_scenario_id();
            st = self.lock();
            if next == st.scenario_id {
                return;
            }
            let loaded = match load_scenario(&self.assets_dir, &next) {
                Ok(l) => Arc::new(l),
                Err(e) => {
                    let keep = st.scenario_id.clone();
                    drop(st);
                    self.log(
                        ActivityLevel::Warn,
                        &format!("scenario {next} failed to load ({e}); keeping {keep}"),
                    );
                    return;
                }
            };
            st.scenario_id = next.clone();
            st.player.set_scenario(loaded);
            st.switch_pending = true;
            self.log(
                ActivityLevel::Info,
                &format!("scenario switched to {next} — ending the current lap"),
            );
            if st.video_driven && self.host.get().is_some() {
                after.push(After::Cycle);
            } else {
                if st.player.phase() == ReplayPhase::Lap {
                    self.finish_lap(&mut st, "scenario switch");
                }
                self.begin_lap(&mut st, now_ms(), "scenario switch", &mut after);
            }
        }
        self.run_after(after);
    }

    /// One clock tick (the 1 s self-timer).
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
            self.reapply();
        }
        let mut after = Vec::new();
        {
            let mut st = self.lock();
            let phase = st.player.phase();
            if st.video_driven {
                if phase == ReplayPhase::Gap
                    && st.player.phase_age_ms() > self.gap_ms() + VIDEO_GRACE_MS
                {
                    self.log(
                        ActivityLevel::Warn,
                        "no capture child came back after the gap — running the lap clock without video",
                    );
                    st.video_driven = false;
                    self.begin_lap(&mut st, now_ms(), "timer fallback", &mut after);
                }
            } else if phase == ReplayPhase::Idle && st.player.phase_age_ms() > VIDEO_GRACE_MS {
                self.log(
                    ActivityLevel::Info,
                    "no capture child reported PLAYING — telemetry-only replay",
                );
                self.begin_lap(&mut st, now_ms(), "timer, no video", &mut after);
            } else if phase == ReplayPhase::Lap && st.player.lap_elapsed() {
                self.finish_lap(&mut st, "lap time elapsed");
            } else if phase == ReplayPhase::Gap && st.player.phase_age_ms() >= self.gap_ms() {
                self.begin_lap(&mut st, now_ms(), "timer", &mut after);
            }
        }
        self.run_after(after);
    }

    /// `status()` (diagnostics).
    pub fn status_line(&self) -> String {
        let st = self.lock();
        format!(
            "scenario={} phase={} lap={} videoDriven={}",
            st.scenario_id,
            st.player.phase().as_str(),
            st.player.lap_index(),
            st.video_driven
        )
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
            .find(|s| s.stream_key == cam.stream_key)
            .unwrap_or(&sc.manifest.streams[0]);
        replay_capture_head(&sc.dir.join(&stream.lap), stream.fps)
    }

    fn on_playing(&self, key: &str, wall_ms: i64) {
        let mut after = Vec::new();
        {
            let mut st = self.lock();
            if st.stopped || key != Self::clock_stream(&st) {
                return;
            }
            st.video_driven = true;
            self.begin_lap(&mut st, wall_ms, "video playing", &mut after);
        }
        self.run_after(after);
    }

    fn on_ended(&self, key: &str, clean: bool) {
        let mut st = self.lock();
        if st.stopped || key != Self::clock_stream(&st) {
            return;
        }
        if st.player.phase() != ReplayPhase::Lap {
            return;
        }
        self.finish_lap(&mut st, if clean { "video EOS" } else { "capture exited" });
    }

    fn respawn_hold_ms(&self, key: &str) -> Option<u64> {
        let st = self.lock();
        if key != Self::clock_stream(&st) || st.switch_pending {
            return Some(0);
        }
        drop(st);
        self.loop_().then(|| self.gap_ms() as u64)
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

/// Container exits requested by `reboot` / `power_off` in replay mode
/// (`REPLAY_EXIT_CODES`): the worker's main loop selects on this. SPIKE: a
/// process-wide channel; the real WP puts it on the ctx.
pub mod exit {
    use std::sync::OnceLock;
    use tokio::sync::mpsc;

    static TX: OnceLock<mpsc::UnboundedSender<(String, i32)>> = OnceLock::new();

    /// Install the channel (worker, replay mode only).
    pub fn install() -> mpsc::UnboundedReceiver<(String, i32)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = TX.set(tx);
        rx
    }

    /// Ask the worker to shut down with `code`; false when not installed.
    pub fn request(reason: &str, code: i32) -> bool {
        TX.get()
            .is_some_and(|tx| tx.send((reason.to_string(), code)).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t: f64, speed: f64) -> TraceSample {
        let raw = serde_json::json!({"gps": {"lat": {"value": 43.6}, "speed": {"value": speed, "unit": "km/h"}}});
        TraceSample {
            t,
            seq: Value::from(0),
            metrics: metrics_of(&raw),
            raw_metrics: raw,
        }
    }

    /// `sampleAt`: null before the first sample, nearest ≤ offset after.
    #[test]
    fn sample_at_is_nearest_at_or_before() {
        let s = vec![
            sample(-2328.0, 1.0),
            sample(1000.0, 2.0),
            sample(2000.0, 3.0),
        ];
        assert_eq!(sample_at(&s, -3000.0), None);
        assert_eq!(sample_at(&s, 0.0), Some(0));
        assert_eq!(sample_at(&s, 1000.0), Some(1));
        assert_eq!(sample_at(&s, 1999.0), Some(1));
        assert_eq!(sample_at(&s, 1e9), Some(2));
    }

    /// `parkedMetrics`: position kept, speed 0 with its unit.
    #[test]
    fn parked_metrics_zero_speed_keeps_unit() {
        let m = parked_metrics(&sample(0.0, 42.0).metrics);
        let sp = &m["gps"]["speed"];
        assert_eq!(sp.value.0, 0.0);
        assert_eq!(sp.unit.as_deref(), Some("km/h"));
        assert_eq!(m["gps"]["lat"].value.0, 43.6);
    }

    /// `replayCaptureHead`: the Phase 0 recipe, token for token.
    #[test]
    fn capture_head_tokens() {
        let h = replay_capture_head(Path::new("/a/s/cam0/lap.h264"), 15.0);
        assert_eq!(
            h.join(" "),
            "filesrc location=/a/s/cam0/lap.h264 ! h264parse ! video/x-h264,framerate=15/1 ! avdec_h264 ! identity sync=true ! videoconvert ! videorate"
        );
    }
}
