//! The KVS WebRTC master supervisor — the runtime half of
//! `ts:src/agent/collectors/media.ts` (`createMediaCollector`, :83-380).
//!
//! One `xorgate-kvs-webrtc` master per manifest channel so the device streams
//! live video to the console's viewer. Every decision (lazy start, restart
//! ladder, spawn guard, shed / disabled / restart / stop, every `[media …]`
//! line) is `xorgate_core::media_ladder::MediaLadder`; this module executes
//! its [`Effect`]s in order:
//!
//! - `Spawn` → `streamChannel` (`xorgate_hal::kvs_runtime::master_spec`)
//!   through `xorgate-hal::child` (own process group, both pipes drained line
//!   by line, `Done` last); no `cwd` (inherited, like execa: the runtime's
//!   signalling cache lands in the agent's working directory);
//! - `Signal` → SIGINT to the master (never SIGKILL from here, P11);
//! - `Arm` → a tokio timer that calls back into the ladder;
//! - `Log` → `console.log/warn/error` = `info!/warn!/error!`.
//!
//! stdout/stderr are ALWAYS drained (a full 64 KB pipe blocks the master) and
//! forwarded as `[media <channel>] <line>` (stdout `info`, stderr `error`;
//! blank lines skipped); never parsed. Tape: `child_spawn` / `child_line` /
//! `child_signal` / `child_exit` with `role:"kvs"`, key = the stream key.
//!
//! The ladder's clock is the tokio monotonic clock (P8; `tokio::time::pause`
//! drives it in tests). Lock rule: the supervisor's mutex is held while the
//! injected guard runs (`spawn()` asks it synchronously, like the TS) — the
//! guard must not call back into the supervisor.
//!
//! Children still alive when the tokio runtime is dropped at process exit are
//! SIGKILLed by `kill_on_drop` (hal child); the TS equivalent is execa's
//! `cleanup` SIGTERM at Node exit. The runtime has no SIGTERM handler, so
//! both are a default-action death right after `stop()`'s SIGINT (evidence
//! file, ambiguity A3).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::mpsc;
use tracing::{error, info, warn};
use xorgate_core::media_ladder::{
    Effect, LadderOpts, LogLevel, MediaLadder, RuntimeDetect, SpawnEnv, TimerKind,
};
use xorgate_core::stream_guard::StreamGuard;
use xorgate_core::telemetry::TelemetryMedia;
use xorgate_hal::child::{self, ChildEvent, ChildHandle, ChildSpec};
use xorgate_hal::io::{ChildExitRec, ChildSpawnRec, TapeSink, ROLE_KVS};
use xorgate_hal::kvs_manifest_io::load_manifest;
use xorgate_hal::kvs_runtime::{detect_runtime, master_spec, KvsRuntimeEnv};

use super::config::MediaConfig;
use super::reconcile::resolve_channel_pipeline;

/// The spawn guard (`MediaConfig.streamGuard`, built by WP1 over the
/// reconcile statuses with `stream_guard_verdict`).
pub type GuardFn = Arc<dyn Fn(&str) -> StreamGuard + Send + Sync>;

/// A running master as the supervisor sees it (`StreamHandle`).
pub trait MasterProc: Send + Sync + 'static {
    fn pid(&self) -> Option<u32>;
    /// `stop(signal)`: a no-op once the child is reaped.
    fn signal(&self, sig: i32);
}

impl MasterProc for ChildHandle {
    fn pid(&self) -> Option<u32> {
        ChildHandle::pid(self)
    }
    fn signal(&self, sig: i32) {
        ChildHandle::signal(self, sig);
    }
}

/// The supervisor's edges: runtime detection and the child spawn (fakes in tests).
pub trait MasterBackend: Send + Sync + 'static {
    /// `detectRuntime(binaryPath)`.
    fn detect(&self, binary_path: &str) -> RuntimeDetect;
    /// `runStream(spec…)`.
    fn spawn(&self, spec: &ChildSpec)
        -> (Arc<dyn MasterProc>, mpsc::UnboundedReceiver<ChildEvent>);
}

/// Production: [`detect_runtime`] + [`child::spawn`].
#[derive(Debug, Default, Clone, Copy)]
pub struct RealMasterBackend;

impl MasterBackend for RealMasterBackend {
    fn detect(&self, binary_path: &str) -> RuntimeDetect {
        detect_runtime(binary_path)
    }
    fn spawn(
        &self,
        spec: &ChildSpec,
    ) -> (Arc<dyn MasterProc>, mpsc::UnboundedReceiver<ChildEvent>) {
        let (h, rx) = child::spawn(spec);
        (Arc::new(h), rx)
    }
}

/// One spawned master.
struct Live {
    generation: u64,
    stream_key: String,
    proc_: Arc<dyn MasterProc>,
    /// The agent signalled it (its exit is not `external` on the tape).
    signalled: bool,
}

struct State {
    ladder: MediaLadder,
    live: HashMap<String, Live>,
    generation: u64,
}

