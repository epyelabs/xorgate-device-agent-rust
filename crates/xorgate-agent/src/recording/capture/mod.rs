//! The video capture daemon — port of `VideoCaptureSupervisor`
//! (`src/recording/video-capture.ts:395-1021` @ 89b5c64): one supervised
//! `gst-launch-1.0` child per camera that OWNS the camera. Record mode tees
//! raw frames into x264 → splitmuxsink ~60 s fragments AND a low-res shmsink
//! bridge the KVS master reads; bridge-only mode (recording off) feeds just
//! the bridge. Every argv comes from the core builders
//! (`xorgate_core::video_capture`, byte-identical to the TS; `videorate`
//! before `videoscale` kept).
//!
//! Supervision (decisions in `xorgate_core::capture_ladder`): crash → backoff
//! 2→30 s; 3 quick deaths (<15 s) → park with a 60 s slow retry; a child that
//! wrote no bytes to its active fragment for 45 s is SIGKILLed (the 1080p30
//! stall, memory `1080p30-stall-bug`), stalls counted apart from quick
//! failures, 3 in a row → park, and `recording:true` re-earned by bytes; stop
//! = SIGINT (`-e` ⇒ EOS ⇒ the last fragment finalises). After EVERY exit the
//! dead child's own `/dev/shm/shmpipe.<pid>.*` are reaped (shmsink unlinks
//! them only in its own cleanup, which SIGKILL skips).
//!
//! Sidecars: fs.watch (inotify, one sweep per event like Node) + a 10 s poll
//! sweep (authoritative) run the core `plan_sweep`; the active fragment is
//! fsynced on every sweep; 0-byte fragments are dropped only by the final
//! sweep at child exit. `startTs` is the fragment's birth time, so the
//! ~2.1 s splitmuxsink GOP lag is REPRODUCED (memory
//! `video-sidecar-gop-latency`; Phase 7 fixes it).
//!
//! Runtime model (deliberate, no observable change): the TS runs on one
//! event loop; here the state is one mutex, timers are tokio tasks guarded
//! by a generation number (a timer that lost a race with its cancellation is
//! a no-op), and every callback (`on_event`, `on_status`, `on_spawned`,
//! `on_active_path`, `on_segment_closed`) is collected while the lock is held
//! and run AFTER it is released, in the TS's order, so a callback may call
//! back into the supervisor. Signals are delivered after those callbacks:
//! in the TS a child's exit can only be observed after the synchronous block
//! that signalled it has returned, and deferring the delivery keeps that
//! ordering (a `stalled: no bytes written…` event always precedes its
//! `stalled; retry 1/3…`).
//!
//! Tape (phase-3 §Record/replay, `role:"capture"`): `child_spawn` right
//! after spawn (program + argv, pid, nice), `child_line` for EVERY drained
//! line (the writer keeps `New clock:`/`Got EOS`/`ERROR`), `child_signal` for
//! every agent-sent signal to a still-live child, `child_exit` (`external` =
//! not agent-signalled) followed by `shm` (the `/dev/shm/shmpipe.*` names
//! left after the reap), `shm` every 60 s while a tape is open, and `fs_out`
//! per fragment: `create` when a fragment first becomes the active one,
//! `close` + `sidecar` per sidecar written, `delete` per 0-byte fragment
//! dropped. Paths are relative to `home`. No-ops without a tape.
//!
//! Replay mode (Phase 7e): the TS replay hooks `sourceHead`,
//! `onPlaying`, `onEnded`, `eosIsClean`, `respawnHoldMs` (`:314-340`,
//! `:768-787` @ 09da38d) are [`ReplayCaptureHooks`] (`eosIsClean` is
//! [`ReplayCaptureHooks::eos_is_clean`]). D72: PLAYING/EOS lines are keyed
//! to the CURRENT child (a dead child's late stdout cannot touch its
//! successor's run; unobservable for every TS-reachable input). Without hooks (every real device) every path is
//! byte-for-byte the pre-7e behaviour: `cycle()` finalises the last fragment
//! and re-enters the normal ladder (the TS without `respawnHoldMs`).

pub mod backend;
pub mod recover;
pub mod sweep;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tracing::error;
use xorgate_core::capture_ladder::{
    camera_disabled_event, exit_verdict, is_recording, progress_verdict, spawn_event,
    stagger_delay_ms, stall_event, stderr_log_line, CaptureState, ExitInput, LadderCounters,
    Progress, ProgressVerdict, BASE_BACKOFF_MS, SHM_SEGMENT_DIR, STABLE_MS, STALL_MS,
    SWEEP_POLL_MS,
};
use xorgate_core::clock::Clock;
use xorgate_core::hardware_block::EncoderToken;
use xorgate_core::session::mint_session_id;
use xorgate_core::sidecar::{ClockPair, SessionManifest, VideoSessionManifest};
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::types::TimeSource;
use xorgate_core::video_capture::{
    build_bridge_only_pipeline_args, build_capture_pipeline_args, CapturePipelineSpec,
};
use xorgate_core::video_presets::VideoPreset;
use xorgate_hal::child::{ChildDone, ChildEvent};
use xorgate_hal::fragment_stat::{hrtime_ns, monotonic_ms, size_or_zero};
use xorgate_hal::io::{null_tape, ChildExitRec, ChildSpawnRec, FsOp, TapeSink, ROLE_CAPTURE};
use xorgate_hal::{fs_watch, shm_reaper};

pub use backend::{CaptureBackend, CaptureChild, GstLaunchBackend};
pub use recover::{recover_orphan_video_sessions, RecoverOpts, RecoverResult};
pub use sweep::{sweep_session_dir, write_session_manifest, SweepIo, SweepOutcome};
pub use xorgate_core::video_capture::CaptureCamera;

/// Period of the tape's `shm` audit record while a tape is open
/// (phase-3 §Record/replay: "every 60 s + at child exit"). Tape-only.
pub const SHM_TAPE_PERIOD_MS: u64 = 60_000;

/// The env seam `captureSourceHead` reads on every spawn (`video-capture.ts:130`).
pub const CAPTURE_SOURCE_HEAD_ENV: &str = "XORGATE_CAPTURE_SOURCE_HEAD";

/// `onEvent(level, msg)`.
pub type EventFn = Arc<dyn Fn(ActivityLevel, &str) + Send + Sync>;
/// `onSpawned(streamKey)`.
pub type KeyFn = Arc<dyn Fn(&str) + Send + Sync>;
/// `onActiveSegment(streamKey, path | null)`.
pub type ActivePathFn = Arc<dyn Fn(&str, Option<&Path>) + Send + Sync>;
/// `onSegmentClosed(mp4Path)`.
pub type PathFn = Arc<dyn Fn(&Path) + Send + Sync>;
/// `onStatus()`.
pub type StatusFn = Arc<dyn Fn() + Send + Sync>;

/// gst-launch's stdout line at PLAYING (`GST_PLAYING_RE`, `video-capture.ts:142`).
pub const GST_PLAYING_MARK: &str = "New clock:";
/// gst-launch's stdout line at a natural EOS (`GST_EOS_RE`, `video-capture.ts:143`).
pub const GST_EOS_MARK: &str = "Got EOS from element";

