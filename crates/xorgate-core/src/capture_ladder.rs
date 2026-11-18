//! Per-camera supervision decisions of the video capture daemon. Port of the
//! DECISION parts of `VideoCaptureSupervisor` (`src/recording/video-capture.ts`
//! @ 89b5c64): the exit ladder of `onExit` (`:789-856`: not-found park,
//! watchdog stalls counted apart from quick failures, stable → prompt
//! restart, quick failures → backoff 2→30 s → park), the progress watchdog
//! verdict of `checkProgress` (`:963-1007`), the `recording` claim of
//! `isRecording` (`:591-594`), the `shmpipe.<pid>.` match of
//! `reapShmSegments` (`:873-891`) and every event string the supervisor
//! emits (`:439`, `:710-715`, `:792`, `:805-817`, `:824-850`, `:994-999`).
//!
//! I/O (spawn, timers, fs.watch, sweeps, unlinks) lives in
//! `xorgate-agent::recording::capture`; the replay-mode branches
//! (`eosIsClean`, `respawnHoldMs`, `cycle` laps, `:768-787`) are NOT ported
//! (phase-3 P14 / Q6).
//!
//! Field-proven behaviour this file pins (memory `1080p30-stall-bug`): a run
//! that put no bytes on the active fragment for [`STALL_MS`] is SIGKILLed;
//! stalls accumulate in their own counter that only observed bytes clear
//! (the stable timer would clear a shared one, so a stalled camera would
//! hot-loop at 2 s forever); [`MAX_STALL_FAILURES`] in a row parks the camera
//! on the 60 s slow retry; after one stall `recording:true` must be earned
//! by bytes.

use serde::{Deserialize, Serialize};

use crate::round::js_math_round;
use crate::telemetry::ActivityLevel;

/// `STABLE_MS` (`video-capture.ts:52`): a child alive this long is "running"
/// and its crash counters reset.
pub const STABLE_MS: u64 = 15_000;
/// `BASE_BACKOFF_MS` (`:53`).
pub const BASE_BACKOFF_MS: u64 = 2_000;
/// `MAX_BACKOFF_MS` (`:54`).
pub const MAX_BACKOFF_MS: u64 = 30_000;
/// `MAX_QUICK_FAILURES` (`:55`): deaths before stabilising that park the camera.
pub const MAX_QUICK_FAILURES: u32 = 3;
/// `PARKED_RETRY_MS` (`:56`): the slow retry of a parked camera.
pub const PARKED_RETRY_MS: u64 = 60_000;
/// `SWEEP_POLL_MS` (`:58`): the fs.watch fallback sweep (authoritative).
pub const SWEEP_POLL_MS: u64 = 10_000;
/// `STALL_MS` (`:74`): progress-watchdog patience (~10x margin over the
/// 2-4 s a healthy child needs for its first bytes).
pub const STALL_MS: u64 = 45_000;
/// `MAX_STALL_FAILURES` (`:76`): consecutive stalls before parking.
pub const MAX_STALL_FAILURES: u32 = 3;
/// `SHM_SEGMENT_DIR` (`:82`): where shmsink's `shm_open` areas land.
pub const SHM_SEGMENT_DIR: &str = "/dev/shm";
/// gst-launch's PLAYING line on stdout (`GST_PLAYING_RE`, `:142`).
pub const GST_PLAYING_MARK: &str = "New clock:";
/// gst-launch's EOS line on stdout (`GST_EOS_RE`, `:143`).
pub const GST_EOS_MARK: &str = "Got EOS from element";

/// `CaptureState` (`video-capture.ts:261`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureState {
    Starting,
    Running,
    Retrying,
    Failed,
    Stopped,
}

/// The crash counters a `CameraRun` carries across children
/// (`video-capture.ts:361-362,380`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderCounters {
    pub quick_failures: u32,
    pub stall_failures: u32,
    pub backoff_ms: u64,
}

