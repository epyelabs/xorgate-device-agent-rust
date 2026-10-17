//! Telemetry recorder — port of `src/recording/telemetry-recorder.ts`
//! (126 lines) + the I/O half of `SegmentWriter` (`segments.ts:86-192`) and
//! `finalizeOrphanSegments` (`segments.ts:224-270`).
//!
//! It samples the collectors' caches at `rateHz` and appends JSONL sample
//! lines to the session's segment writer. Fully decoupled from the live 1 Hz
//! publish loop AND from connectivity: it runs its own interval, so a device
//! that boots offline still records. Freshness + cadence gating is core's
//! [`RecorderGate`] (monotonic, Phase 8); naming/rotation/header decisions
//! are core's [`SegmentWriterCore`]; the fd I/O is
//! [`xorgate_hal::segment_io::SegmentExecutor`] under the S2 contract.
//!
//! Layout (under the recordings root):
//! `telemetry/<sessionId>/seg-<startTsMs>-<seq5>.jsonl[.gz]`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::{Map, Value};
use xorgate_core::clock::Clock;
use xorgate_core::recorder_gate::{tick_interval_ms, RecorderGate};
use xorgate_core::segments::{
    finalize_plan, orphan_entry_action, orphan_finalize_failed_error, HeaderTag, OrphanEntryAction,
    OrphanPlan, SampleLine, SegmentHeader, SegmentOp, SegmentWriterCore,
};
use xorgate_hal::gzip_finalize::{gzip_finalize, read_file_node};
use xorgate_hal::io::{node_fs_message, TapeSink};
use xorgate_hal::segment_io::{SegmentEvent, SegmentExecutor, SegmentIoError};
use xorgate_hal::tape::FsOp;

use crate::ctx::{BoxFut, GroupSample};
use crate::recording::time_source::TimeSourceHandle;

/// `sampleGroups()` (`TelemetryRecorderOptions.sampleGroups`).
pub type SampleGroupsFn =
    Arc<dyn Fn() -> BoxFut<'static, IndexMap<String, GroupSample>> + Send + Sync>;
/// `onSegmentClosed(gzPath)`.
pub type SegmentClosedFn = Arc<dyn Fn(&Path) + Send + Sync>;
/// Fired whenever the active plain segment changes (open / close).
pub type ActivePathFn = Arc<dyn Fn(Option<&Path>) + Send + Sync>;
/// `process.hrtime.bigint()` — the `mono` (ns) of a sample line.
pub type MonoNsFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// `TelemetryRecorderOptions` + the manager's `writer` block
/// (`manager.ts:742-776`, `telemetry-recorder.ts:35-45`).
pub struct TelemetryRecorderOpts {
    /// The recordings root (`XORGATE_HOME/recordings`); the session dir is
    /// `<root>/telemetry/<session_id>` (`manager.ts:745`).
    pub root: PathBuf,
    pub device_id: String,
    pub session_id: String,
    /// `readBootId()` (constant per process).
    pub boot_id: String,
    pub rate_hz: u32,
    pub time_source: TimeSourceHandle,
    pub sample_groups: SampleGroupsFn,
    /// `onSegmentClosed` — the manager's `void this.runEnforce()`.
    pub on_segment_closed: Option<SegmentClosedFn>,
    /// Not in the TS (the manager polls `activePath()` before each enforce);
    /// an optional push of the same value. Never required for parity.
    pub on_active_path: Option<ActivePathFn>,
    /// `Date.now()` names/stamps; `monotonicMs()` gates cadence + rotation age.
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub tape: Arc<dyn TapeSink>,
    /// `segmentMaxBytes` / `segmentMaxAgeMs` test overrides (`None` = 1 MB / 60 s).
    pub segment_max_bytes: Option<u64>,
    pub segment_max_age_ms: Option<u64>,
    /// `nativeCadenceHz` override (replaces the table).
    pub native_cadence_hz: Option<IndexMap<String, f64>>,
    /// `mono` source of sample lines; `None` = CLOCK_MONOTONIC ns.
    pub mono_ns: Option<MonoNsFn>,
    /// Test seam: a fault-injecting executor (`None` = the real one).
    pub executor: Option<SegmentExecutor>,
}

struct State {
    core: SegmentWriterCore,
    exec: SegmentExecutor,
    gate: RecorderGate<tokio::time::Instant>,
    seq: u64,
    last_active: Option<String>,
}