/// The replay-mode capture hooks (`VideoCaptureOptions` `:314-340` @ 09da38d).
/// `None` on [`VideoCaptureOpts::replay`] = a real device: none of the TS
/// hooks is set and every path is the pre-7e behaviour.
///
/// Locking: `source_head` and `respawn_hold_ms` are called WITH the
/// supervisor lock held, so they must not call back into the supervisor
/// (the replay source only takes its own lock and reads settings).
/// `on_playing` and `on_ended` are called with no supervisor lock held,
/// `on_ended` after the child's final sweep (its last sidecar is written).
pub trait ReplayCaptureHooks: Send + Sync {
    /// `sourceHead(cam)`: the pipeline HEAD, re-evaluated on EVERY spawn.
    fn source_head(&self, cam: &CaptureCamera) -> Vec<String>;
    /// `onPlaying(streamKey, wallMs)`: gst-launch printed `New clock:`
    /// (once per child).
    fn on_playing(&self, key: &str, wall_ms: i64);
    /// `onEnded(streamKey, clean)`: a child exited and its session was swept.
    fn on_ended(&self, key: &str, clean: bool);
    /// `respawnHoldMs(streamKey)`: the gap before the next lap; `None` = park.
    fn respawn_hold_ms(&self, key: &str) -> Option<u64>;
    /// `eosIsClean`: a child that printed `Got EOS from element` before
    /// exiting is a CLEAN end (the replay source: always).
    fn eos_is_clean(&self) -> bool {
        true
    }
}
/// `timeSource()` — the tracker's CURRENT value, synchronously (the TS
/// passes `() => this.timeSource.current()`, `manager.ts:717`).
pub type TimeSourceFn = Arc<dyn Fn() -> TimeSource + Send + Sync>;

/// `VideoCaptureOptions` (`video-capture.ts:274-349`), real-mode fields.
pub struct VideoCaptureOpts {
    pub device_id: String,
    pub cameras: Vec<CaptureCamera>,
    /// False = bridge-only (no session, no disk, never `recording`).
    pub record: bool,
    pub preset: VideoPreset,
    pub segment_seconds: u64,
    /// Recording encoder token; `None` = sw-h264 (today's argv).
    pub encoder: Option<EncoderToken>,
    /// Inter-camera spawn delay: camera N starts `N * stagger_ms` after
    /// `start()` (every (re)start, not just boot).
    pub stagger_ms: u64,
    /// `recordings/video` root: sessions at `<root>/<streamKey>/<sessionId>/`.
    pub video_root: PathBuf,
    /// Shm socket dir (created; sockets are `<dir>/<streamKey>.sock`).
    pub shm_dir: PathBuf,
    /// The agent home, for the tape's home-relative `fs_out` paths.
    pub home: Option<PathBuf>,
    /// `readBootId()` (per-process cache; WP2's `read_boot_id()`).
    pub boot_id: String,
    pub time_source: TimeSourceFn,
    pub backend: Arc<dyn CaptureBackend>,
    pub on_segment_closed: Option<PathFn>,
    pub on_active_path: Option<ActivePathFn>,
    pub on_status: Option<StatusFn>,
    pub on_event: Option<EventFn>,
    pub on_spawned: Option<KeyFn>,
    /// Wall clock for naming/stamping; its mono half measures every interval
    /// (rotation-free stall window, stable window, P8).
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub tape: Arc<dyn TapeSink>,
    /// Test seams (`stableMs`, `sweepPollMs`, `stallMs`, `shmSegmentDir`).
    pub stable_ms: u64,
    pub sweep_poll_ms: u64,
    pub stall_ms: u64,
    pub shm_segment_dir: PathBuf,
    /// Arm the inotify watcher (the poll sweep runs regardless). Tests on a
    /// paused clock turn it off: a real thread's events would race virtual time.
    pub fs_watch: bool,
    /// Replay mode only (Phase 7e); `None` = real mode, unchanged behaviour.
    pub replay: Option<Arc<dyn ReplayCaptureHooks>>,
}

impl VideoCaptureOpts {
    /// The required options; everything else at the TS defaults
    /// (`gst-launch-1.0` backend, no callbacks, no tape, 15 s / 10 s / 45 s,
    /// `/dev/shm`, fs.watch on).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_id: impl Into<String>,
        cameras: Vec<CaptureCamera>,
        record: bool,
        preset: VideoPreset,
        segment_seconds: u64,
        video_root: PathBuf,
        shm_dir: PathBuf,
        boot_id: impl Into<String>,
        time_source: TimeSourceFn,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Self {
        Self {
            device_id: device_id.into(),
            cameras,
            record,
            preset,
            segment_seconds,
            encoder: None,
            stagger_ms: 0,
            video_root,
            shm_dir,
            home: None,
            boot_id: boot_id.into(),
            time_source,
            backend: Arc::new(GstLaunchBackend::default()),
            on_segment_closed: None,
            on_active_path: None,
            on_status: None,
            on_event: None,
            on_spawned: None,
            clock,
            tape: null_tape(),
            stable_ms: STABLE_MS,
            sweep_poll_ms: SWEEP_POLL_MS,
            stall_ms: STALL_MS,
            shm_segment_dir: PathBuf::from(SHM_SEGMENT_DIR),
            fs_watch: true,
            replay: None,
        }
    }
}

/// `CaptureStatus` (`video-capture.ts:266-272`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatus {
    pub stream_key: String,
    pub state: CaptureState,
    pub session_id: Option<String>,
    /// True only in record mode while the child is up — NOT in bridge-only mode.
    pub recording: bool,
}

/// A pending timer (its task + the generation that makes a late fire a no-op).
struct Timer {
    gen: u64,
    abort: AbortHandle,
}

fn cancel(t: &mut Option<Timer>) {
    if let Some(t) = t.take() {
        t.abort.abort();
    }
}

#[derive(Clone, Copy)]
enum Slot {
    Stable,
    Restart,
}

/// The live child of a run (`run.handle`).
struct ChildSlot {
    id: u64,
    child: Arc<dyn CaptureChild>,
    pid: Option<u32>,
    /// Flips to `true` once `onExit` has run (the TS `handle.done`, whose
    /// `onExit` continuation was registered first).
    exited: watch::Receiver<bool>,
    /// The agent signalled it (its exit is not `external` on the tape).
    signalled: bool,
}

/// `CameraRun` (`video-capture.ts:351-387`), real-mode fields.
struct Run {
    id: u64,
    cam: CaptureCamera,
    state: CaptureState,
    retiring: bool,
    child: Option<ChildSlot>,
    session_id: Option<String>,
    session_dir: Option<PathBuf>,
    manifest: Option<SessionManifest>,
    /// Monotonic ms at spawn.
    started_at: u64,
    counters: LadderCounters,
    stable_timer: Option<Timer>,
    restart_timer: Option<Timer>,
    /// fs.watch + its pump task; `watch_gen` guards both sweep sources.
    watcher: Option<(fs_watch::DirWatcher, AbortHandle)>,
    sweep_timer: Option<Timer>,
    watch_gen: Option<u64>,
    progress: Progress,
    stalled: bool,
    cycling: bool,
    /// Replay: the child printed `Got EOS from element` (`run.eosSeen`).
    eos_seen: bool,
    /// Replay: `onPlaying` already fired for this child (`run.playingReported`).
    playing_reported: bool,
    /// The active fragment last reported to the tape (`fs_out create` once
    /// per fragment). Tape bookkeeping only.
    tape_active: Option<PathBuf>,
}

impl Run {
    fn new(id: u64, cam: CaptureCamera) -> Self {
        Self {
            id,
            cam,
            state: CaptureState::Starting,
            retiring: false,
            child: None,
            session_id: None,
            session_dir: None,
            manifest: None,
            started_at: 0,
            counters: LadderCounters::default(),
            stable_timer: None,
            restart_timer: None,
            watcher: None,
            sweep_timer: None,
            watch_gen: None,
            progress: Progress {
                path: None,
                bytes: 0,
                at_ms: 0,
            },
            stalled: false,
            cycling: false,
            eos_seen: false,
            playing_reported: false,
            tape_active: None,
        }
    }

    fn stop_watching(&mut self) {
        if let Some((_w, pump)) = self.watcher.take() {
            pump.abort(); // dropping `_w` ends the inotify thread
        }
        cancel(&mut self.sweep_timer);
        self.watch_gen = None;
    }
}

