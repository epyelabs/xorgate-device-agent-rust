//! Fakes for the recording-spine tests: a fake [`RecordingBackend`] whose
//! capture supervisor logs `{cam, record}` spawns like the TS tests' fake
//! spawner (one child per camera; SIGINT on stop/disable), plus a shared
//! hand-driven clock (wall + mono, P8) and the manager builder.

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use xorgate_agent::ctx::BoxFut;
use xorgate_agent::recording::manager::*;
use xorgate_core::clock::Clock;
use xorgate_core::recording_config::ResolvedRetention;
use xorgate_core::telemetry::{ActivityLevel, RecordingStatus};
use xorgate_core::types::{GnssView, PowerSampleReading, TimeSource};
use xorgate_core::video_capture::CaptureCamera;
use xorgate_hal::boot_sources::FileBootSources;
use xorgate_hal::io::null_tape;

/// One wall + mono clock for everything (the TS rig's `clock`).
#[derive(Default)]
pub struct TestClock {
    t: Mutex<(i64, u64)>,
}

impl TestClock {
    pub fn new(wall: i64, mono: u64) -> Arc<Self> {
        Arc::new(Self {
            t: Mutex::new((wall, mono)),
        })
    }
    pub fn advance(&self, ms: u64) {
        let mut g = self.t.lock().unwrap();
        g.0 += ms as i64;
        g.1 += ms;
    }
    pub fn get(&self) -> (i64, u64) {
        *self.t.lock().unwrap()
    }
}