struct Inner {
    opts: TelemetryRecorderOpts,
    dir: String,
    state: Mutex<State>,
    ticking: AtomicBool,
    stopped: AtomicBool,
    timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// `class TelemetryRecorder` (`telemetry-recorder.ts:47-126`). Cheap to clone.
#[derive(Clone)]
pub struct TelemetryRecorder {
    inner: Arc<Inner>,
}

/// Clears `ticking` on every exit of `tick()` (its `finally`).
struct TickGuard<'a>(&'a AtomicBool);

impl Drop for TickGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The manager's `header()` factory (`manager.ts:756-765`): called only
    /// when a segment opens or a header is re-emitted, so `timeSource` and
    /// `startedAt` are current.
    fn header(&self) -> SegmentHeader {
        SegmentHeader {
            kind: HeaderTag::Header,
            v: 1,
            device_id: self.opts.device_id.clone(),
            session_id: self.opts.session_id.clone(),
            boot_id: self.opts.boot_id.clone(),
            time_source: self.opts.time_source.current(),
            rate_hz: self.opts.rate_hz as f64,
            started_at: self.opts.clock.wall_ms() as f64,
        }
    }

    fn mono_ns(&self) -> u64 {
        match &self.opts.mono_ns {
            Some(f) => f(),
            None => xorgate_hal::boot_id::monotonic_ns(),
        }
    }

    /// Execute one core call's ops; the events are collected for
    /// [`Inner::fire`], which runs after the state lock is released.
    fn run(
        &self,
        st: &mut State,
        ops: Vec<SegmentOp>,
        events: &mut Vec<SegmentEvent>,
    ) -> Result<(), SegmentIoError> {
        if ops.is_empty() {
            return Ok(());
        }
        let State { core, exec, .. } = st;
        exec.execute(core, &ops, events)
    }

    /// Tape `fs_out` + `onSegmentClosed` + the active-path push, in the
    /// order the ops ran (outside the state lock: the manager's enforce
    /// calls back into `active_path()`).
    fn fire(&self, events: Vec<SegmentEvent>, active: Option<Option<String>>) {
        let tape = &*self.opts.tape;
        let root = &self.opts.root;
        for ev in events {
            match &ev {
                SegmentEvent::Created { path } => {
                    tape.fs_out(&home_rel(root, path), FsOp::Create, 0)
                }
                SegmentEvent::Closed { path, size } => {
                    tape.fs_out(&home_rel(root, path), FsOp::Close, *size)
                }
                SegmentEvent::Deleted { path } => {
                    tape.fs_out(&home_rel(root, path), FsOp::Delete, 0)
                }
                SegmentEvent::Gzipped { gz, size } => {
                    tape.fs_out(&home_rel(root, gz), FsOp::Gz, *size);
                    if let Some(cb) = &self.opts.on_segment_closed {
                        cb(gz);
                    }
                }
            }
        }
        if let (Some(cb), Some(active)) = (&self.opts.on_active_path, active) {
            cb(active.as_deref().map(Path::new));
        }
    }

    /// The active path after a batch, when it changed (`None` = unchanged).
    fn active_change(st: &mut State) -> Option<Option<String>> {
        let now = st.core.active_path().map(str::to_string);
        if now != st.last_active {
            st.last_active = now.clone();
            Some(now)
        } else {
            None
        }
    }
}

impl TelemetryRecorder {
    /// `new TelemetryRecorder(opts)` (`telemetry-recorder.ts:60-65`).
    pub fn new(mut opts: TelemetryRecorderOpts) -> Self {
        let dir = opts
            .root
            .join("telemetry")
            .join(&opts.session_id)
            .to_string_lossy()
            .into_owned();
        let core = SegmentWriterCore::new(&dir, opts.segment_max_bytes, opts.segment_max_age_ms);
        let exec = opts.executor.take().unwrap_or_default();
        let gate = RecorderGate::new(opts.native_cadence_hz.clone());
        Self {
            inner: Arc::new(Inner {
                opts,
                dir,
                state: Mutex::new(State {
                    core,
                    exec,
                    gate,
                    seq: 0,
                    last_active: None,
                }),
                ticking: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                timer: Mutex::new(None),
            }),
        }
    }

    /// The session directory (`<root>/telemetry/<sessionId>`).
    pub fn dir(&self) -> &str {
        &self.inner.dir
    }