struct Inner {
    /// Insertion-ordered like the TS `Map` (`status()` order).
    runs: IndexMap<String, Run>,
    stopped: bool,
    counter: u64,
    /// D41 agent-exit cleanup under way (review-wp5-r2 R-WP5R-1): `onExit`
    /// then ONLY reaps the dead child's shm segments — no final sweep, no
    /// sidecar, no `onSegmentClosed`/enforcer, no tape close records, no
    /// event — exactly what the runtime drop's `kill_on_drop` left before
    /// (and the TS process exit leaves): the fragments wait for the next
    /// boot's recovery, which stamps the newest `finalized:false`.
    exiting: bool,
    /// The segments `onExit` reaped while `exiting`, per stream key (the
    /// exit cleanup's count includes them, R-WP5R-5).
    exit_reaped: Vec<(String, Vec<String>)>,
}

fn next(counter: &mut u64) -> u64 {
    *counter += 1;
    *counter
}

/// A deferred callback (run after the lock is released, in order).
enum Fx {
    Event(ActivityLevel, String),
    Status,
    Spawned(String),
    Active(String, Option<PathBuf>),
    Closed(PathBuf),
    /// Replay `onEnded(streamKey, clean)`.
    Ended(String, bool),
}

/// A deferred signal (delivered after the callbacks).
/// One capture child the agent-exit cleanup SIGKILLed ([`VideoCaptureSupervisor::exit_kill`]).
#[derive(Debug, Clone)]
pub struct ExitKilled {
    pub key: String,
    pub pid: Option<u32>,
    /// Flips to true once its `onExit` has run.
    pub exited: watch::Receiver<bool>,
}

struct Sig {
    child: Arc<dyn CaptureChild>,
    key: String,
    pid: Option<u32>,
    sig: i32,
    /// The child's `onExit` flag: a signal to an exited child is not sent
    /// (execa `kill` on a reaped child), so it is not taped either.
    exited: watch::Receiver<bool>,
}

#[derive(Default)]
struct Out {
    fx: Vec<Fx>,
    sigs: Vec<Sig>,
}

struct Shared {
    opts: VideoCaptureOpts,
    inner: Mutex<Inner>,
}

/// Supervises one capture child per camera (`VideoCaptureSupervisor`).
/// Cheap to clone; every clone is the same supervisor.
#[derive(Clone)]
pub struct VideoCaptureSupervisor {
    shared: Arc<Shared>,
}

impl VideoCaptureSupervisor {
    pub fn new(opts: VideoCaptureOpts) -> Self {
        Self {
            shared: Arc::new(Shared {
                opts,
                inner: Mutex::new(Inner {
                    runs: IndexMap::new(),
                    stopped: false,
                    counter: 0,
                    exiting: false,
                    exit_reaped: Vec::new(),
                }),
            }),
        }
    }

    /// `start()` (`video-capture.ts:404-407`): create the shm dir, launch
    /// every camera (staggered). Must be called inside a tokio runtime.
    ///
    /// Accepted divergence D27: a failed shm-dir `mkdir` is ignored (the
    /// TS `mkdirSync` at `:405` throws out of the manager's `videoTransition`
    /// chain → unhandledRejection + state.json error, and every later video
    /// transition is skipped until restart). The child then fails to bind its
    /// socket and the ladder retries / parks.
    pub fn start(&self) {
        let s = &self.shared;
        let _ = std::fs::create_dir_all(&s.opts.shm_dir); // D27
        let mut out = Out::default();
        {
            let mut g = s.lock();
            let cams = s.opts.cameras.clone();
            launch(s, &mut g, &cams, &mut out);
        }
        s.flush(out);
        if s.opts.tape.enabled() {
            tape_shm_audit(s);
        }
    }

    /// `setCameras(cameras)` (`video-capture.ts:418-444`): bring the running
    /// set to exactly `cameras` WITHOUT disturbing the cameras that stay. A
    /// removed camera is SIGINT-ed (EOS → final sweep) and forgotten; an
    /// added one spawns fresh. Resolves once every removed child has exited.
    pub async fn set_cameras(&self, cameras: &[CaptureCamera]) {
        let s = &self.shared;
        let mut out = Out::default();
        let mut retired = Vec::new();
        {
            let mut g = s.lock();
            if g.stopped {
                return;
            }
            // D27 (see `start`): the TS `mkdirSync` at `:420` throws here.
            let _ = std::fs::create_dir_all(&s.opts.shm_dir);
            let wanted: HashSet<&str> = cameras.iter().map(|c| c.stream_key.as_str()).collect();
            let keys: Vec<String> = g.runs.keys().cloned().collect();
            for key in keys {
                if wanted.contains(key.as_str()) {
                    continue;
                }
                let run = g.runs.get_mut(&key).expect("listed");
                run.retiring = true;
                cancel(&mut run.stable_timer);
                cancel(&mut run.restart_timer);
                run.stop_watching();
                if let Some(slot) = run.child.as_mut() {
                    // onExit finalises + sweeps, then drops the run from the map.
                    retired.push(slot.exited.clone());
                    slot.signalled = true;
                    out.sigs.push(Sig {
                        child: slot.child.clone(),
                        key: key.clone(),
                        pid: slot.pid,
                        sig: libc::SIGINT,
                        exited: slot.exited.clone(),
                    });
                } else {
                    s.final_sweep(&key, run, true, &mut out);
                    g.runs.shift_remove(&key);
                }
                out.fx
                    .push(Fx::Event(ActivityLevel::Info, camera_disabled_event(&key)));
            }
            let added: Vec<CaptureCamera> = cameras
                .iter()
                .filter(|c| !g.runs.contains_key(&c.stream_key))
                .cloned()
                .collect();
            launch(s, &mut g, &added, &mut out);
            out.fx.push(Fx::Status);
        }
        s.flush(out);
        for rx in retired {
            wait_exited(rx).await;
        }
    }

    /// `stop()` (`video-capture.ts:493-507`): SIGINT every child (EOS → clean
    /// fragment finalize), close watchers; the final sweep happens at exit.
    pub fn stop(&self) {
        let s = &self.shared;
        let mut out = Out::default();
        {
            let mut g = s.lock();
            let _ = stop_locked(&mut g, &mut out);
        }
        s.flush(out);
    }

    /// Agent-exit cleanup, phase 1 (D41, orchestrator rulings 2026-11-06;
    /// Rust only — the TS process exit leaves execa's `cleanup` to kill the
    /// child and reaps nothing). Marks the supervisor EXITING (its `onExit`
    /// then only reaps shm, see [`Inner::exiting`]), sends the shutdown
    /// SIGINTs if `stop()` has not, and SIGKILLs every capture child still
    /// alive (the SIGINT's EOS outlived the agent). Returns those children;
    /// the caller waits for ALL of them with ONE total bound (R-WP5R-6), then
    /// calls [`Self::exit_finish`]. No wait for the EOS is added: the caller
    /// runs this where the runtime drop's `kill_on_drop` used to land.
    pub fn exit_kill(&self) -> Vec<ExitKilled> {
        let s = &self.shared;
        let handles = {
            let mut g = s.lock();
            let mut out = Out::default();
            let h = stop_locked(&mut g, &mut out);
            g.exiting = true;
            drop(g);
            s.flush(out);
            h
        };
        let mut killed = Vec::new();
        for (sig, rx) in handles {
            if *rx.borrow() {
                continue; // exited already (its normal `onExit` ran)
            }
            s.deliver(&sig); // SIGKILL (taped like every other signal)
            killed.push(ExitKilled {
                key: sig.key.clone(),
                pid: sig.pid,
                exited: rx,
            });
        }
        killed
    }

    /// Agent-exit cleanup, phase 2: unlink every shmsink segment each killed
    /// child's pid owns (`shm_reaper::reap_shm_segments`, the same
    /// `shm_owner_pid` matcher — also when its exit was not observed within
    /// the bound) and report, per child, every segment unlinked during the
    /// cleanup, its `onExit`'s included (R-WP5R-5).
    pub fn exit_finish(&self, killed: &[ExitKilled]) -> Vec<(String, Vec<String>)> {
        let s = &self.shared;
        let mut by_onexit = std::mem::take(&mut s.lock().exit_reaped);
        killed
            .iter()
            .map(|k| {
                let mut segs: Vec<String> = Vec::new();
                by_onexit.retain(|(key, names)| {
                    if key == &k.key {
                        segs.extend(names.iter().cloned());
                        false
                    } else {
                        true
                    }
                });
                segs.extend(shm_reaper::reap_shm_segments(
                    &s.opts.shm_segment_dir,
                    k.pid,
                ));
                (k.key.clone(), segs)
            })
            .collect()
    }