impl Default for LadderCounters {
    /// A fresh run (`launch`, `:460-471`).
    fn default() -> Self {
        Self {
            quick_failures: 0,
            stall_failures: 0,
            backoff_ms: BASE_BACKOFF_MS,
        }
    }
}

/// What `onExit` knows about a child that died while the supervisor still
/// wants it (not stopped, not retiring — those branches are I/O-only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInput<'a> {
    pub stream_key: &'a str,
    /// `exitCode` (`None` = killed by a signal / never spawned → `?`).
    pub exit_code: Option<i32>,
    /// `res.notFound`. NOTE: the TS's execa 9 (`reject:false`) never sets it
    /// for a missing binary (phase-2 review-wp4: `notFound:false`,
    /// `exitCode:null`), so in real mode the caller passes `false`.
    pub not_found: bool,
    /// The progress watchdog killed this child (`run.stalled`).
    pub stalled: bool,
    /// `Date.now() - run.startedAt` (measured on the monotonic clock, P8).
    pub ran_ms: u64,
    /// `this.opts.stableMs ?? STABLE_MS`.
    pub stable_ms: u64,
    /// The session the child was writing (`run.sessionId` before it is cleared).
    pub ended_session: Option<&'a str>,
    pub counters: LadderCounters,
}

/// The `onExit` verdict: the run's new state and counters, the one event
/// it emits and the restart delay it schedules (always exactly one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitVerdict {
    /// The run's new state; `None` = unchanged (the stable-exit branch,
    /// `:823-833`, never assigns `run.state`).
    pub state: Option<CaptureState>,
    pub counters: LadderCounters,
    pub level: ActivityLevel,
    pub event: String,
    pub restart_ms: u64,
}

/// `sessionId.slice(0, 8)` — session ids are ASCII UUIDv7 strings.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// `${exitCode ?? "?"}`.
fn code_text(code: Option<i32>) -> String {
    code.map_or_else(|| "?".to_string(), |c| c.to_string())
}

/// The failure ladder of `onExit` (`video-capture.ts:789-856`), in the TS's
/// precedence: not-found → park; a watchdog kill → its own counter (retry at
/// the BASE backoff, park on the 3rd in a row); a child that outlived the
/// stable window → prompt restart at the base backoff with the quick
/// counters reset; else a quick failure → retry at the current backoff
/// (then doubled, capped at 30 s), park on the 3rd (counters reset).
pub fn exit_verdict(input: &ExitInput<'_>) -> ExitVerdict {
    let key = input.stream_key;
    let mut c = input.counters;
    if input.not_found {
        return ExitVerdict {
            state: Some(CaptureState::Failed),
            counters: c,
            level: ActivityLevel::Warn,
            event: format!("video capture {key}: gst-launch-1.0 not found; parking"),
            restart_ms: PARKED_RETRY_MS,
        };
    }
    if input.stalled {
        c.stall_failures += 1;
        if c.stall_failures >= MAX_STALL_FAILURES {
            return ExitVerdict {
                state: Some(CaptureState::Failed),
                counters: c,
                level: ActivityLevel::Warn,
                event: format!(
                    "video capture {key} stalled {}x in a row; parking for {}s (sensor mode unsupported? camera wedged?)",
                    c.stall_failures,
                    PARKED_RETRY_MS / 1000
                ),
                restart_ms: PARKED_RETRY_MS,
            };
        }
        return ExitVerdict {
            state: Some(CaptureState::Retrying),
            counters: c,
            level: ActivityLevel::Warn,
            event: format!(
                "video capture {key} stalled; retry {}/{MAX_STALL_FAILURES} in {BASE_BACKOFF_MS}ms",
                c.stall_failures
            ),
            restart_ms: BASE_BACKOFF_MS,
        };
    }
    if input.ran_ms >= input.stable_ms {
        // The TS leaves `run.state` untouched here (still "running" or
        // "starting") — reproduced: `state: None`.
        c.quick_failures = 0;
        c.backoff_ms = BASE_BACKOFF_MS;
        return ExitVerdict {
            state: None,
            counters: c,
            level: ActivityLevel::Warn,
            event: format!(
                "video capture {key} exited (code {}, session {}…); restarting",
                code_text(input.exit_code),
                input.ended_session.map(short).unwrap_or("?")
            ),
            restart_ms: BASE_BACKOFF_MS,
        };
    }
    c.quick_failures += 1;
    if c.quick_failures >= MAX_QUICK_FAILURES {
        let event = format!(
            "video capture {key} exited (code {}) before stabilizing {}x; parking for {}s (camera in use? libcamera error?)",
            code_text(input.exit_code),
            c.quick_failures,
            PARKED_RETRY_MS / 1000
        );
        c.quick_failures = 0;
        c.backoff_ms = BASE_BACKOFF_MS;
        return ExitVerdict {
            state: Some(CaptureState::Failed),
            counters: c,
            level: ActivityLevel::Warn,
            event,
            restart_ms: PARKED_RETRY_MS,
        };
    }
    let event = format!(
        "video capture {key} exited (code {}); retry {}/{MAX_QUICK_FAILURES} in {}ms",
        code_text(input.exit_code),
        c.quick_failures,
        c.backoff_ms
    );
    let restart_ms = c.backoff_ms;
    c.backoff_ms = (c.backoff_ms * 2).min(MAX_BACKOFF_MS);
    ExitVerdict {
        state: Some(CaptureState::Retrying),
        counters: c,
        level: ActivityLevel::Warn,
        event,
        restart_ms,
    }
}

