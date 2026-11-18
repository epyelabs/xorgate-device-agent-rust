//! The KVS master supervisor's decisions — the pure half of
//! `ts:src/agent/collectors/media.ts` (`createMediaCollector`, :83-380).
//!
//! One `xorgate-kvs-webrtc` master per manifest channel: lazy start on the
//! first `read()`, a per-channel restart ladder (stable 15 s resets backoff,
//! backoff 2 → 30 s, 3 quick failures → `failed` + parked 60 s), the spawn
//! guard (`no-camera` / `no-pipeline`, parked on the slow retry), and the
//! controls `setShed` / `setDisabledStreams` / `restartChannel` / `stop`.
//!
//! [`MediaLadder`] holds the TS closure's `let`s and the `channels` Map. Each
//! method mirrors one TS function and returns the side effects IN THE TS
//! ORDER as [`Effect`]s; the agent (`xorgate-agent::media::supervisor`)
//! executes them: spawns the child, sends SIGINT, arms the timer, prints the
//! line. Every operator-grepped `[media …]` string is built here.
//!
//! Timers are modelled like `setTimeout`/`clearTimeout`: [`Effect::Arm`]
//! hands out an id; a timer fires (the executor calls
//! [`MediaLadder::on_timer`]) unless the ladder cleared it first, exactly
//! where the TS calls `clearTimeout`.
//!
//! Deliberate deviation (P8): `c.startedAt = Date.now()` / `Date.now() -
//! c.startedAt` (`:184`, `:247`) are measured on the MONOTONIC clock (`now`
//! arguments are mono ms) so a wall-clock step cannot fake a stable run.

use std::collections::BTreeSet;

use crate::kvs_manifest::KvsManifest;
use crate::stream_guard::StreamGuard;
use crate::telemetry::{BoolLeaf, MediaStream, TelemetryMedia};

/// `STABLE_MS` (`media.ts:25`): a master alive this long is up; resets backoff.
pub const STABLE_MS: u64 = 15_000;
/// `BASE_BACKOFF_MS` (`:26`), run-master.sh's `sleep 2`.
pub const BASE_BACKOFF_MS: u64 = 2_000;
/// `MAX_BACKOFF_MS` (`:27`).
pub const MAX_BACKOFF_MS: u64 = 30_000;
/// `MAX_QUICK_FAILURES` (`:28`): quick crashes before the channel parks as failed.
pub const MAX_QUICK_FAILURES: u32 = 3;
/// `PARKED_RETRY_MS` (`:29`): parked channels re-attempt this often.
pub const PARKED_RETRY_MS: u64 = 60_000;

/// `MediaStream["state"]` (`telemetry.ts:100`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelState {
    Starting,
    Running,
    Retrying,
    Failed,
    Stopped,
    NoCamera,
    NoPipeline,
}

impl ChannelState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Retrying => "retrying",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::NoCamera => "no-camera",
            Self::NoPipeline => "no-pipeline",
        }
    }

    fn from_guard(g: StreamGuard) -> Self {
        match g {
            StreamGuard::Ok => Self::Starting,
            StreamGuard::NoCamera => Self::NoCamera,
            StreamGuard::NoPipeline => Self::NoPipeline,
        }
    }
}

/// Which `console` method printed a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
    /// `console.log`
    Log,
    /// `console.warn`
    Warn,
    /// `console.error`
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// The two per-channel `setTimeout` slots (`stableTimer`, `restartTimer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimerKind {
    Stable,
    Restart,
}

/// One side effect, in TS order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Print `line` with `console[level]`.
    Log { level: LogLevel, line: String },
    /// `streamChannel(channel, …, {…, pipelinePath})` (`:185-217`): launch a
    /// master. The ladder now considers the channel to hold a handle; the
    /// executor reports its exit through [`MediaLadder::on_exit`].
    Spawn {
        channel: String,
        stream_key: String,
        pipeline: Option<String>,
    },
    /// `c.handle.stop("SIGINT")` — the runtime's graceful shutdown; never SIGKILL (P11).
    Signal { channel: String },
    /// `setTimeout(…, delay_ms)`; call [`MediaLadder::on_timer`] with `id` when it fires.
    Arm {
        channel: String,
        kind: TimerKind,
        id: u64,
        delay_ms: u64,
    },
}

/// `detectRuntime()`'s result (`ts:src/hardware/kvs-webrtc.ts:18-24`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeDetect {
    pub available: bool,
    /// `gst-launch-1.0` is on PATH (a hint only).
    pub gstreamer: bool,
    /// Why `available` is false.
    pub reason: Option<String>,
}