    /// `start()` (`telemetry-recorder.ts:67-72`): `setInterval(tick,
    /// Math.round(1000 / rateHz))` — first tick one period after start; each
    /// tick runs as its own task (`void this.tick()`), so a slow one is
    /// skipped over by the `ticking` guard rather than delaying the timer.
    pub fn start(&self) {
        let mut timer = self.inner.timer.lock().unwrap_or_else(|e| e.into_inner());
        if timer.is_some() || self.inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        let period = Duration::from_millis(tick_interval_ms(self.inner.opts.rate_hz as f64).max(1));
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        *timer = Some(tokio::spawn(async move {
            let mut iv = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                iv.tick().await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let rec = TelemetryRecorder { inner };
                tokio::spawn(async move { rec.tick().await });
            }
        }));
    }

    /// `stop()` (`telemetry-recorder.ts:75-82`): stop ticking and finalize
    /// the active segment, synchronously (safe from shutdown handlers). The
    /// TS lets a finalize error throw out of `stop()`; it is returned here.
    pub fn stop(&self) -> Result<(), SegmentIoError> {
        self.inner.stopped.store(true, Ordering::SeqCst);
        if let Some(t) = self
            .inner
            .timer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            t.abort();
        }
        let mut events = Vec::new();
        let (res, active) = {
            let mut st = self.inner.lock();
            let ops = st.core.finalize();
            let res = self.inner.run(&mut st, ops, &mut events);
            (res, Inner::active_change(&mut st))
        };
        self.inner.fire(events, active);
        res
    }

    /// `activePath()` (`telemetry-recorder.ts:85-87`): the active plain
    /// segment (disk-manager protection), or `None`.
    pub fn active_path(&self) -> Option<PathBuf> {
        self.inner.lock().core.active_path().map(PathBuf::from)
    }

    /// One sampling tick (`telemetry-recorder.ts:90-125`); also drives
    /// time-based rotation. Never fails out: an error is logged as
    /// `[recording] tick failed: <message>` and the rest of the tick skipped.
    pub async fn tick(&self) {
        let inner = &self.inner;
        // a slow tick must not overlap the next
        if inner.stopped.load(Ordering::SeqCst)
            || inner
                .ticking
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        let _guard = TickGuard(&inner.ticking);
        let groups = (inner.opts.sample_groups)().await;
        if inner.stopped.load(Ordering::SeqCst) {
            return; // stop() ran during the await — segment is finalized
        }
        let mut events = Vec::new();
        let (res, active) = {
            let mut st = inner.lock();
            // stop() may have finalized between the check above and the lock.
            if inner.stopped.load(Ordering::SeqCst) {
                return;
            }
            let res = self.tick_locked(&mut st, groups, &mut events);
            (res, Inner::active_change(&mut st))
        };
        inner.fire(events, active);
        if let Err(e) = res {
            tracing::error!("[recording] tick failed: {}", e.message);
        }
    }

    fn tick_locked(
        &self,
        st: &mut State,
        groups: IndexMap<String, GroupSample>,
        events: &mut Vec<SegmentEvent>,
    ) -> Result<(), SegmentIoError> {
        let inner = &self.inner;
        let rate_hz = inner.opts.rate_hz as f64;
        let now = inner.opts.clock.wall_ms();
        let now_mono = inner.opts.clock.mono_ms();
        let mut metrics: Map<String, Value> = Map::new();
        for (group, s) in groups {
            if !st.gate.admit(&group, s.updated_mono, now_mono, rate_hz) {
                continue;
            }
            // CF42: built from the typed `JsNumber` leaves, printed by JsJson.
            let fields = serde_json::to_value(&s.fields).unwrap_or(Value::Null);
            metrics.insert(group, fields);
        }

        let ts = inner.opts.time_source.current();
        let header = || inner.header();
        let ops = st.core.note_time_source(ts, &header);
        inner.run(st, ops, events)?;
        if !metrics.is_empty() {
            let seq = st.seq;
            st.seq += 1; // `this.seq++` is evaluated before writeLine runs
            let line = SampleLine {
                ts: now as f64,
                mono: inner.mono_ns() as f64,
                seq: seq as f64,
                metrics,
            };
            let mono = inner.opts.clock.mono_ms();
            let ops = st.core.write_line(&line, now, mono, &header);
            inner.run(st, ops, events)?;
        }
        let mono = inner.opts.clock.mono_ms();
        let ops = st.core.maybe_rotate(mono);
        inner.run(st, ops, events)
    }
}