impl Clock for TestClock {
    fn wall_ms(&self) -> i64 {
        self.t.lock().unwrap().0
    }
    fn mono_ms(&self) -> u64 {
        self.t.lock().unwrap().1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    pub cam: String,
    pub record: bool,
    pub stopped: bool,
    pub preset: &'static str,
}

/// Everything the fakes observed, in order.
#[derive(Default)]
pub struct Log {
    pub spawns: Mutex<Vec<Spawn>>,
    pub events: Mutex<Vec<(ActivityLevel, String)>>,
    pub live_shed: Mutex<Vec<bool>>,
    pub disabled: Mutex<Vec<Vec<String>>>,
    pub statuses: Mutex<Vec<RecordingStatus>>,
    pub recorders: Mutex<Vec<RecorderSpecView>>,
    pub recorder_stops: AtomicUsize,
    /// The next N `stop()` calls fail (a throwing finalize).
    pub recorder_stop_failures: AtomicUsize,
    /// Each recorder's `stopped` flag, in creation order. As in WP2's
    /// `TelemetryRecorder::stop` (= `telemetry-recorder.ts:75-81`), it is set
    /// and the sampling timer cleared BEFORE the finalize. A recorder whose
    /// finalize failed is therefore dead: it samples nothing (review-wp1 M1).
    pub recorder_stopped: Mutex<Vec<Arc<std::sync::atomic::AtomicBool>>>,
    /// Each recorder's `onSegmentClosed`, in creation order.
    pub segment_closed: Mutex<Vec<NotifyFn>>,
    /// A successful `stop()` fires `onSegmentClosed` (WP2's finalize does).
    pub fire_closed_on_stop: std::sync::atomic::AtomicBool,
    /// Every `disk.setActivePath("telemetry", …)`.
    pub active_paths: Mutex<Vec<Option<PathBuf>>>,
    pub finalize_calls: AtomicUsize,
    /// D41: start-up shm sweeps, and `finalize_calls` at the time of each.
    pub sweeps: Mutex<Vec<usize>>,
    /// D41: agent-exit cleanups: the preset of every supervisor exit-killed.
    pub exit_kills: Mutex<Vec<String>>,
    /// R-WP5R-3: when set, a supervisor's `stop_and_wait` hangs (its child
    /// "in its EOS") until the exit cleanup kills it.
    pub hang_stop_and_wait: AtomicBool,
    /// R-WP5R-6: when set, an exit-killed child's exit is never observed
    /// (its watch sender is held here, never sent).
    pub exit_never: AtomicBool,
    pub exit_never_tx: Mutex<Vec<tokio::sync::watch::Sender<bool>>>,
    pub recover_calls: AtomicUsize,
    pub disks: AtomicUsize,
    pub enforces: AtomicUsize,
    pub uploaders: AtomicUsize,
    pub kicks: AtomicUsize,
    pub pauses: AtomicUsize,
    pub uploader_stops: AtomicUsize,
    pub captures: Mutex<Vec<(bool, Vec<String>)>>,
    pub sample_calls: AtomicUsize,
    pub spawned_callbacks: Mutex<Vec<String>>,
    /// The time source each recorder/capture saw at construction.
    pub time_sources: Mutex<Vec<TimeSource>>,
}

#[derive(Debug, Clone)]
pub struct RecorderSpecView {
    pub session_id: String,
    pub rate_hz: u32,
    pub dir: PathBuf,
}

pub struct FakeCapture {
    log: Arc<Log>,
    record: bool,
    preset: &'static str,
    /// (stream key, index into log.spawns) of the live children.
    live: Mutex<Vec<(String, usize)>>,
    on_spawned: Arc<dyn Fn(&str) + Send + Sync>,
    /// R-WP5R-3: released by `exit_kill` (a hung `stop_and_wait`).
    killed: Arc<tokio::sync::Notify>,
    /// The cameras it was built with (one exit-killed child each).
    cams: Vec<String>,
}

impl FakeCapture {
    fn spawn(&self, cam: &str) {
        let mut sp = self.log.spawns.lock().unwrap();
        sp.push(Spawn {
            cam: cam.to_string(),
            record: self.record,
            stopped: false,
            preset: self.preset,
        });
        self.live
            .lock()
            .unwrap()
            .push((cam.to_string(), sp.len() - 1));
        drop(sp);
        (self.on_spawned)(cam);
    }
    fn stop_all(&self) {
        let live: Vec<_> = self.live.lock().unwrap().drain(..).collect();
        let mut sp = self.log.spawns.lock().unwrap();
        for (_, i) in live {
            sp[i].stopped = true;
        }
    }
}

impl CaptureApi for FakeCapture {
    fn start(&self) {}
    fn set_cameras(&self, cameras: Vec<CaptureCamera>) -> BoxFut<'_, ()> {
        Box::pin(async move {
            let wanted: HashSet<String> = cameras.iter().map(|c| c.stream_key.clone()).collect();
            let mut live = self.live.lock().unwrap();
            let mut sp = self.log.spawns.lock().unwrap();
            live.retain(|(k, i)| {
                let keep = wanted.contains(k);
                if !keep {
                    sp[*i].stopped = true;
                }
                keep
            });
            let have: HashSet<String> = live.iter().map(|(k, _)| k.clone()).collect();
            drop(sp);
            drop(live);
            for c in cameras {
                if !have.contains(&c.stream_key) {
                    self.spawn(&c.stream_key);
                }
            }
        })
    }
    fn stop(&self) {
        self.stop_all();
    }
    fn stop_and_wait(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            if self.log.hang_stop_and_wait.load(Ordering::SeqCst) {
                self.killed.notified().await;
            }
            self.stop_all()
        })
    }
    fn exit_kill(&self) -> Vec<xorgate_agent::recording::capture::ExitKilled> {
        self.log
            .exit_kills
            .lock()
            .unwrap()
            .push(self.preset.to_string());
        self.killed.notify_one();
        let never = self.log.exit_never.load(Ordering::SeqCst);
        self.cams
            .iter()
            .enumerate()
            .map(|(i, cam)| {
                let (tx, rx) = tokio::sync::watch::channel(!never);
                if never {
                    self.log.exit_never_tx.lock().unwrap().push(tx);
                }
                xorgate_agent::recording::capture::ExitKilled {
                    key: cam.clone(),
                    pid: Some(1162 + i as u32),
                    exited: rx,
                }
            })
            .collect()
    }
    fn exit_finish(
        &self,
        killed: &[xorgate_agent::recording::capture::ExitKilled],
    ) -> Vec<(String, Vec<String>)> {
        killed
            .iter()
            .map(|k| {
                (
                    k.key.clone(),
                    vec![format!("shmpipe.{:>5}.    0", k.pid.unwrap_or(0))],
                )
            })
            .collect()
    }
    fn active_session_ids(&self) -> HashSet<String> {
        HashSet::new()
    }
    fn any_recording(&self) -> bool {
        self.record && !self.live.lock().unwrap().is_empty()
    }
    fn status(&self) -> Vec<CaptureStatus> {
        self.live
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| CaptureStatus {
                stream_key: k.clone(),
                state: "running".into(),
                session_id: None,
                recording: self.record,
            })
            .collect()
    }
}

struct FakeRecorder {
    log: Arc<Log>,
    active: Option<PathBuf>,
    on_closed: NotifyFn,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}