/// What `spawn()` asks the world: the stream guard and the pipeline
/// resolution (`resolveChannelPipeline`, an `existsSync`), injected.
pub trait SpawnEnv {
    /// `cfg.streamGuard?.(streamKey) ?? "ok"`.
    fn guard(&self, stream_key: &str) -> StreamGuard;
    /// `resolveChannelPipeline(cfg.pipelineDir, streamKey, cfg.pipelinePath)`.
    fn resolve_pipeline(&self, stream_key: &str) -> Option<String>;
}

/// The bits of `MediaConfig` the decisions print.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LadderOpts {
    /// `cfg.libDir` (the parked line's hint).
    pub lib_dir: String,
    /// `cfg.manifestPath` (the no-manifest warning).
    pub manifest_path: String,
    /// `cfg.source` (the startup line).
    pub source: String,
}

/// `ChannelSupervisor` (`media.ts:31-43`); `handle` is `has_handle`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Channel {
    stream_key: String,
    channel_name: String,
    disabled: bool,
    state: ChannelState,
    has_handle: bool,
    started_at: u64,
    backoff_ms: u64,
    quick_failures: u32,
    stable_timer: Option<u64>,
    restart_timer: Option<u64>,
}

/// What a `read()` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOutcome {
    /// The payload's `media` block.
    pub media: TelemetryMedia,
    /// This read kicked off the lazy start: run `detectRuntime` and call
    /// [`MediaLadder::start`] (`void start()`, non-blocking).
    pub kick_start: bool,
}

/// The supervisor's state (`createMediaCollector`'s closure, `:84-92`).
#[derive(Debug, Clone, Default)]
pub struct MediaLadder {
    opts: LadderOpts,
    started: bool,
    stopped: bool,
    shed: bool,
    disabled_streams: BTreeSet<String>,
    /// D86: stream keys whose capture source is down (the capture
    /// supervisor's start-failure back-off); their masters are held.
    source_absent: BTreeSet<String>,
    warned: bool,
    manifest: Option<KvsManifest>,
    /// The `channels` Map, in insertion order, keyed by channel name.
    channels: Vec<Channel>,
    next_timer: u64,
    /// Timers armed and neither fired nor cleared.
    live_timers: BTreeSet<u64>,
}

impl MediaLadder {
    pub fn new(opts: LadderOpts) -> Self {
        Self {
            opts,
            ..Default::default()
        }
    }