    /// Both phases for ONE supervisor: [`Self::exit_kill`], wait ≤ `bound`
    /// in total for the killed children, [`Self::exit_finish`].
    pub async fn kill_and_reap_at_exit(&self, bound: Duration) -> Vec<(String, Vec<String>)> {
        let killed = self.exit_kill();
        let _ = tokio::time::timeout(bound, async {
            for k in &killed {
                wait_exited(k.exited.clone()).await;
            }
        })
        .await;
        self.exit_finish(&killed)
    }

    /// `stopAndWait(graceMs = 10_000)` (`video-capture.ts:516-531`): stop and
    /// WAIT for every child to exit, escalating to SIGKILL after `grace`.
    /// Required before a replacement supervisor spawns: an exiting shmsink
    /// unlinks its socket path, so an old child overlapping a new one would
    /// delete the NEW socket (hardware-verified).
    ///
    /// Invariant 8 (review-wp4 S1): the child snapshot, `stopped = true` and
    /// the SIGINTs are ONE critical section ([`stop_locked`]). The TS has no
    /// gap (one synchronous block); with two lock acquisitions a restart or
    /// stagger timer on another worker could spawn a child between them that
    /// this call SIGINTs but never awaits. After the section every spawn path
    /// refuses (`spawn_locked` checks `stopped`; the timers are cancelled).
    pub async fn stop_and_wait(&self, grace: Duration) {
        let s = &self.shared;
        let mut out = Out::default();
        let handles = {
            let mut g = s.lock();
            stop_locked(&mut g, &mut out)
        };
        #[cfg(test)]
        tests::run_stop_gap_hook(s);
        s.flush(out);
        if handles.is_empty() {
            return;
        }
        let all = async {
            for (_, rx) in &handles {
                wait_exited(rx.clone()).await;
            }
        };
        if tokio::time::timeout(grace, all).await.is_err() {
            for (sig, _) in &handles {
                s.deliver(sig);
            }
            for (_, rx) in &handles {
                wait_exited(rx.clone()).await;
            }
        }
    }

    /// `activeSessionIds()` (`video-capture.ts:534-538`): orphan-recovery exclusion.
    pub fn active_session_ids(&self) -> HashSet<String> {
        self.shared
            .lock()
            .runs
            .values()
            .filter_map(|r| r.session_id.clone())
            .collect()
    }

    /// `cycle(streamKey)` (`video-capture.ts:547-560`): end the current child
    /// cleanly (SIGINT, last fragment finalised); a camera without a child
    /// spawns now. Real mode: the respawn then follows the normal ladder.
    pub fn cycle(&self, key: &str) {
        let s = &self.shared;
        let mut out = Out::default();
        {
            let mut g = s.lock();
            let stopped = g.stopped;
            let Some(run) = g.runs.get_mut(key) else {
                return;
            };
            if stopped || run.retiring {
                return;
            }
            cancel(&mut run.restart_timer);
            if let Some(slot) = run.child.as_mut() {
                run.cycling = true;
                slot.signalled = true;
                out.sigs.push(Sig {
                    child: slot.child.clone(),
                    key: key.to_string(),
                    pid: slot.pid,
                    sig: libc::SIGINT,
                    exited: slot.exited.clone(),
                });
            } else {
                spawn_locked(s, &mut g, key, &mut out);
            }
        }
        s.flush(out);
    }

    /// `cycleAll()` (`video-capture.ts:563-565`).
    pub fn cycle_all(&self) {
        let keys: Vec<String> = self.shared.lock().runs.keys().cloned().collect();
        for k in keys {
            self.cycle(&k);
        }
    }

    /// `anyRecording()` (`video-capture.ts:568-571`): any camera capturing TO DISK.
    pub fn any_recording(&self) -> bool {
        if !self.shared.opts.record {
            return false;
        }
        self.shared
            .lock()
            .runs
            .values()
            .any(|r| is_recording(r.state, r.counters.stall_failures, r.progress.bytes))
    }

    /// `status()` (`video-capture.ts:573-580`), in insertion order.
    pub fn status(&self) -> Vec<CaptureStatus> {
        let record = self.shared.opts.record;
        self.shared
            .lock()
            .runs
            .values()
            .map(|r| CaptureStatus {
                stream_key: r.cam.stream_key.clone(),
                state: r.state,
                session_id: r.session_id.clone(),
                recording: record
                    && is_recording(r.state, r.counters.stall_failures, r.progress.bytes),
            })
            .collect()
    }