impl RecorderApi for FakeRecorder {
    fn start(&self) {}
    fn stop(&self) -> Result<(), String> {
        // `this.stopped = true; clearInterval(this.timer)` come first.
        self.stopped.store(true, Ordering::SeqCst);
        let fails = &self.log.recorder_stop_failures;
        if fails.load(Ordering::SeqCst) > 0 {
            fails.fetch_sub(1, Ordering::SeqCst);
            return Err("ENOSPC: no space left on device, write".into());
        }
        self.log.recorder_stops.fetch_add(1, Ordering::SeqCst);
        // WP2's finalize fires `onSegmentClosed` from inside `stop()`.
        if self.log.fire_closed_on_stop.load(Ordering::SeqCst) {
            (self.on_closed)();
        }
        Ok(())
    }
    fn active_path(&self) -> Option<PathBuf> {
        self.active.clone()
    }
}

pub struct FakeDisk {
    log: Arc<Log>,
    pub stats: Mutex<BacklogStats>,
}
impl DiskApi for FakeDisk {
    fn set_retention(&self, _r: &ResolvedRetention) {}
    fn set_active_path(&self, _owner: &str, p: Option<PathBuf>) {
        self.log.active_paths.lock().unwrap().push(p);
    }
    fn stats(&self) -> BacklogStats {
        *self.stats.lock().unwrap()
    }
    fn enforce(&self) -> BoxFut<'static, ()> {
        self.log.enforces.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

pub struct FakeUploader {
    log: Arc<Log>,
}
impl UploaderApi for FakeUploader {
    fn kick(&self) {
        self.log.kicks.fetch_add(1, Ordering::SeqCst);
    }
    fn pause(&self) {
        self.log.pauses.fetch_add(1, Ordering::SeqCst);
    }
    fn stop(&self) {
        self.log.uploader_stops.fetch_add(1, Ordering::SeqCst);
    }
    fn status(&self) -> UploaderStatus {
        UploaderStatus::default()
    }
}

pub struct FakeBackend {
    pub log: Arc<Log>,
    pub orphans: usize,
    pub backfilled: usize,
}

impl RecordingBackend for FakeBackend {
    fn finalize_orphan_segments(&self, _t: &Path) -> usize {
        self.log.finalize_calls.fetch_add(1, Ordering::SeqCst);
        self.orphans
    }
    fn recover_orphan_video_sessions(&self, _v: &Path) -> usize {
        self.log.recover_calls.fetch_add(1, Ordering::SeqCst);
        self.backfilled
    }
    fn sweep_dead_shm_segments(&self) -> Vec<(String, u32)> {
        let f = self.log.finalize_calls.load(Ordering::SeqCst);
        self.log.sweeps.lock().unwrap().push(f);
        vec![("shmpipe. 1035.    0".into(), 1035)]
    }
    fn boot_id(&self) -> String {
        "boot-1".into()
    }
    fn disk(&self, _r: &Path, _ret: &ResolvedRetention, _e: EventFn) -> Arc<dyn DiskApi> {
        self.log.disks.fetch_add(1, Ordering::SeqCst);
        Arc::new(FakeDisk {
            log: self.log.clone(),
            stats: Mutex::new(BacklogStats::default()),
        })
    }
    fn recorder(&self, spec: RecorderSpec) -> Arc<dyn RecorderApi> {
        self.log
            .time_sources
            .lock()
            .unwrap()
            .push(spec.time_source.current());
        self.log.recorders.lock().unwrap().push(RecorderSpecView {
            session_id: spec.session_id.clone(),
            rate_hz: spec.rate_hz,
            dir: spec.dir.clone(),
        });
        self.log
            .segment_closed
            .lock()
            .unwrap()
            .push(spec.on_segment_closed.clone());
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.log
            .recorder_stopped
            .lock()
            .unwrap()
            .push(stopped.clone());
        Arc::new(FakeRecorder {
            log: self.log.clone(),
            active: Some(spec.dir.join("seg-active.jsonl")),
            on_closed: spec.on_segment_closed,
            stopped,
        })
    }
    fn uploader(&self, _spec: UploaderSpec) -> Arc<dyn UploaderApi> {
        self.log.uploaders.fetch_add(1, Ordering::SeqCst);
        Arc::new(FakeUploader {
            log: self.log.clone(),
        })
    }
    fn capture(&self, spec: CaptureSpec) -> Arc<dyn CaptureApi> {
        self.log
            .time_sources
            .lock()
            .unwrap()
            .push(spec.time_source.current());
        self.log.captures.lock().unwrap().push((
            spec.record,
            spec.cameras.iter().map(|c| c.stream_key.clone()).collect(),
        ));
        let cap = Arc::new(FakeCapture {
            log: self.log.clone(),
            record: spec.record,
            preset: spec.preset.name,
            live: Mutex::new(Vec::new()),
            on_spawned: spec.on_spawned.clone(),
            killed: Arc::new(tokio::sync::Notify::new()),
            cams: spec.cameras.iter().map(|c| c.stream_key.clone()).collect(),
        });
        // `launch()`: index 0 spawns now; later cameras after index × stagger
        // (the fixtures only use stagger 0 with two cameras).
        for (i, c) in spec.cameras.iter().enumerate() {
            assert!(
                i == 0 || spec.stagger_ms == 0,
                "stagger not modeled by the fake"
            );
            cap.spawn(&c.stream_key);
        }
        cap
    }
}

/// A time source whose `refresh` flips `unsynced` → `value` (optionally hung).
pub struct FakeTimeSource {
    pub value: Mutex<TimeSource>,
    pub probed: TimeSource,
    pub hang: bool,
    pub delay_ms: u64,
}
impl TimeSourceApi for FakeTimeSource {
    fn current(&self) -> TimeSource {
        *self.value.lock().unwrap()
    }
    fn refresh(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            if self.hang {
                std::future::pending::<()>().await;
            }
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            *self.value.lock().unwrap() = self.probed;
        })
    }
}