/// D86 (F-DA21-KVSLOOP, Rust-only): the cap of the start-failure back-off.
pub const ABSENT_BACKOFF_MAX_MS: u64 = 300_000;

/// D86: the park delay of back-off step `step` (1-based). Step 1 is the
/// JS park ([`PARKED_RETRY_MS`], 60 s); each later step doubles it, capped
/// at [`ABSENT_BACKOFF_MAX_MS`]: 60 → 120 → 240 → 300 → 300 … s.
pub fn absent_backoff_ms(step: u32) -> u64 {
    let shift = step.saturating_sub(1).min(16);
    PARKED_RETRY_MS
        .saturating_mul(1u64 << shift)
        .min(ABSENT_BACKOFF_MAX_MS)
}

/// D86: [`exit_verdict`] plus the start-failure back-off the TS lacks.
///
/// The TS ladder (kept verbatim for a camera that has never parked) retries a
/// camera that dies at start twice (2 s, 4 s), parks it for 60 s, then starts
/// the SAME three-attempt ladder again with fresh counters, forever: an
/// absent or broken sensor (`Camera.start()` → `Remote I/O error`) costs three
/// spawns (each with ~20 lines of libcamera/gst stderr) and three KVS master
/// bounces a minute, ≈150 journal lines a minute (bench 2, 2026-11-18).
///
/// `steps` counts the camera's consecutive start-failure parks with no
/// stable run in between (0 = never parked: the TS ladder). Rules:
/// - a QUICK exit (not a watchdog stall, not "not found", `ran_ms <
///   stable_ms`) with `steps == 0` keeps the TS verdict; if that verdict
///   parks (the 3rd quick failure), `steps` becomes 1;
/// - a quick exit with `steps >= 1` is a failed PROBE after a park: ONE
///   attempt per step (no 2 s / 4 s mini-ladder), parked again for
///   [`absent_backoff_ms`]`(steps + 1)`, one `Warn` line per step;
/// - an exit after the stable window (the camera worked) resets `steps` to
///   0 (the stable timer resets it too, in the agent); a stall or a
///   not-found exit leaves it alone (their own TS ladders decide).
///
/// Returns the verdict to apply and the new `steps`. While `steps >= 1`
/// the capture source is down: the agent holds the camera's KVS master
/// (`MediaLadder::set_source_absent`) until a probe runs stable.
pub fn absent_backoff(input: &ExitInput<'_>, v: ExitVerdict, steps: u32) -> (ExitVerdict, u32) {
    if input.not_found || input.stalled {
        return (v, steps);
    }
    if input.ran_ms >= input.stable_ms {
        return (v, 0);
    }
    if steps == 0 {
        let parked = v.state == Some(CaptureState::Failed);
        return (v, u32::from(parked));
    }
    let step = steps.saturating_add(1);
    let restart_ms = absent_backoff_ms(step);
    let counters = LadderCounters {
        quick_failures: 0,
        stall_failures: input.counters.stall_failures,
        backoff_ms: BASE_BACKOFF_MS,
    };
    let event = format!(
        "video capture {} exited (code {}) at start again; backing off {}s (step {step}; camera absent or broken?)",
        input.stream_key,
        code_text(input.exit_code),
        restart_ms / 1000
    );
    (
        ExitVerdict {
            state: Some(CaptureState::Failed),
            counters,
            level: ActivityLevel::Warn,
            event,
            restart_ms,
        },
        step,
    )
}