    /// `shmSocketPath(streamKey)` (`video-capture.ts:596-598`): `<shmDir>/<key>.sock`.
    pub fn shm_socket_path(&self, key: &str) -> PathBuf {
        self.shared.shm_socket_path(key)
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn shm_socket_path(&self, key: &str) -> PathBuf {
        self.opts.shm_dir.join(format!("{key}.sock"))
    }

    /// Replay: track `Got EOS` and the first `New clock:` of the CURRENT
    /// child (`onStdout`, `video-capture.ts:676-683`); returns the PLAYING
    /// wall time when `onPlaying` must fire (outside the lock).
    fn note_stdout(&self, key: &str, child_id: u64, line: &str) -> Option<i64> {
        let mut g = self.lock();
        let run = g.runs.get_mut(key)?;
        if run.child.as_ref().map(|c| c.id) != Some(child_id) {
            return None;
        }
        if line.contains(GST_EOS_MARK) {
            run.eos_seen = true;
        }
        if !run.playing_reported && line.contains(GST_PLAYING_MARK) {
            run.playing_reported = true;
            return Some(self.opts.clock.wall_ms());
        }
        None
    }

    /// `{ wallMs: Date.now(), monoMs: monotonicMs() }`, read once per sweep.
    fn clock_pair(&self) -> ClockPair {
        ClockPair {
            wall_ms: self.opts.clock.wall_ms() as f64,
            mono_ms: monotonic_ms() as f64,
        }
    }

    /// Run the deferred callbacks in order, then deliver the signals.
    fn flush(&self, out: Out) {
        let o = &self.opts;
        for fx in out.fx {
            match fx {
                Fx::Event(level, msg) => {
                    if let Some(f) = &o.on_event {
                        f(level, &msg);
                    }
                }
                Fx::Status => {
                    if let Some(f) = &o.on_status {
                        f();
                    }
                }
                Fx::Spawned(key) => {
                    if let Some(f) = &o.on_spawned {
                        f(&key);
                    }
                }
                Fx::Active(key, path) => {
                    if let Some(f) = &o.on_active_path {
                        f(&key, path.as_deref());
                    }
                }
                Fx::Closed(p) => {
                    if let Some(f) = &o.on_segment_closed {
                        f(&p);
                    }
                }
                Fx::Ended(key, clean) => {
                    if let Some(h) = &o.replay {
                        h.on_ended(&key, clean);
                    }
                }
            }
        }
        for sig in &out.sigs {
            self.deliver(sig);
        }
    }

    /// `run.handle.stop(sig)` — execa's `child.kill(sig)`: the child pid
    /// only, a no-op once it is reaped (P11).
    fn deliver(&self, sig: &Sig) {
        if *sig.exited.borrow() {
            return; // reaped: execa's kill is a no-op, nothing is sent
        }
        let Some(pid) = sig.pid else {
            sig.child.signal(sig.sig);
            return;
        };
        // review-wp6 M5: taped AFTER the kill(2) returned, with the signal
        // really passed and its errno (0 = sent) — a record proves delivery.
        let (tape, key, s) = (self.opts.tape.clone(), sig.key.clone(), sig.sig);
        sig.child.signal_reported(
            sig.sig,
            Box::new(move |errno| tape.child_signal_sent(ROLE_CAPTURE, &key, pid, s, errno)),
        );
    }

    /// Home-relative path for the tape (the full path when outside `home`).
    fn rel(&self, p: &Path) -> String {
        rel_to(self.opts.home.as_deref(), p)
    }

    /// `fs_out` for one sweep's outcome (`close` + `sidecar` per closed
    /// fragment, `delete` per dropped 0-byte fragment).
    fn tape_sweep(&self, closed: &[PathBuf], reaped: &[PathBuf]) {
        tape_sweep_outcome(&*self.opts.tape, self.opts.home.as_deref(), closed, reaped);
    }

    /// `finalSweep(run, cleanStop)` (`video-capture.ts:1009-1020`): clean
    /// stop → every fragment `finalized:true`, newest included; crash → the
    /// per-fragment default (`!isNewest`). Drops 0-byte fragments.
    fn final_sweep(&self, _key: &str, run: &Run, clean: bool, out: &mut Out) {
        let (Some(dir), Some(manifest)) = (&run.session_dir, &run.manifest) else {
            return;
        };
        let res = sweep_session_dir(
            dir,
            manifest,
            &SweepIo {
                include_newest: true,
                finalized: clean.then_some(true),
                time_source: Some((self.opts.time_source)()),
                clock_pair: self.clock_pair(),
            },
        );
        self.tape_sweep(&res.closed, &res.reaped);
        for mp4 in res.closed {
            out.fx.push(Fx::Closed(mp4));
        }
    }
}

/// `p` relative to `home` when under it, else in full (tape paths).
fn rel_to(home: Option<&Path>, p: &Path) -> String {
    home.and_then(|h| p.strip_prefix(h).ok())
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

/// `fs_out` records for a sweep's (or [`recover_orphan_video_sessions`]'s)
/// closed and dropped fragments: per closed mp4 `close` (its size) then
/// `sidecar` (the sidecar's size), per dropped one `delete` (0). Paths are
/// relative to `home` when under it. A no-op without a tape.
pub fn tape_sweep_outcome(
    tape: &dyn TapeSink,
    home: Option<&Path>,
    closed: &[PathBuf],
    reaped: &[PathBuf],
) {
    if !tape.enabled() {
        return;
    }
    for mp4 in closed {
        tape.fs_out(&rel_to(home, mp4), FsOp::Close, size_or_zero(mp4));
        let sc = mp4.with_extension("json");
        tape.fs_out(&rel_to(home, &sc), FsOp::Sidecar, size_or_zero(&sc));
    }
    for p in reaped {
        tape.fs_out(&rel_to(home, p), FsOp::Delete, 0);
    }
}

/// The tape's periodic `shm` record (every [`SHM_TAPE_PERIOD_MS`], first one
/// a period after `start()`, until `stop()` or the supervisor is dropped).
fn tape_shm_audit(s: &Arc<Shared>) {
    let weak = Arc::downgrade(s);
    let period = Duration::from_millis(SHM_TAPE_PERIOD_MS);
    tokio::spawn(async move {
        // Rust-only (tape) — same re-arm-from-fire model as every agent timer.
        let mut iv = crate::timer::SetInterval::new(period);
        loop {
            iv.tick().await;
            let Some(s) = weak.upgrade() else { break };
            if s.lock().stopped {
                break;
            }
            s.opts
                .tape
                .shm(&shm_reaper::shm_pipe_names(&s.opts.shm_segment_dir));
        }
    });
}

/// `stop()`'s body under the caller's lock, returning the `stopAndWait`
/// snapshot — `(SIGKILL escalation, onExit flag)` of every live child — taken
/// in the SAME critical section that sets `stopped` (review-wp4 S1). Like the
/// TS, the snapshot is taken even when already stopped (`stop()` is then a
/// no-op), so a second `stop_and_wait` still awaits the exiting children.
fn stop_locked(g: &mut Inner, out: &mut Out) -> Vec<(Sig, watch::Receiver<bool>)> {
    let handles = g
        .runs
        .iter()
        .filter_map(|(k, r)| {
            r.child.as_ref().map(|c| {
                (
                    Sig {
                        child: c.child.clone(),
                        key: k.clone(),
                        pid: c.pid,
                        sig: libc::SIGKILL,
                        exited: c.exited.clone(),
                    },
                    c.exited.clone(),
                )
            })
        })
        .collect();
    if g.stopped {
        return handles;
    }
    g.stopped = true;
    for (key, run) in g.runs.iter_mut() {
        cancel(&mut run.stable_timer);
        cancel(&mut run.restart_timer);
        run.stop_watching();
        run.state = CaptureState::Stopped;
        if let Some(slot) = run.child.as_mut() {
            slot.signalled = true;
            out.sigs.push(Sig {
                child: slot.child.clone(),
                key: key.clone(),
                pid: slot.pid,
                sig: libc::SIGINT,
                exited: slot.exited.clone(),
            });
        }
    }
    handles
}

/// Wait until a child's `onExit` has run (a dropped sender counts as done).
async fn wait_exited(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await;
}

/// 16 random bytes for `mintSessionId` (`randomBytes(16)`).
fn random16() -> [u8; 16] {
    use std::io::Read;
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok()
    {
        return b;
    }
    // No /dev/urandom (never on a Pi): hash-seeded fallback, still unique per call.
    use std::hash::{BuildHasher, Hasher};
    for chunk in b.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(hrtime_ns());
        chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
    }
    b
}

/// Arm a one-shot timer for `slot` of run `run_id`.
fn arm(
    s: &Arc<Shared>,
    counter: &mut u64,
    delay_ms: u64,
    key: &str,
    run_id: u64,
    slot: Slot,
) -> Timer {
    let gen = next(counter);
    let weak: Weak<Shared> = Arc::downgrade(s);
    let key = key.to_string();
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        if let Some(s) = weak.upgrade() {
            fire(&s, &key, run_id, slot, gen);
        }
    });
    Timer {
        gen,
        abort: task.abort_handle(),
    }
}

/// A timer fired: act only if it is still the run's pending timer.
fn fire(s: &Arc<Shared>, key: &str, run_id: u64, slot: Slot, gen: u64) {
    let mut out = Out::default();
    {
        let mut g = s.lock();
        let stopped = g.stopped;
        let Some(run) = g.runs.get_mut(key) else {
            return;
        };
        if run.id != run_id {
            return;
        }
        let t = match slot {
            Slot::Stable => &mut run.stable_timer,
            Slot::Restart => &mut run.restart_timer,
        };
        if t.as_ref().map(|t| t.gen) != Some(gen) {
            return;
        }
        *t = None;
        match slot {
            // The stable timer (`video-capture.ts:700-708`).
            Slot::Stable => {
                if stopped {
                    return;
                }
                if run.child.is_some() {
                    run.state = CaptureState::Running;
                    run.counters.quick_failures = 0;
                    run.counters.backoff_ms = BASE_BACKOFF_MS;
                    out.fx.push(Fx::Status);
                }
            }
            // A stagger slot or `scheduleRestart` (`:484-487`, `:893-899`).
            Slot::Restart => spawn_locked(s, &mut g, key, &mut out),
        }
    }
    s.flush(out);
}

/// `launch(cameras)` (`video-capture.ts:447-490`): create + (staggered) spawn.
fn launch(s: &Arc<Shared>, g: &mut Inner, cameras: &[CaptureCamera], out: &mut Out) {
    for (index, cam) in cameras.iter().enumerate() {
        let id = next(&mut g.counter);
        let key = cam.stream_key.clone();
        g.runs.insert(key.clone(), Run::new(id, cam.clone()));
        let delay = stagger_delay_ms(index, s.opts.stagger_ms);
        if delay == 0 {
            spawn_locked(s, g, &key, out);
        } else {
            // Staggered bring-up rides the restart-timer slot so stop()
            // cancels a still-pending first spawn like a retry.
            let t = arm(s, &mut g.counter, delay, &key, id, Slot::Restart);
            g.runs.get_mut(&key).expect("inserted").restart_timer = Some(t);
        }
    }
}