pub fn ntp() -> Arc<FakeTimeSource> {
    Arc::new(FakeTimeSource {
        value: Mutex::new(TimeSource::Ntp),
        probed: TimeSource::Ntp,
        hang: false,
        delay_ms: 0,
    })
}

/// The knobs of one rig (`makeRig`/`makeManager` in the TS tests).
pub struct Rig {
    pub home: PathBuf,
    pub cameras: Vec<String>,
    pub media_enabled: bool,
    pub video: bool,
    pub budget_ma: Option<u32>,
    pub gnss_wired: bool,
    pub gnss: Arc<Mutex<Option<GnssView>>>,
    pub reading: Arc<Mutex<Option<PowerSampleReading>>>,
    pub power_sample: bool,
    pub clock: Arc<TestClock>,
    pub log: Arc<Log>,
    pub time_source: Arc<FakeTimeSource>,
    pub orphans: usize,
    pub backfilled: usize,
    pub poll_ms: u64,
    pub shed_sample_ms: u64,
    pub time_probe_timeout_ms: Option<u64>,
    pub upload: bool,
    /// Called inside `setLiveShed` (after logging) — the no-deadlock test blocks here.
    pub live_shed_hook: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    /// A real backend instead of [`FakeBackend`] (component-wiring tests).
    pub backend_override: Option<Arc<dyn RecordingBackend>>,
    /// Upload options instead of the `upload` flag's always-OK put.
    pub upload_override: Option<xorgate_cloud::s3::UploadOptions>,
    /// The clock trust gate (Phase 5b); `None` = always trusted.
    pub clock_trusted: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl Rig {
    pub fn new(tag: &str) -> Self {
        let home = std::env::temp_dir().join(format!(
            "xg-rec-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        Self {
            home,
            cameras: vec!["cam0".into()],
            media_enabled: true,
            video: true,
            budget_ma: None,
            gnss_wired: true,
            gnss: Arc::new(Mutex::new(None)),
            reading: Arc::new(Mutex::new(None)),
            power_sample: true,
            clock: TestClock::new(1_760_000_000_000, 5_000_000),
            log: Arc::new(Log::default()),
            time_source: ntp(),
            orphans: 0,
            backfilled: 0,
            poll_ms: 3_600_000,
            shed_sample_ms: 3_600_000,
            time_probe_timeout_ms: None,
            upload: false,
            live_shed_hook: None,
            backend_override: None,
            upload_override: None,
            clock_trusted: None,
        }
    }

    pub fn settings_path(&self) -> PathBuf {
        self.home.join("settings.json")
    }
    pub fn telemetry_path(&self) -> PathBuf {
        self.home.join("telemetry.json")
    }
    pub fn root(&self) -> PathBuf {
        self.home.join("recordings")
    }

    /// `saveSettings({ recording })`.
    pub fn save_recording(&self, recording: &serde_json::Value) {
        let doc = serde_json::json!({ "recording": recording });
        std::fs::write(self.settings_path(), serde_json::to_vec(&doc).unwrap()).unwrap();
    }

    pub fn build(&self) -> RecordingManager {
        if let Some(b) = self.budget_ma {
            std::fs::write(self.home.join("max_current"), b.to_be_bytes()).unwrap();
        }
        let gnss = self.gnss.clone();
        let sources = FileBootSources {
            budget_path: self.home.join("max_current"),
            telemetry_file: self.telemetry_path(),
            tape: null_tape(),
            gnss: self
                .gnss_wired
                .then(|| -> xorgate_hal::boot_sources::GnssFn {
                    Arc::new(move || *gnss.lock().unwrap())
                }),
        };
        let log = self.log.clone();
        let cams: Vec<CaptureCamera> = self
            .cameras
            .iter()
            .map(|k| CaptureCamera {
                stream_key: k.clone(),
                camera_name: format!("/base/{k}@1a"),
            })
            .collect();
        let (l1, l2, l3) = (log.clone(), log.clone(), log.clone());
        let hook = self.live_shed_hook.clone();
        let video = VideoOpts {
            media_enabled: self.media_enabled,
            cameras: Arc::new(move || cams.clone()),
            encoder: Arc::new(|| xorgate_core::hardware_block::EncoderToken::SwH264),
            on_capture_spawned: Arc::new(move |k: &str| {
                l3.spawned_callbacks.lock().unwrap().push(k.to_string())
            }),
            set_live_shed: Some(Arc::new(move |s: bool| {
                l1.live_shed.lock().unwrap().push(s);
                if let Some(h) = &hook {
                    h(s);
                }
            })),
            set_disabled_cameras: Some(Arc::new(move |k: &[String]| {
                l2.disabled.lock().unwrap().push(k.to_vec())
            })),
            replay: None,
        };
        let (reading, clock, l4) = (self.reading.clone(), self.clock.clone(), log.clone());
        let power: Option<PowerSampleFn> = self.power_sample.then(|| -> PowerSampleFn {
            Arc::new(move || {
                let (reading, clock, l4) = (reading.clone(), clock.clone(), l4.clone());
                Box::pin(async move {
                    l4.sample_calls.fetch_add(1, Ordering::SeqCst);
                    clock.advance(1000);
                    *reading.lock().unwrap()
                })
            })
        });
        let (l5, l6) = (log.clone(), log.clone());
        let upload = self.upload_override.clone().or_else(|| {
            self.upload.then(|| xorgate_cloud::s3::UploadOptions {
                put: Arc::new(NoPut),
                is_online: Arc::new(|| true),
            })
        });
        RecordingManager::new(RecordingManagerOpts {
            device_id: "dev-1".into(),
            root: self.root(),
            shm_dir: self.home.join("shm"),
            settings_path: self.settings_path(),
            time_source: self.time_source.clone(),
            upload,
            video: self.video.then_some(video),
            sample_groups: Arc::new(|| Box::pin(async { IndexMap::new() })),
            power_sample: power,
            boot_sources: Box::new(sources),
            on_status: Arc::new(move |s: &RecordingStatus| {
                l5.statuses.lock().unwrap().push(s.clone())
            }),
            on_event: Arc::new(move |lv: ActivityLevel, m: &str| {
                l6.events.lock().unwrap().push((lv, m.to_string()))
            }),
            clock: self.clock.clone(),
            backend: self.backend_override.clone().unwrap_or_else(|| {
                Arc::new(FakeBackend {
                    log: log.clone(),
                    orphans: self.orphans,
                    backfilled: self.backfilled,
                })
            }),
            poll_ms: Some(self.poll_ms),
            shed_sample_ms: Some(self.shed_sample_ms),
            time_probe_timeout_ms: self.time_probe_timeout_ms,
            clock_trusted: self.clock_trusted.clone(),
        })
    }

    pub fn live_cams(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .log
            .spawns
            .lock()
            .unwrap()
            .iter()
            .filter(|s| !s.stopped)
            .map(|s| s.cam.clone())
            .collect();
        v.sort();
        v
    }
    pub fn spawns(&self) -> Vec<Spawn> {
        self.log.spawns.lock().unwrap().clone()
    }
    pub fn events(&self) -> Vec<String> {
        self.log
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|(_, m)| m.clone())
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct NoPut;
impl xorgate_cloud::s3::ObjectPut for NoPut {
    fn put<'a>(
        &'a self,
        _key: &'a str,
        _body: bytes::Bytes,
        _content_type: &'a str,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> xorgate_cloud::s3::PutFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}