/// The watchdog bookkeeping of a `CameraRun` (`video-capture.ts:369-371`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// The fragment last seen growing.
    pub path: Option<String>,
    pub bytes: u64,
    /// When (monotonic ms).
    pub at_ms: u64,
}

/// `checkProgress`'s verdict (`video-capture.ts:963-1007`) once its
/// preconditions hold (record mode, not stopped/retiring/stalled, a child).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressVerdict {
    /// The active fragment moved on or grew: new baseline, `stallFailures = 0`.
    Advanced { path: String, bytes: u64 },
    /// No progress yet, still inside the patience window.
    Waiting,
    /// No progress for `stall_ms`: mark stalled, state `failed`, stop
    /// watching, emit [`stall_event`], SIGKILL.
    Stalled,
}

/// `checkProgress` (`video-capture.ts:966-989`): progress is "the active
/// fragment moved on, or grew" — a rotation counts even though the new file
/// is smaller (closed fragments are deleted after upload, so byte totals are
/// not monotonic); a 0-byte or missing active fragment is NOT progress.
/// `bytes` is the active file's size (0 when the stat failed).
pub fn progress_verdict(
    active_path: Option<&str>,
    bytes: u64,
    last: &Progress,
    now_ms: u64,
    stall_ms: u64,
) -> ProgressVerdict {
    if let Some(p) = active_path {
        if bytes > 0 && (last.path.as_deref() != Some(p) || bytes > last.bytes) {
            return ProgressVerdict::Advanced {
                path: p.to_string(),
                bytes,
            };
        }
    }
    if now_ms.saturating_sub(last.at_ms) < stall_ms {
        return ProgressVerdict::Waiting;
    }
    ProgressVerdict::Stalled
}

/// The stall event (`video-capture.ts:994-999`).
pub fn stall_event(stream_key: &str, stall_ms: u64, session_id: Option<&str>) -> String {
    format!(
        "video capture {stream_key} stalled: no bytes written for {}s (session {}…) — killing and restarting",
        js_math_round(stall_ms as f64 / 1000.0),
        session_id.map(short).unwrap_or("?")
    )
}

/// `isRecording` (`video-capture.ts:591-594`): up (starting/running) AND,
/// once the camera has stalled, bytes observed since. `status().recording`
/// and `anyRecording()` additionally require record mode.
pub fn is_recording(state: CaptureState, stall_failures: u32, progress_bytes: u64) -> bool {
    if state != CaptureState::Running && state != CaptureState::Starting {
        return false;
    }
    stall_failures == 0 || progress_bytes > 0
}