/// `finalizeOrphanSegments(telemetryRoot)` (`segments.ts:224-270`): startup
/// crash recovery — gzip any plain `.jsonl` a previous run left behind
/// (torn last line truncated away; a segment without a header AND one
/// sample deleted) and remove leftover gzip temps. Returns the finalized
/// count. Safe before the new session writes (session dirs are per-run).
/// Directories are walked in libuv's `scandir` (byte) order.
pub fn finalize_orphan_segments(telemetry_root: &Path) -> usize {
    finalize_orphan_segments_taped(telemetry_root, &xorgate_hal::io::NullTape)
}

/// [`finalize_orphan_segments`] with the tape hook.
///
/// `fs_out` rel paths are relative to the agent home, i.e. the parent of the
/// recordings root (`telemetry_root/../..`).
pub fn finalize_orphan_segments_taped(telemetry_root: &Path, tape: &dyn TapeSink) -> usize {
    let recordings_root = telemetry_root.parent().unwrap_or(telemetry_root);
    let mut finalized = 0;
    let Some(sessions) = sorted_names(telemetry_root) else {
        return 0; // no tree yet
    };
    for session in sessions {
        let dir = telemetry_root.join(&session);
        // statSync(dir).isDirectory() (follows symlinks), then readdirSync.
        if !std::fs::metadata(&dir).map(|m| m.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(entries) = sorted_names(&dir) else {
            continue;
        };
        for name in entries {
            let path = dir.join(&name);
            match orphan_entry_action(&name) {
                OrphanEntryAction::DeleteTemp => {
                    // best effort
                    if std::fs::remove_file(&path).is_ok() {
                        tape.fs_out(&home_rel(recordings_root, &path), FsOp::Delete, 0);
                    }
                }
                OrphanEntryAction::Skip => {}
                OrphanEntryAction::Finalize => match finalize_one(&path) {
                    Ok(Some(gz)) => {
                        finalized += 1;
                        let size = std::fs::metadata(&gz).map(|m| m.len()).unwrap_or(0);
                        tape.fs_out(&home_rel(recordings_root, &gz), FsOp::Gz, size);
                    }
                    Ok(None) => {
                        tape.fs_out(&home_rel(recordings_root, &path), FsOp::Delete, 0);
                    }
                    Err(msg) => tracing::error!(
                        "{}",
                        orphan_finalize_failed_error(&path.to_string_lossy(), &msg)
                    ),
                },
            }
        }
    }
    finalized
}

/// One orphan `.jsonl` (`segments.ts:252-266`): `Ok(Some(gz))` = finalized,
/// `Ok(None)` = deleted (nothing worth keeping), `Err` = Node's message.
fn finalize_one(path: &Path) -> Result<Option<PathBuf>, String> {
    let raw = read_file_node(path, xorgate_core::segments::GzipFinalizeStep::ReadPlain)
        .map_err(|e| e.message)?;
    match finalize_plan(&raw) {
        OrphanPlan::Delete => {
            std::fs::remove_file(path).map_err(|e| node_fs_message(&e, "unlink", path))?;
            Ok(None)
        }
        OrphanPlan::Finalize { keep_len } => {
            let gz = gzip_finalize(path, Some(&raw[..keep_len]), None).map_err(|e| e.message)?;
            Ok(Some(gz))
        }
    }
}

/// `readdirSync(dir)` names in libuv order (strcmp), `None` when unreadable.
fn sorted_names(dir: &Path) -> Option<Vec<String>> {
    use std::os::unix::ffi::OsStrExt;
    let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .collect();
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Some(
        names
            .into_iter()
            .map(|n| n.to_string_lossy().into_owned())
            .collect(),
    )
}

/// The tape's `fs_out.rel` for `path`: relative to the agent home (the
/// parent of the recordings root, `XORGATE_HOME`), e.g.
/// `recordings/telemetry/<sid>/seg-…jsonl.gz`. A path outside the home is
/// recorded as given.
pub fn home_rel(recordings_root: &Path, path: &Path) -> String {
    let home = recordings_root.parent().unwrap_or(recordings_root);
    path.strip_prefix(home)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}