struct Inner {
    cfg: MediaConfig,
    guard: GuardFn,
    tape: Arc<dyn TapeSink>,
    backend: Arc<dyn MasterBackend>,
    origin: tokio::time::Instant,
    st: Mutex<State>,
}

/// `createMediaCollector(cfg)` (`media.ts:83`): the `MediaSupervisor` interface.
#[derive(Clone)]
pub struct MediaSupervisor {
    inner: Arc<Inner>,
}

/// [`SpawnEnv`] over the injected guard + `resolveChannelPipeline`.
struct Env<'a> {
    cfg: &'a MediaConfig,
    guard: &'a GuardFn,
}

impl SpawnEnv for Env<'_> {
    fn guard(&self, stream_key: &str) -> StreamGuard {
        (self.guard)(stream_key)
    }
    fn resolve_pipeline(&self, stream_key: &str) -> Option<String> {
        resolve_channel_pipeline(
            Path::new(&self.cfg.pipeline_dir),
            stream_key,
            self.cfg.pipeline_path.as_deref().map(Path::new),
        )
        .map(|p| p.to_string_lossy().into_owned())
    }
}

impl MediaSupervisor {
    /// `createMediaCollector(cfg)` with the real runtime.
    pub fn new(cfg: MediaConfig, guard: GuardFn, tape: Arc<dyn TapeSink>) -> Self {
        Self::with_backend(cfg, guard, tape, Arc::new(RealMasterBackend))
    }

    /// [`Self::new`] with the edges injected (tests).
    pub fn with_backend(
        cfg: MediaConfig,
        guard: GuardFn,
        tape: Arc<dyn TapeSink>,
        backend: Arc<dyn MasterBackend>,
    ) -> Self {
        let ladder = MediaLadder::new(LadderOpts {
            lib_dir: cfg.lib_dir.clone(),
            manifest_path: cfg.manifest_path.clone(),
            source: cfg.source.clone(),
        });
        Self {
            inner: Arc::new(Inner {
                cfg,
                guard,
                tape,
                backend,
                origin: tokio::time::Instant::now(),
                st: Mutex::new(State {
                    ladder,
                    live: HashMap::new(),
                    generation: 0,
                }),
            }),
        }
    }

    /// `read()` (`media.ts:292-299`): the payload's `media` block; the first
    /// call kicks off the lazy start (detect + manifest load + spawns) on a
    /// task and returns the IDLE block. Needs a tokio runtime.
    pub fn read(&self) -> TelemetryMedia {
        let r = self.inner.lock().ladder.read();
        if r.kick_start {
            let me = self.inner.clone();
            tokio::spawn(async move { me.start().await });
        }
        r.media
    }

    /// `stop()` (`media.ts:366-378`): SIGINT every master, cancel restarts.
    pub fn stop(&self) {
        self.inner.run(|l, _, _| l.stop());
    }

    /// `restartChannel(streamKey)` (`media.ts:354-365`): bounce a live
    /// master after its capture daemon respawned.
    pub fn restart_channel(&self, key: &str) {
        self.inner.run(|l, _, _| l.restart_channel(key));
    }

    /// `setShed(shed)` (`media.ts:300-329`): the ONLY live-parking actuator
    /// (the decision is WP1's choke point).
    pub fn set_shed(&self, shed: bool) {
        self.inner.run(|l, now, env| l.set_shed(shed, now, env));
    }

    /// `setDisabledStreams(streamKeys)` (`media.ts:330-353`).
    pub fn set_disabled_streams(&self, keys: &[String]) {
        self.inner
            .run(|l, now, env| l.set_disabled_streams(keys, now, env));
    }

    /// The live masters' pids by channel name (diagnostics / gate checks;
    /// never `kill` them yourself).
    pub fn master_pids(&self) -> BTreeMap<String, u32> {
        self.inner
            .lock()
            .live
            .iter()
            .filter_map(|(ch, l)| l.proc_.pid().map(|p| (ch.clone(), p)))
            .collect()
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn now(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    /// Run one ladder step under the lock and execute its effects in order.
    fn run(
        self: &Arc<Self>,
        step: impl FnOnce(&mut MediaLadder, u64, &dyn SpawnEnv) -> Vec<Effect>,
    ) {
        let mut st = self.lock();
        let env = Env {
            cfg: &self.cfg,
            guard: &self.guard,
        };
        let fx = step(&mut st.ladder, self.now(), &env);
        self.execute(&mut st, fx);
    }

    /// `start()` (`media.ts:113-151`) after `read()` set `started`.
    async fn start(self: Arc<Self>) {
        let detect = self.backend.detect(&self.cfg.binary_path);
        let manifest_path = self.cfg.manifest_path.clone();
        self.run(|l, now, env| {
            l.start(
                &detect,
                || load_manifest(Path::new(&manifest_path)),
                now,
                env,
            )
        });
    }

    fn execute(self: &Arc<Self>, st: &mut State, fx: Vec<Effect>) {
        for e in fx {
            match e {
                Effect::Log { level, line } => match level {
                    LogLevel::Log => info!("{line}"),
                    LogLevel::Warn => warn!("{line}"),
                    LogLevel::Error => error!("{line}"),
                },
                Effect::Spawn {
                    channel,
                    stream_key,
                    pipeline,
                } => self.spawn_master(st, channel, stream_key, pipeline),
                Effect::Signal { channel } => {
                    if let Some(l) = st.live.get_mut(&channel) {
                        l.proc_.signal(libc_sigint());
                        l.signalled = true;
                        self.tape.child_signal(
                            ROLE_KVS,
                            &l.stream_key,
                            l.proc_.pid().unwrap_or(0),
                            libc_sigint(),
                        );
                    }
                }
                Effect::Arm {
                    channel,
                    kind,
                    id,
                    delay_ms,
                } => {
                    let me = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        me.fire(&channel, kind, id);
                    });
                }
            }
        }
    }