/// `spawn(streamKey)` (`video-capture.ts:600-720`). Refuses once the
/// supervisor is stopped or the run retiring (`:601-602`): with
/// [`stop_locked`] that makes a timer firing after `stop_and_wait`'s snapshot
/// a no-op (review-wp4 S1).
///
/// Accepted divergence D27 (review-wp4 M1): a failed session-dir `mkdir` or
/// `session.json` write is logged at error (`[video-capture <key>] …`) and
/// the spawn CONTINUES — gst then fails on the missing dir and the normal
/// quick-fail ladder retries and parks. The TS throws (`:608` `mkdirSync`,
/// `:626` `writeSessionManifest`): from `start()`/`setCameras()` that rejects
/// the manager's `videoTransition` chain (`manager.ts:578-583`) →
/// `[agent] unhandledRejection` + `store.setError` (`worker.ts:656-660`) and
/// every later video transition is skipped (wedged until restart); from a
/// restart/stagger timer (`:484-487`, `:893-899`) it is an uncaught
/// exception → the process dies → systemd restart. Reproducing a wedge or a
/// crash is not parity worth having. Pinned by `capture_supervisor.rs`
/// `d27_spawn_time_fs_failure_logs_and_lets_the_ladder_retry`.
fn spawn_locked(s: &Arc<Shared>, g: &mut Inner, key: &str, out: &mut Out) {
    let Inner {
        runs,
        stopped,
        counter,
        ..
    } = g;
    let Some(run) = runs.get_mut(key) else {
        return;
    };
    if *stopped || run.retiring {
        return;
    }
    let o = &s.opts;
    let socket = s.shm_socket_path(key).to_string_lossy().into_owned();
    let env_head = std::env::var(CAPTURE_SOURCE_HEAD_ENV).ok();
    // Replay: an explicit head wins over the env seam (`captureSourceHead`).
    let hook_head: Option<Vec<String>> = o.replay.as_ref().map(|h| h.source_head(&run.cam));
    let args = if o.record {
        let session_id = mint_session_id(o.clock.wall_ms(), random16());
        let session_dir = o.video_root.join(key).join(&session_id);
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            // D27: the TS throws here (`video-capture.ts:608`).
            error!(
                "[video-capture {key}] session dir {}: {e}",
                session_dir.display()
            );
        }
        let manifest = VideoSessionManifest {
            v: 1,
            device_id: o.device_id.clone(),
            stream_key: key.to_string(),
            session_id: session_id.clone(),
            boot_id: o.boot_id.clone(),
            time_source: (o.time_source)(),
            preset: o.preset.name.to_string(),
            width: f64::from(o.preset.width),
            height: f64::from(o.preset.height),
            fps: f64::from(o.preset.fps),
            bitrate_kbps: f64::from(o.preset.bitrate_kbps),
            segment_seconds: o.segment_seconds as f64,
            anchor_ts: o.clock.wall_ms() as f64,
            anchor_mono: hrtime_ns().to_string(),
        };
        if let Err(e) = write_session_manifest(&session_dir, &manifest) {
            // D27: the TS throws here (`video-capture.ts:626`).
            error!(
                "[video-capture {key}] session.json write failed: {}",
                xorgate_hal::io::node_fs_message(&e, "open", &session_dir)
            );
        }
        let args = build_capture_pipeline_args(
            &CapturePipelineSpec {
                camera_name: run.cam.camera_name.clone(),
                head: hook_head.clone(),
                preset: o.preset.clone(),
                segment_seconds: o.segment_seconds,
                output_dir: session_dir.to_string_lossy().into_owned(),
                shm_socket_path: socket.clone(),
                encoder: o.encoder,
            },
            env_head.as_deref(),
        );
        run.session_id = Some(session_id);
        run.session_dir = Some(session_dir);
        run.manifest = Some(SessionManifest::Typed(manifest));
        args
    } else {
        run.session_id = None;
        run.session_dir = None;
        run.manifest = None;
        build_bridge_only_pipeline_args(
            &run.cam.camera_name,
            &socket,
            hook_head.as_deref(),
            env_head.as_deref(),
        )
    };
    run.state = CaptureState::Starting;
    run.started_at = o.clock.mono_ms();
    // Fresh watchdog baseline per child; `stall_failures` deliberately
    // survives the respawn so a camera that stalls every time eventually parks.
    run.progress = Progress {
        path: None,
        bytes: 0,
        at_ms: run.started_at,
    };
    run.stalled = false;
    run.cycling = false;
    run.eos_seen = false;
    run.playing_reported = false;
    run.tape_active = None;

    // A socket file left by a killed predecessor makes the new shmsink
    // accept nothing ("Connection refused" on the KVS side).
    let _ = std::fs::remove_file(&socket);

    let (child, rx) = o.backend.spawn(&args);
    let child: Arc<dyn CaptureChild> = Arc::from(child);
    let pid = child.pid();
    let (done_tx, done_rx) = watch::channel(false);
    let child_id = next(counter);
    run.child = Some(ChildSlot {
        id: child_id,
        child: child.clone(),
        pid,
        exited: done_rx,
        signalled: false,
    });

    // Encoders run at nice 10 (record mode only; the bridge is near-zero CPU).
    let nice = (o.record && pid.is_some() && child.renice(10).is_ok()).then_some(10);
    o.tape.child_spawn(&ChildSpawnRec {
        role: ROLE_CAPTURE.to_string(),
        key: key.to_string(),
        argv: std::iter::once(o.backend.program().to_string())
            .chain(args.iter().cloned())
            .collect(),
        env: Default::default(),
        pid: pid.unwrap_or(0),
        nice,
    });

    if o.record {
        watch_session(s, counter, key, run);
    }

    run.stable_timer = Some(arm(s, counter, o.stable_ms, key, run.id, Slot::Stable));

    out.fx.push(Fx::Event(
        ActivityLevel::Info,
        spawn_event(
            o.record,
            key,
            o.preset.name,
            run.session_id.as_deref().unwrap_or(""),
        ),
    ));
    out.fx.push(Fx::Status);
    out.fx.push(Fx::Spawned(key.to_string()));

    tokio::spawn(consume(s.clone(), key.to_string(), child_id, rx, done_tx));
}

/// Drain one child's events (both pipes, `Done` last) and run `onExit`.
async fn consume(
    s: Arc<Shared>,
    key: String,
    child_id: u64,
    mut rx: mpsc::UnboundedReceiver<ChildEvent>,
    done_tx: watch::Sender<bool>,
) {
    let mut done = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            // gst-launch's progress lines ride stdout: consumed always (an
            // unread pipe could block a chatty child); acted on only by the
            // replay hooks, which a real device never installs.
            ChildEvent::Stdout(line) => {
                s.opts.tape.child_line(ROLE_CAPTURE, &key, 1, &line);
                // ACTED on only by the replay hooks (`video-capture.ts:676-683`).
                if let Some(h) = &s.opts.replay {
                    if let Some(wall) = s.note_stdout(&key, child_id, &line) {
                        h.on_playing(&key, wall);
                    }
                }
            }
            ChildEvent::Stderr(line) => {
                s.opts.tape.child_line(ROLE_CAPTURE, &key, 2, &line);
                if let Some(l) = stderr_log_line(&key, &line) {
                    error!("{l}");
                }
            }
            ChildEvent::Done(d) => {
                done = Some(d);
                break;
            }
        }
    }
    on_exit(&s, &key, child_id, &done.unwrap_or_default());
    let _ = done_tx.send(true);
}