/// The spawn event (`video-capture.ts:710-715`). `session_id` is the freshly
/// minted id in record mode.
pub fn spawn_event(record: bool, stream_key: &str, preset: &str, session_id: &str) -> String {
    if record {
        format!(
            "video capture started: {stream_key} {preset} (session {}…)",
            short(session_id)
        )
    } else {
        format!("video bridge started: {stream_key} (streaming only, recording off)")
    }
}

/// The `setCameras` removal event (`video-capture.ts:439`).
pub fn camera_disabled_event(stream_key: &str) -> String {
    format!("video capture stopped: {stream_key} (camera disabled)")
}

/// The stderr forward (`video-capture.ts:672-674`): `console.error` of
/// `[video-capture <key>] <line>` for every non-blank line.
pub fn stderr_log_line(stream_key: &str, line: &str) -> Option<String> {
    if crate::js::trim(line).is_empty() {
        None
    } else {
        Some(format!("[video-capture {stream_key}] {line}"))
    }
}

/// The owner pid of a shmsink area name, `None` for any other name.
///
/// shmsink names its areas `"/shmpipe.%5d.%5d"` with `getpid()` and a
/// counter (gst-plugins-bad `sys/shm/shmpipe.c:307` @ 1.22, `sp_writer_create`;
/// observed with gst 1.22.0: `shmpipe.   16.    0`, `shmpipe.12022.    0`;
/// bench: `shmpipe. 1035.    0`). So a pid below 10000 is SPACE-padded to 5
/// columns and a pid above 99999 is not padded at all. The pid field is the
/// text between `shmpipe.` and the next `.`, leading spaces trimmed; nothing
/// else about the name is checked, so every unpadded name the TS prefix
/// `shmpipe.<pid>.` matches still matches (D41).
pub fn shm_owner_pid(name: &str) -> Option<u32> {
    let (field, _) = name.strip_prefix("shmpipe.")?.split_once('.')?;
    let digits = field.trim_start_matches(' ');
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The names `reapShmSegments` unlinks (`video-capture.ts:873-891`): only
/// the dead child's OWN segments — never another process's (the JS agent's
/// live on the same tmpfs). No pid → nothing.
///
/// Accepted divergence D41: the TS matches the prefix `shmpipe.${pid}.`
/// (`:876`), which never matches shmsink's real name for a pid below 10000
/// (`shmpipe. 1035.    0`, see [`shm_owner_pid`]), so the TS reaper silently
/// leaks 20 MiB per killed child started near boot. Rust compares the
/// trimmed owner pid instead: the same names for 5+ digit pids (and for every
/// TS fixture), plus the padded ones.
pub fn shm_names_to_reap(names: &[String], pid: Option<u32>) -> Vec<&str> {
    let Some(pid) = pid else {
        return Vec::new();
    };
    names
        .iter()
        .filter(|n| shm_owner_pid(n) == Some(pid))
        .map(String::as_str)
        .collect()
}

/// `launch` (`video-capture.ts:477`): camera N spawns `N * staggerMs` after
/// the call; `0` = synchronously.
pub fn stagger_delay_ms(index: usize, stagger_ms: u64) -> u64 {
    index as u64 * stagger_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_counts_as_progress() {
        let last = Progress {
            path: Some("/s/00000.mp4".into()),
            bytes: 50_000,
            at_ms: 0,
        };
        assert_eq!(
            progress_verdict(Some("/s/00001.mp4"), 120, &last, 999_999, 200),
            ProgressVerdict::Advanced {
                path: "/s/00001.mp4".into(),
                bytes: 120
            }
        );
        assert_eq!(
            progress_verdict(Some("/s/00000.mp4"), 50_000, &last, 199, 200),
            ProgressVerdict::Waiting
        );
        assert_eq!(
            progress_verdict(Some("/s/00000.mp4"), 0, &last, 200, 200),
            ProgressVerdict::Stalled
        );
    }

    #[test]
    fn shm_prefix_is_exact() {
        let names: Vec<String> = ["shmpipe.42.    0", "shmpipe.4242.    0", "shmpipe.42"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(shm_names_to_reap(&names, Some(42)), ["shmpipe.42.    0"]);
        assert!(shm_names_to_reap(&names, None).is_empty());
    }

    /// D41: shmsink's REAL names (`"shmpipe.%5d.%5d"`, shmpipe.c:307):
    /// 2-4 digit pids are space-padded, 5 digits fill the field, 6+ overflow.
    #[test]
    fn shm_owner_pid_reads_shmsinks_padded_names() {
        for (name, pid) in [
            ("shmpipe.   16.    0", Some(16)),
            ("shmpipe.  983.    0", Some(983)),
            ("shmpipe. 1035.    0", Some(1035)),
            ("shmpipe. 1035.    1", Some(1035)),
            ("shmpipe.12022.    0", Some(12022)),
            ("shmpipe.123456.    0", Some(123456)),
            ("shmpipe.4242.    0", Some(4242)), // unpadded (the TS fixture form)
            ("shmpipe.    .    0", None),
            ("shmpipe. 10 35.    0", None),
            ("shmpipe.-1035.    0", None),
            ("shmpipe. 1035", None),
            ("xshmpipe. 1035.    0", None),
            ("lttng-ust-wait-8", None),
        ] {
            assert_eq!(shm_owner_pid(name), pid, "{name:?}");
        }
        let names: Vec<String> = [
            "shmpipe. 1035.    0",
            "shmpipe. 1035.    1",
            "shmpipe.  103.    0",
            "shmpipe.10350.    0",
            "shmpipe.12022.    0",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // The pre-D41 prefix match reaped NOTHING here (the bench leak).
        assert_eq!(
            shm_names_to_reap(&names, Some(1035)),
            ["shmpipe. 1035.    0", "shmpipe. 1035.    1"]
        );
        assert_eq!(
            shm_names_to_reap(&names, Some(12022)),
            ["shmpipe.12022.    0"]
        );
        assert!(shm_names_to_reap(&names, Some(35)).is_empty());
    }

    // ---- D86 (F-DA21-KVSLOOP): the start-failure back-off ----

    fn quick(steps_counters: LadderCounters, ran_ms: u64) -> ExitInput<'static> {
        ExitInput {
            stream_key: "cam1",
            exit_code: Some(1),
            not_found: false,
            stalled: false,
            ran_ms,
            stable_ms: STABLE_MS,
            ended_session: None,
            counters: steps_counters,
        }
    }

    /// One exit through the TS ladder and the D86 overlay, like the agent.
    fn step(c: LadderCounters, steps: u32, ran_ms: u64) -> (ExitVerdict, u32) {
        let i = quick(c, ran_ms);
        absent_backoff(&i, exit_verdict(&i), steps)
    }

    #[test]
    fn d86_backoff_schedule_is_exponential_and_capped_at_5_min() {
        let got: Vec<u64> = (1..=8).map(absent_backoff_ms).collect();
        assert_eq!(
            got,
            [60_000, 120_000, 240_000, 300_000, 300_000, 300_000, 300_000, 300_000]
        );
        assert_eq!(
            absent_backoff_ms(0),
            PARKED_RETRY_MS,
            "step 0 never schedules; clamp"
        );
        assert_eq!(
            absent_backoff_ms(u32::MAX),
            ABSENT_BACKOFF_MAX_MS,
            "no overflow"
        );
        assert_eq!(
            absent_backoff_ms(1),
            PARKED_RETRY_MS,
            "step 1 = the TS park"
        );
    }

    /// An absent sensor (every child dies at start): the TS ladder once
    /// (2 s, 4 s, park 60 s, its exact lines), then ONE attempt and ONE line
    /// per back-off step: 120 s, 240 s, 300 s, 300 s.
    #[test]
    fn d86_absent_sensor_walks_the_ts_ladder_once_then_one_probe_per_step() {
        let mut c = LadderCounters::default();
        let mut steps = 0;
        let mut delays = Vec::new();
        let mut lines = Vec::new();
        for _ in 0..7 {
            let (v, s) = step(c, steps, 120);
            c = v.counters;
            steps = s;
            delays.push(v.restart_ms);
            lines.push(v.event.clone());
            assert_eq!(v.level, ActivityLevel::Warn);
        }
        assert_eq!(
            delays,
            [2_000, 4_000, 60_000, 120_000, 240_000, 300_000, 300_000]
        );
        assert_eq!(steps, 5);
        // The first three lines are the TS's, byte for byte.
        assert_eq!(
            lines[0],
            "video capture cam1 exited (code 1); retry 1/3 in 2000ms"
        );
        assert_eq!(
            lines[1],
            "video capture cam1 exited (code 1); retry 2/3 in 4000ms"
        );
        assert_eq!(
            lines[2],
            "video capture cam1 exited (code 1) before stabilizing 3x; parking for 60s (camera in use? libcamera error?)"
        );
        assert_eq!(
            lines[3],
            "video capture cam1 exited (code 1) at start again; backing off 120s (step 2; camera absent or broken?)"
        );
        assert_eq!(
            lines[6],
            "video capture cam1 exited (code 1) at start again; backing off 300s (step 5; camera absent or broken?)"
        );
        // Spawns per hour at the cap: 3600 / 300 = 12, vs the TS's 3 per ~66 s ≈ 164.
    }

    #[test]
    fn d86_probe_park_state_and_counters() {
        let c = LadderCounters {
            quick_failures: 0,
            stall_failures: 2,
            backoff_ms: BASE_BACKOFF_MS,
        };
        let (v, s) = step(c, 1, 50);
        assert_eq!(s, 2);
        assert_eq!(
            v.state,
            Some(CaptureState::Failed),
            "parked = failed, as the TS park"
        );
        assert_eq!(
            v.counters,
            LadderCounters {
                quick_failures: 0,
                stall_failures: 2,
                backoff_ms: BASE_BACKOFF_MS
            },
            "the stall counter is the watchdog's own; untouched"
        );
        // A signal death prints `?` like the TS.
        let mut i = quick(c, 10);
        i.exit_code = None;
        let (v, _) = absent_backoff(&i, exit_verdict(&i), 3);
        assert!(
            v.event
                .contains("(code ?) at start again; backing off 300s (step 4;"),
            "{}",
            v.event
        );
    }

    /// A camera that ran stable resets the back-off (recovery); the TS
    /// verdict is untouched.
    #[test]
    fn d86_a_stable_run_resets_the_steps() {
        let c = LadderCounters::default();
        let i = quick(c, STABLE_MS);
        let v = exit_verdict(&i);
        let (got, s) = absent_backoff(&i, v.clone(), 4);
        assert_eq!((got, s), (v, 0));
    }

    /// Stalls and not-found exits keep their own TS ladders and leave the
    /// steps alone; a camera that never parked is pure TS.
    #[test]
    fn d86_never_parked_stall_and_not_found_are_pure_ts() {
        for (stalled, not_found) in [(true, false), (false, true)] {
            let mut i = quick(LadderCounters::default(), 50_000);
            i.stalled = stalled;
            i.not_found = not_found;
            let v = exit_verdict(&i);
            assert_eq!(absent_backoff(&i, v.clone(), 3), (v, 3));
        }
        for q in 0..MAX_QUICK_FAILURES - 1 {
            let c = LadderCounters {
                quick_failures: q,
                stall_failures: 0,
                backoff_ms: BASE_BACKOFF_MS << q,
            };
            let i = quick(c, 100);
            let v = exit_verdict(&i);
            assert_eq!(absent_backoff(&i, v.clone(), 0), (v, 0), "q={q}");
        }
    }
}