    fn fire(self: &Arc<Self>, channel: &str, kind: TimerKind, id: u64) {
        self.run(|l, now, env| l.on_timer(channel, kind, id, now, env));
    }

    /// `streamChannel(…)` (`media.ts:185-217`, `kvs-webrtc.ts:85-109`).
    fn spawn_master(
        self: &Arc<Self>,
        st: &mut State,
        channel: String,
        stream_key: String,
        pipeline: Option<String>,
    ) {
        let Some(m) = st.ladder.manifest() else {
            return; // unreachable: the ladder spawns only with a manifest
        };
        let env = KvsRuntimeEnv {
            region: m.region.clone(),
            credential_endpoint: m.credential_endpoint.clone(),
            role_alias: m.role_alias.clone(),
            thing_name: m.device_id.clone(),
            cert_path: self.cfg.cert_path.clone(),
            key_path: self.cfg.key_path.clone(),
            ca_cert_path: self.cfg.ca_cert_path.clone(),
            lib_dir: self.cfg.lib_dir.clone(),
            pipeline_path: pipeline,
            log_level: Some(self.cfg.log_level.clone()),
            idle_teardown_secs: Some(self.cfg.idle_teardown_secs.clone()),
        };
        let inherited_ld = std::env::var("LD_LIBRARY_PATH").ok();
        let spec = master_spec(
            &channel,
            &self.cfg.media_type,
            &self.cfg.source,
            &self.cfg.binary_path,
            &env,
            inherited_ld.as_deref(),
        );
        let (proc_, rx) = self.backend.spawn(&spec);
        st.generation += 1;
        let generation = st.generation;
        self.tape.child_spawn(&ChildSpawnRec {
            role: ROLE_KVS.to_string(),
            key: stream_key.clone(),
            argv: spec.argv(),
            env: spec.env.iter().cloned().collect(),
            pid: proc_.pid().unwrap_or(0),
            nice: None,
        });
        st.live.insert(
            channel.clone(),
            Live {
                generation,
                stream_key: stream_key.clone(),
                proc_: proc_.clone(),
                signalled: false,
            },
        );
        let me = self.clone();
        tokio::spawn(async move { me.pump(channel, stream_key, generation, proc_, rx).await });
    }

    /// Drain one master (`onStdout`/`onStderr`, `media.ts:206-215`) until its
    /// `done` (`:231` → `onExit`). Holds the process handle until the exit so
    /// a stopped master is never killed by dropping it.
    async fn pump(
        self: Arc<Self>,
        channel: String,
        stream_key: String,
        generation: u64,
        proc_: Arc<dyn MasterProc>,
        mut rx: mpsc::UnboundedReceiver<ChildEvent>,
    ) {
        let done = loop {
            match rx.recv().await {
                Some(ChildEvent::Stdout(line)) => {
                    self.tape.child_line(ROLE_KVS, &stream_key, 1, &line);
                    if !xorgate_core::js::trim(&line).is_empty() {
                        info!("[media {channel}] {line}");
                    }
                }
                Some(ChildEvent::Stderr(line)) => {
                    self.tape.child_line(ROLE_KVS, &stream_key, 2, &line);
                    if !xorgate_core::js::trim(&line).is_empty() {
                        error!("[media {channel}] {line}");
                    }
                }
                Some(ChildEvent::Done(d)) => break Some(d),
                None => break None,
            }
        };
        let (code, signal) = done.map_or((None, None), |d| (d.exit_code, d.signal));
        let mut st = self.lock();
        let signalled = match st.live.get(&channel) {
            Some(l) if l.generation == generation => {
                let s = l.signalled;
                st.live.remove(&channel);
                s
            }
            _ => true,
        };
        self.tape.child_exit(&ChildExitRec {
            role: ROLE_KVS.to_string(),
            key: stream_key,
            pid: proc_.pid().unwrap_or(0),
            code,
            signal,
            external: !signalled,
        });
        let fx = st.ladder.on_exit(&channel, code, self.now());
        self.execute(&mut st, fx);
        drop(st);
        drop(proc_);
    }
}

fn libc_sigint() -> i32 {
    nix::libc::SIGINT
}