    /// The manifest loaded by [`Self::start`] (the spawn env's region,
    /// credential endpoint, role alias and thing name).
    pub fn manifest(&self) -> Option<&KvsManifest> {
        self.manifest.as_ref()
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// The channel's stream key (for tape records).
    pub fn stream_key_of(&self, channel: &str) -> Option<&str> {
        self.find(channel)
            .map(|i| self.channels[i].stream_key.as_str())
    }

    /// The channel's current state (tests / diagnostics).
    pub fn state_of(&self, channel: &str) -> Option<ChannelState> {
        self.find(channel).map(|i| self.channels[i].state)
    }

    fn find(&self, channel: &str) -> Option<usize> {
        self.channels.iter().position(|c| c.channel_name == channel)
    }

    /// `IDLE` (`media.ts:77-81`).
    pub fn idle() -> TelemetryMedia {
        TelemetryMedia::idle()
    }

    /// `snapshot()` (`media.ts:101-109`).
    pub fn snapshot(&self) -> TelemetryMedia {
        let streams: Vec<MediaStream> = self
            .channels
            .iter()
            .map(|c| MediaStream {
                stream_key: c.stream_key.clone(),
                channel_name: c.channel_name.clone(),
                state: c.state.as_str().to_string(),
                recording: None,
            })
            .collect();
        let live = streams
            .iter()
            .any(|s| s.state == "running" || s.state == "starting");
        TelemetryMedia {
            kvs: BoolLeaf { value: live },
            recording: BoolLeaf { value: false },
            streams,
        }
    }

    /// `read()` (`media.ts:292-299`): IDLE once stopped; the first call marks
    /// the start (`started = true` before the first await, `:114`) and
    /// returns IDLE; later calls the snapshot.
    pub fn read(&mut self) -> ReadOutcome {
        if self.stopped {
            return ReadOutcome {
                media: Self::idle(),
                kick_start: false,
            };
        }
        if !self.started {
            self.started = true;
            return ReadOutcome {
                media: Self::idle(),
                kick_start: true,
            };
        }
        ReadOutcome {
            media: self.snapshot(),
            kick_start: false,
        }
    }

    fn warn_once(&mut self, msg: &str, out: &mut Vec<Effect>) {
        // `warnOnce` (`media.ts:94-99`).
        if !self.warned {
            out.push(Effect::Log {
                level: LogLevel::Warn,
                line: format!("[media] {msg}"),
            });
            self.warned = true;
        }
    }

    /// The rest of `start()` (`media.ts:113-151`) once `detectRuntime` has
    /// resolved: `load_manifest` is `loadManifest(cfg.manifestPath)`, called
    /// only when the runtime is available.
    pub fn start(
        &mut self,
        detect: &RuntimeDetect,
        load_manifest: impl FnOnce() -> Option<KvsManifest>,
        now: u64,
        env: &dyn SpawnEnv,
    ) -> Vec<Effect> {
        let mut out = Vec::new();
        self.started = true;
        if !detect.available {
            let reason = detect
                .reason
                .clone()
                .unwrap_or_else(|| "media runtime unavailable".to_string());
            self.warn_once(&reason, &mut out);
            return out;
        }
        if !detect.gstreamer {
            out.push(Effect::Log {
                level: LogLevel::Warn,
                line: "[media] gst-launch-1.0 not found; GStreamer may be missing (continuing)"
                    .to_string(),
            });
        }
        self.manifest = load_manifest();
        let Some(manifest) = self.manifest.clone() else {
            let msg = format!(
                "no KVS manifest at {}; media disabled",
                self.opts.manifest_path
            );
            self.warn_once(&msg, &mut out);
            return out;
        };
        if manifest.channels.is_empty() {
            self.warn_once("KVS manifest has no channels; nothing to stream", &mut out);
            return out;
        }
        out.push(Effect::Log {
            level: LogLevel::Log,
            line: format!(
                "[media] starting {} KVS master(s) (region={}, source={}) for {}",
                manifest.channels.len(),
                manifest.region,
                self.opts.source,
                manifest.device_id
            ),
        });
        for ch in &manifest.channels {
            let disabled = self.disabled_streams.contains(&ch.stream_key);
            let entry = Channel {
                stream_key: ch.stream_key.clone(),
                channel_name: ch.channel_name.clone(),
                disabled,
                state: if self.shed || disabled {
                    ChannelState::Stopped
                } else {
                    ChannelState::Starting
                },
                has_handle: false,
                started_at: 0,
                backoff_ms: BASE_BACKOFF_MS,
                quick_failures: 0,
                stable_timer: None,
                restart_timer: None,
            };
            // `channels.set(name, …)`: a repeated name replaces the entry in place.
            match self.find(&ch.channel_name) {
                Some(i) => self.channels[i] = entry,
                None => self.channels.push(entry),
            }
            self.spawn(&ch.channel_name, now, env, &mut out);
        }
        out
    }

    fn arm(&mut self, idx: usize, kind: TimerKind, delay_ms: u64, out: &mut Vec<Effect>) -> u64 {
        self.next_timer += 1;
        let id = self.next_timer;
        self.live_timers.insert(id);
        out.push(Effect::Arm {
            channel: self.channels[idx].channel_name.clone(),
            kind,
            id,
            delay_ms,
        });
        id
    }

    fn clear(&mut self, id: Option<u64>) {
        if let Some(id) = id {
            self.live_timers.remove(&id);
        }
    }

    /// `spawn(channelName)` (`media.ts:153-232`).
    fn spawn(&mut self, channel: &str, now: u64, env: &dyn SpawnEnv, out: &mut Vec<Effect>) {
        let Some(i) = self.find(channel) else { return };
        if self.stopped || self.shed || self.channels[i].disabled || self.manifest.is_none() {
            return;
        }
        let guard = env.guard(&self.channels[i].stream_key);
        if guard != StreamGuard::Ok {
            let parked = ChannelState::from_guard(guard);
            if self.channels[i].state != parked {
                let why = if guard == StreamGuard::NoCamera {
                    " (model expects a camera that did not enumerate)"
                } else {
                    " (no pipeline file rendered/resolved)"
                };
                out.push(Effect::Log {
                    level: LogLevel::Warn,
                    line: format!(
                        "[media {channel}] not starting a master: {}{why}",
                        guard.as_str()
                    ),
                });
            }
            self.channels[i].state = parked;
            self.schedule_restart(i, Some(PARKED_RETRY_MS), out);
            return;
        }
        if self.source_absent.contains(&self.channels[i].stream_key) {
            // D86: no capture source to stream from — hold the master (no
            // timer: `set_source_absent(…, false)` starts it).
            self.channels[i].state = ChannelState::Failed;
            return;
        }
        let pipeline = env.resolve_pipeline(&self.channels[i].stream_key);
        let c = &mut self.channels[i];
        c.state = ChannelState::Starting;
        c.started_at = now;
        c.has_handle = true;
        out.push(Effect::Spawn {
            channel: c.channel_name.clone(),
            stream_key: c.stream_key.clone(),
            pipeline,
        });
        let id = self.arm(i, TimerKind::Stable, STABLE_MS, out);
        self.channels[i].stable_timer = Some(id);
    }

    /// `scheduleRestart(c, delayMs = c.backoffMs)` (`media.ts:283-289`).
    fn schedule_restart(&mut self, i: usize, delay_ms: Option<u64>, out: &mut Vec<Effect>) {
        if self.stopped {
            return;
        }
        let delay = delay_ms.unwrap_or(self.channels[i].backoff_ms);
        let id = self.arm(i, TimerKind::Restart, delay, out);
        self.channels[i].restart_timer = Some(id);
    }

    /// A timer armed by an [`Effect::Arm`] fired. Ignored when the ladder
    /// cleared it meanwhile (`clearTimeout`).
    ///
    /// - stable (`media.ts:221-229`): promote to `running`, reset the ladder;
    /// - restart (`:285-288`): `c.restartTimer = null; spawn(c.channelName)`.
    pub fn on_timer(
        &mut self,
        channel: &str,
        kind: TimerKind,
        id: u64,
        now: u64,
        env: &dyn SpawnEnv,
    ) -> Vec<Effect> {
        let mut out = Vec::new();
        if !self.live_timers.remove(&id) {
            return out;
        }
        match kind {
            TimerKind::Stable => {
                if self.stopped {
                    return out;
                }
                if let Some(i) = self.find(channel) {
                    let c = &mut self.channels[i];
                    if c.has_handle {
                        c.state = ChannelState::Running;
                        c.quick_failures = 0;
                        c.backoff_ms = BASE_BACKOFF_MS;
                    }
                }
            }
            TimerKind::Restart => {
                // `c.restartTimer = null` on the captured channel — whatever
                // the slot holds now, like the TS.
                if let Some(i) = self.find(channel) {
                    self.channels[i].restart_timer = None;
                }
                self.spawn(channel, now, env, &mut out);
            }
        }
        out
    }

    /// `onExit(channelName, exitCode)` (`media.ts:234-281`): the master's
    /// `done` resolved. `exit_code` is `null` (`None`) when it was killed by a
    /// signal or never spawned.
    pub fn on_exit(&mut self, channel: &str, exit_code: Option<i32>, now: u64) -> Vec<Effect> {
        let mut out = Vec::new();
        let Some(i) = self.find(channel) else {
            return out;
        };
        let stable = self.channels[i].stable_timer.take();
        self.clear(stable);
        self.channels[i].has_handle = false;
        if self.stopped || self.shed || self.channels[i].disabled {
            self.channels[i].state = ChannelState::Stopped;
            return out;
        }
        if self.source_absent.contains(&self.channels[i].stream_key) {
            // D86: held until its capture source is up; no restart ladder.
            self.channels[i].state = ChannelState::Failed;
            return out;
        }
        let code = exit_code.map_or_else(|| "?".to_string(), |c| c.to_string());
        let ran_ms = now.saturating_sub(self.channels[i].started_at);
        if ran_ms >= STABLE_MS {
            out.push(Effect::Log {
                level: LogLevel::Warn,
                line: format!("[media {channel}] master exited (code {code}); restarting"),
            });
            let c = &mut self.channels[i];
            c.quick_failures = 0;
            c.backoff_ms = BASE_BACKOFF_MS;
            self.schedule_restart(i, None, &mut out);
            return out;
        }
        self.channels[i].quick_failures += 1;
        if self.channels[i].quick_failures >= MAX_QUICK_FAILURES {
            self.channels[i].state = ChannelState::Failed;
            out.push(Effect::Log {
                level: LogLevel::Error,
                line: format!(
                    "[media {channel}] master exited (code {code}) before stabilizing {}x; \
                     parking for {}s. Check: KVS SDK .so libs in {}, GStreamer install, and IoT creds.",
                    self.channels[i].quick_failures,
                    PARKED_RETRY_MS / 1000,
                    self.opts.lib_dir
                ),
            });
            let c = &mut self.channels[i];
            c.quick_failures = 0;
            c.backoff_ms = BASE_BACKOFF_MS;
            self.schedule_restart(i, Some(PARKED_RETRY_MS), &mut out);
            return out;
        }
        self.channels[i].state = ChannelState::Retrying;
        out.push(Effect::Log {
            level: LogLevel::Warn,
            line: format!(
                "[media {channel}] master exited (code {code}); retry {}/{} in {}ms",
                self.channels[i].quick_failures, MAX_QUICK_FAILURES, self.channels[i].backoff_ms
            ),
        });
        self.schedule_restart(i, None, &mut out);
        let c = &mut self.channels[i];
        c.backoff_ms = (c.backoff_ms * 2).min(MAX_BACKOFF_MS);
        out
    }

    /// `setShed(on)` (`media.ts:300-329`): load-shedding rung 1 / the GPS-LTE
    /// gate parks EVERY master (SIGINT, no respawn) until released.
    pub fn set_shed(&mut self, on: bool, now: u64, env: &dyn SpawnEnv) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.stopped || self.shed == on {
            return out;
        }
        self.shed = on;
        if on {
            for i in 0..self.channels.len() {
                let (st, rt) = (
                    self.channels[i].stable_timer.take(),
                    self.channels[i].restart_timer.take(),
                );
                self.clear(st);
                self.clear(rt);
                self.channels[i].state = ChannelState::Stopped;
                if self.channels[i].has_handle {
                    out.push(Effect::Signal {
                        channel: self.channels[i].channel_name.clone(),
                    });
                }
            }
            if !self.channels.is_empty() {
                out.push(Effect::Log {
                    level: LogLevel::Warn,
                    line: "[media] live masters parked (power policy)".to_string(),
                });
            }
            return out;
        }
        for i in 0..self.channels.len() {
            if self.channels[i].disabled {
                continue;
            }
            self.channels[i].quick_failures = 0;
            self.channels[i].backoff_ms = BASE_BACKOFF_MS;
            if !self.channels[i].has_handle && self.channels[i].restart_timer.is_none() {
                let name = self.channels[i].channel_name.clone();
                self.spawn(&name, now, env, &mut out);
            }
        }
        if !self.channels.is_empty() {
            out.push(Effect::Log {
                level: LogLevel::Log,
                line: "[media] live masters released (power policy cleared)".to_string(),
            });
        }
        out
    }