/// `onExit(streamKey, exitCode, notFound)` (`video-capture.ts:722-856`).
fn on_exit(s: &Arc<Shared>, key: &str, child_id: u64, done: &ChildDone) {
    let o = &s.opts;
    let mut out = Out::default();
    {
        let mut g = s.lock();
        let stopped = g.stopped;
        let exiting = g.exiting;
        let Inner {
            runs,
            counter,
            exit_reaped,
            ..
        } = &mut *g;
        let Some(run) = runs.get_mut(key) else {
            return;
        };
        if run.child.as_ref().map(|c| c.id) != Some(child_id) {
            return;
        }
        cancel(&mut run.stable_timer);
        let slot = run.child.take().expect("checked");
        if exiting {
            // R-WP5R-1: killed by the agent-exit cleanup. Nothing but the shm
            // reap — as when the runtime drop killed it unobserved: no final
            // sweep (the newest fragment keeps no sidecar; the next boot's
            // recovery stamps it finalized:false, like the TS), no
            // onSegmentClosed/enforcer, no tape close/sidecar/child_exit, no
            // event. The reaped names count in the cleanup's journal line.
            let reaped = shm_reaper::reap_shm_segments(&o.shm_segment_dir, slot.pid);
            exit_reaped.push((key.to_string(), reaped));
            return;
        }
        let stalled = std::mem::take(&mut run.stalled);
        run.stop_watching();
        // The child is gone, so anything of its still in the shm tmpfs is
        // orphaned by definition.
        let _reaped = shm_reaper::reap_shm_segments(&o.shm_segment_dir, slot.pid);
        o.tape.child_exit(&ChildExitRec {
            role: ROLE_CAPTURE.to_string(),
            key: key.to_string(),
            pid: slot.pid.unwrap_or(0),
            code: done.exit_code,
            signal: done.signal,
            external: !slot.signalled,
        });
        if o.tape.enabled() {
            o.tape.shm(&shm_reaper::shm_pipe_names(&o.shm_segment_dir));
        }

        // Close out what the child left: clean stop (EOS ran) → the last
        // fragment is finalised; crash → it may be truncated.
        let cycled = std::mem::take(&mut run.cycling);
        // Replay: a lap ends by EOF (eosSeen + eosIsClean) or by cycle().
        // Without hooks `eos_seen` is never set (only `note_stdout` sets it).
        let eos_end = std::mem::take(&mut run.eos_seen)
            && o.replay.as_ref().is_some_and(|h| h.eos_is_clean());
        let clean = stopped || run.retiring || cycled || eos_end;
        s.final_sweep(key, run, clean, &mut out);
        let ended = run.session_id.take();
        run.session_dir = None;
        run.manifest = None;
        out.fx.push(Fx::Active(key.to_string(), None));
        if o.replay.is_some() {
            out.fx.push(Fx::Ended(key.to_string(), clean));
        }

        if run.retiring {
            // Disabled camera: finalised and swept — drop it entirely.
            runs.shift_remove(key);
            out.fx.push(Fx::Status);
        } else if stopped {
            run.state = CaptureState::Stopped;
            out.fx.push(Fx::Status);
        } else if let (true, Some(h)) = (cycled || eos_end, o.replay.as_ref()) {
            // Replay lap boundary (`video-capture.ts:768-787`): a clean end
            // is not a failure. Respawn after the gap, or park.
            run.counters.quick_failures = 0;
            run.counters.backoff_ms = BASE_BACKOFF_MS;
            let tail = ended
                .as_deref()
                .map(|sid| format!(" (session {}…)", &sid[..sid.len().min(8)]))
                .unwrap_or_default();
            match h.respawn_hold_ms(key) {
                None => {
                    run.state = CaptureState::Stopped;
                    out.fx.push(Fx::Event(
                        ActivityLevel::Info,
                        format!("video capture {key} ended cleanly; parked (no loop)"),
                    ));
                }
                Some(hold) => {
                    run.state = CaptureState::Retrying;
                    out.fx.push(Fx::Event(
                        ActivityLevel::Info,
                        format!(
                            "video capture {key} ended cleanly{tail}; next lap in {}s",
                            (hold as f64 / 1000.0).round() as u64
                        ),
                    ));
                    run.restart_timer = Some(arm(s, counter, hold, key, run.id, Slot::Restart));
                }
            }
            out.fx.push(Fx::Status);
        } else {
            let ran_ms = o.clock.mono_ms().saturating_sub(run.started_at);
            let v = exit_verdict(&ExitInput {
                stream_key: key,
                exit_code: done.exit_code,
                // execa 9 (`reject:false`) resolves a missing binary with
                // `notFound:false, exitCode:null` (phase-2 review-wp4), so the
                // TS never takes its "not found; parking" branch: reproduced.
                not_found: false,
                stalled,
                ran_ms,
                stable_ms: o.stable_ms,
                ended_session: ended.as_deref(),
                counters: run.counters,
            });
            if let Some(st) = v.state {
                run.state = st;
            }
            run.counters = v.counters;
            out.fx.push(Fx::Event(v.level, v.event));
            run.restart_timer = Some(arm(s, counter, v.restart_ms, key, run.id, Slot::Restart));
            out.fx.push(Fx::Status);
        }
    }
    s.flush(out);
}

/// `watchSession(run)` (`video-capture.ts:902-911`): fs.watch + slow poll → sweep.
fn watch_session(s: &Arc<Shared>, counter: &mut u64, key: &str, run: &mut Run) {
    let Some(dir) = run.session_dir.clone() else {
        return;
    };
    let gen = next(counter);
    run.watch_gen = Some(gen);
    let run_id = run.id;
    if s.opts.fs_watch {
        if let Ok((w, mut rx)) = fs_watch::watch_dir(&dir) {
            let weak = Arc::downgrade(s);
            let k = key.to_string();
            let pump = tokio::spawn(async move {
                while rx.recv().await.is_some() {
                    let Some(s) = weak.upgrade() else { break };
                    sweep(&s, &k, run_id, gen);
                }
            });
            run.watcher = Some((w, pump.abort_handle()));
        } // else: the poll still covers us
    }
    let period = Duration::from_millis(s.opts.sweep_poll_ms.max(1));
    let weak = Arc::downgrade(s);
    let k = key.to_string();
    let task = tokio::spawn(async move {
        // setInterval: the first tick one period after arming, then one
        // period after each fire (P-WP6-1).
        let mut iv = crate::timer::SetInterval::new(period);
        loop {
            iv.tick().await;
            let Some(s) = weak.upgrade() else { break };
            sweep(&s, &k, run_id, gen);
        }
    });
    run.sweep_timer = Some(Timer {
        gen,
        abort: task.abort_handle(),
    });
}

/// `sweep(run)` (`video-capture.ts:920-954`): sidecar closed fragments with
/// the CURRENT time provenance, report the active fragment, fsync it (a hard
/// power cut otherwise loses a whole fragment from the page cache), then the
/// progress watchdog tick.
fn sweep(s: &Arc<Shared>, key: &str, run_id: u64, gen: u64) {
    let o = &s.opts;
    let mut out = Out::default();
    {
        let mut g = s.lock();
        let stopped = g.stopped;
        let Some(run) = g.runs.get_mut(key) else {
            return;
        };
        if run.id != run_id || run.watch_gen != Some(gen) {
            return;
        }
        let (Some(dir), Some(manifest)) = (&run.session_dir, &run.manifest) else {
            return;
        };
        let res = sweep_session_dir(
            dir,
            manifest,
            &SweepIo {
                include_newest: false,
                finalized: None,
                time_source: Some((o.time_source)()),
                clock_pair: s.clock_pair(),
            },
        );
        if o.tape.enabled() {
            s.tape_sweep(&res.closed, &res.reaped);
            if let Some(p) = &res.active_path {
                if run.tape_active.as_ref() != Some(p) {
                    o.tape.fs_out(&s.rel(p), FsOp::Create, size_or_zero(p));
                    run.tape_active = Some(p.clone());
                }
            }
        }
        out.fx
            .push(Fx::Active(key.to_string(), res.active_path.clone()));
        for mp4 in res.closed {
            out.fx.push(Fx::Closed(mp4));
        }
        if let Some(p) = &res.active_path {
            if let Ok(f) = std::fs::File::open(p) {
                let _ = f.sync_all(); // rotated away mid-sweep — fine
            }
        }
        check_progress(s, stopped, key, run, res.active_path.as_deref(), &mut out);
    }
    s.flush(out);
}