    /// `setDisabledStreams(streamKeys)` (`media.ts:330-353`): park the masters
    /// of cameras turned off in `recording.video.cameras`; remembered even
    /// before the lazy start.
    pub fn set_disabled_streams(
        &mut self,
        stream_keys: &[String],
        now: u64,
        env: &dyn SpawnEnv,
    ) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.stopped {
            return out;
        }
        self.disabled_streams = stream_keys.iter().cloned().collect();
        for i in 0..self.channels.len() {
            let want = self.disabled_streams.contains(&self.channels[i].stream_key);
            if want == self.channels[i].disabled {
                continue;
            }
            self.channels[i].disabled = want;
            let name = self.channels[i].channel_name.clone();
            if want {
                let (st, rt) = (
                    self.channels[i].stable_timer.take(),
                    self.channels[i].restart_timer.take(),
                );
                self.clear(st);
                self.clear(rt);
                self.channels[i].state = ChannelState::Stopped;
                if self.channels[i].has_handle {
                    out.push(Effect::Signal {
                        channel: name.clone(),
                    });
                }
                out.push(Effect::Log {
                    level: LogLevel::Warn,
                    line: format!("[media {name}] master stopped (camera disabled in settings)"),
                });
            } else {
                self.channels[i].quick_failures = 0;
                self.channels[i].backoff_ms = BASE_BACKOFF_MS;
                out.push(Effect::Log {
                    level: LogLevel::Log,
                    line: format!("[media {name}] camera re-enabled in settings; starting master"),
                });
                if !self.shed
                    && !self.channels[i].has_handle
                    && self.channels[i].restart_timer.is_none()
                {
                    self.spawn(&name, now, env, &mut out);
                }
            }
        }
        out
    }

    /// D86 (F-DA21-KVSLOOP, Rust-only): the capture source of `stream_key`
    /// went down (`absent`: the capture supervisor parked the camera after a
    /// start-failure streak) or came back up (its probe ran stable, or the
    /// capture run went away). The TS has no such input: its master keeps
    /// being bounced by every capture respawn and, while viewed, spins on the
    /// missing socket (≈91% of a core on bench 2).
    ///
    /// - down: the channel's timers are cleared, a live master is SIGINTed,
    ///   the state is `failed`, and [`Self::spawn`] holds it (no timer) until
    ///   the source is up; one line per transition;
    /// - up: a held channel (not disabled, not shed, no handle, no pending
    ///   restart) starts at once with a fresh ladder; any other channel is
    ///   left to its own path (no line).
    ///
    /// Shed / disabled channels only record the flag (they stay `stopped`).
    /// Remembered before the lazy start, like `setDisabledStreams`.
    pub fn set_source_absent(
        &mut self,
        stream_key: &str,
        absent: bool,
        now: u64,
        env: &dyn SpawnEnv,
    ) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.stopped {
            return out;
        }
        let changed = if absent {
            self.source_absent.insert(stream_key.to_string())
        } else {
            self.source_absent.remove(stream_key)
        };
        if !changed {
            return out;
        }
        for i in 0..self.channels.len() {
            if self.channels[i].stream_key != stream_key {
                continue;
            }
            let name = self.channels[i].channel_name.clone();
            if absent {
                // Shed / disabled stay `stopped`; a guard park (`no-camera` /
                // `no-pipeline`) keeps its own truth and slow retry.
                let guard_parked = matches!(
                    self.channels[i].state,
                    ChannelState::NoCamera | ChannelState::NoPipeline
                ) && !self.channels[i].has_handle;
                if self.shed || self.channels[i].disabled || guard_parked {
                    continue;
                }
                let (st, rt) = (
                    self.channels[i].stable_timer.take(),
                    self.channels[i].restart_timer.take(),
                );
                self.clear(st);
                self.clear(rt);
                self.channels[i].state = ChannelState::Failed;
                if self.channels[i].has_handle {
                    out.push(Effect::Signal {
                        channel: name.clone(),
                    });
                }
                out.push(Effect::Log {
                    level: LogLevel::Warn,
                    line: format!(
                        "[media {name}] capture source down (camera absent or failing); master held until it is up"
                    ),
                });
            } else {
                // Only a HELD channel starts: a live master (SIGINTed, not
                // yet exited) takes the TS path at its exit; a pending
                // restart (a guard park's slow retry) fires by itself.
                if self.shed
                    || self.channels[i].disabled
                    || self.channels[i].has_handle
                    || self.channels[i].restart_timer.is_some()
                {
                    continue;
                }
                self.channels[i].quick_failures = 0;
                self.channels[i].backoff_ms = BASE_BACKOFF_MS;
                out.push(Effect::Log {
                    level: LogLevel::Log,
                    line: format!("[media {name}] capture source up; starting master"),
                });
                self.spawn(&name, now, env, &mut out);
            }
        }
        out
    }

    /// `restartChannel(streamKey)` (`media.ts:354-365`): bounce a running
    /// master after its capture daemon respawned; `onExit` reschedules it.
    pub fn restart_channel(&mut self, stream_key: &str) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.stopped || self.shed {
            return out;
        }
        for c in &self.channels {
            if c.stream_key != stream_key {
                continue;
            }
            if c.has_handle {
                out.push(Effect::Log {
                    level: LogLevel::Log,
                    line: format!(
                        "[media {}] restarting master (capture daemon respawned)",
                        c.channel_name
                    ),
                });
                out.push(Effect::Signal {
                    channel: c.channel_name.clone(),
                });
            }
        }
        out
    }

    /// `stop()` (`media.ts:366-378`): SIGINT every master, cancel restarts.
    pub fn stop(&mut self) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.stopped {
            return out;
        }
        self.stopped = true;
        for i in 0..self.channels.len() {
            let (st, rt) = (
                self.channels[i].stable_timer.take(),
                self.channels[i].restart_timer.take(),
            );
            self.clear(st);
            self.clear(rt);
            self.channels[i].state = ChannelState::Stopped;
            if self.channels[i].has_handle {
                out.push(Effect::Signal {
                    channel: self.channels[i].channel_name.clone(),
                });
            }
            self.channels[i].has_handle = false;
        }
        out
    }
}