/// `checkProgress(run, activePath)` (`video-capture.ts:963-1007`): no growth
/// and no rotation for `stall_ms` ⇒ SIGKILL (SIGINT cannot work: `-e` makes
/// it an EOS request through the very pipeline that stopped moving).
fn check_progress(
    s: &Shared,
    stopped: bool,
    key: &str,
    run: &mut Run,
    active: Option<&Path>,
    out: &mut Out,
) {
    let o = &s.opts;
    if !o.record || stopped || run.retiring || run.stalled {
        return;
    }
    if run.child.is_none() {
        return;
    }
    let bytes = active.map(size_or_zero).unwrap_or(0);
    let now = o.clock.mono_ms();
    let active_str = active.map(|p| p.to_string_lossy().into_owned());
    match progress_verdict(active_str.as_deref(), bytes, &run.progress, now, o.stall_ms) {
        ProgressVerdict::Advanced { path, bytes } => {
            run.progress = Progress {
                path: Some(path),
                bytes,
                at_ms: now,
            };
            run.counters.stall_failures = 0;
        }
        ProgressVerdict::Waiting => {}
        ProgressVerdict::Stalled => {
            run.stalled = true;
            run.state = CaptureState::Failed;
            run.stop_watching();
            out.fx.push(Fx::Event(
                ActivityLevel::Warn,
                stall_event(key, o.stall_ms, run.session_id.as_deref()),
            ));
            out.fx.push(Fx::Status);
            let slot = run.child.as_mut().expect("checked");
            slot.signalled = true;
            out.sigs.push(Sig {
                child: slot.child.clone(),
                key: key.to_string(),
                pid: slot.pid,
                sig: libc::SIGKILL,
                exited: slot.exited.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use xorgate_core::video_presets::resolve_video_preset;

    use super::*;
    use crate::ctx::AgentClock;

    type GapHook = Box<dyn FnOnce(&Arc<Shared>)>;

    thread_local! {
        /// Runs inside `stop_and_wait` at the first instant after its child
        /// snapshot at which another task can take the supervisor lock —
        /// where a racing restart/stagger timer task runs (review-wp4 S1).
        /// Before the fix that instant was BETWEEN the snapshot and `stop()`
        /// (the test then saw 3 spawns, 1 awaited); now it is after both.
        static STOP_GAP_HOOK: RefCell<Option<GapHook>> = const { RefCell::new(None) };
    }

    pub(super) fn run_stop_gap_hook(s: &Arc<Shared>) {
        if let Some(h) = STOP_GAP_HOOK.with(|c| c.borrow_mut().take()) {
            h(s);
        }
    }

    /// Like a real gst-launch: exits a while after a signal, and its shmsink
    /// unlinks the socket path on the way out (`video-capture.ts:509-515`).
    struct SlowChild {
        socket: PathBuf,
        exit_ms: u64,
        tx: Mutex<Option<mpsc::UnboundedSender<ChildEvent>>>,
    }

    impl CaptureChild for SlowChild {
        fn pid(&self) -> Option<u32> {
            None
        }
        fn signal(&self, _sig: i32) {
            let Some(tx) = self.tx.lock().unwrap().take() else {
                return;
            };
            let (socket, ms) = (self.socket.clone(), self.exit_ms);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                let _ = std::fs::remove_file(&socket);
                let _ = tx.send(ChildEvent::Done(ChildDone {
                    exit_code: Some(0),
                    ..ChildDone::default()
                }));
            });
        }
        fn renice(&self, _nice: i32) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Child N (1-based) takes `300 * N` ms to exit after its signal.
    #[derive(Default)]
    struct SlowBackend {
        spawns: AtomicUsize,
    }

    impl CaptureBackend for SlowBackend {
        fn program(&self) -> &str {
            backend::GST_LAUNCH
        }
        fn spawn(
            &self,
            args: &[String],
        ) -> (Box<dyn CaptureChild>, mpsc::UnboundedReceiver<ChildEvent>) {
            let n = self.spawns.fetch_add(1, Ordering::SeqCst) as u64 + 1;
            let socket = args
                .iter()
                .find_map(|a| a.strip_prefix("socket-path="))
                .map(PathBuf::from)
                .expect("socket-path= in the argv");
            let (tx, rx) = mpsc::unbounded_channel();
            let child = SlowChild {
                socket,
                exit_ms: 300 * n,
                tx: Mutex::new(Some(tx)),
            };
            (Box::new(child), rx)
        }
    }

    /// S1 / invariant 8: a stagger (restart-slot) timer that comes due at the
    /// instant `stop_and_wait` runs must not spawn a child the call never
    /// awaits — its shmsink would unlink the replacement supervisor's socket.
    #[tokio::test(start_paused = true)]
    async fn stop_and_wait_leaves_no_child_a_racing_restart_timer_could_spawn() {
        let home = std::env::temp_dir().join(format!(
            "xg-capture-s1-{}-{}",
            std::process::id(),
            hrtime_ns()
        ));
        let shm = home.join("shm");
        let be = Arc::new(SlowBackend::default());
        let cam = |k: &str| CaptureCamera {
            stream_key: k.into(),
            camera_name: format!("/base/{k}@1a"),
        };
        let mut o = VideoCaptureOpts::new(
            "dev-1",
            vec![cam("cam0"), cam("cam1")],
            false,
            resolve_video_preset("1080p15-2mbps").clone(),
            60,
            home.join("video"),
            shm.clone(),
            "boot-1",
            Arc::new(|| TimeSource::Ntp),
            Arc::new(AgentClock::following_tokio(1_760_000_000_000)),
        );
        o.backend = be.clone();
        o.stagger_ms = 1_000;
        o.fs_watch = false;
        o.shm_segment_dir = home.join("devshm");
        let sup = VideoCaptureSupervisor::new(o);
        sup.start();
        // cam0 spawned; cam1 waits on its stagger slot (the restart timer).
        assert_eq!(be.spawns.load(Ordering::SeqCst), 1);
        let (run_id, gen) = {
            let g = sup.shared.lock();
            let r = &g.runs["cam1"];
            (r.id, r.restart_timer.as_ref().expect("stagger armed").gen)
        };
        tokio::time::advance(Duration::from_millis(999)).await;

        // The timer comes due exactly now: its task takes the lock right
        // after the snapshot. Fire it with its real generation, and also try
        // the spawn that a timer past its own guards would make.
        STOP_GAP_HOOK.with(|c| {
            *c.borrow_mut() = Some(Box::new(move |s: &Arc<Shared>| {
                fire(s, "cam1", run_id, Slot::Restart, gen);
                let mut out = Out::default();
                {
                    let mut g = s.lock();
                    spawn_locked(s, &mut g, "cam1", &mut out);
                }
                s.flush(out);
            }));
        });
        sup.stop_and_wait(Duration::from_secs(10)).await;
        assert!(STOP_GAP_HOOK.with(|c| c.borrow().is_none()), "the hook ran");

        assert_eq!(
            be.spawns.load(Ordering::SeqCst),
            1,
            "no child spawned after the snapshot"
        );
        assert!(
            sup.shared.lock().runs.values().all(|r| r.child.is_none()),
            "stop_and_wait awaited every child"
        );

        // The replacement supervisor's sockets survive every old child's exit.
        std::fs::create_dir_all(&shm).unwrap();
        for k in ["cam0", "cam1"] {
            std::fs::write(shm.join(format!("{k}.sock")), b"").unwrap();
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        for k in ["cam0", "cam1"] {
            assert!(shm.join(format!("{k}.sock")).exists(), "{k}.sock deleted");
        }
        assert_eq!(be.spawns.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&home);
    }
}
