//! The recording manager's pure decisions — `src/recording/manager.ts`
//! (`apply :424-456`, `applyVideo :457-594`, `noteLiveBlock :660-678`,
//! `noteBootDefer :679-741`, `startRecorder`/`stopRecorder :742-785`,
//! `start :294-329`) minus every side effect (Phase 3 WP1).
//!
//! The agent's `recording::manager` executes these verdicts in the TS order;
//! nothing here touches a file, a clock or a child.
//!
//! **The live-parking choke point (agent-anatomy invariant 6).** Live
//! streaming is held down by EITHER shed rung ≥ 1 OR the GPS/LTE gate, and the
//! published `liveBlock` reason is derived from the SAME two inputs in the same
//! evaluation: [`decide_live`] returns both, so the boolean that reaches
//! `MediaSupervisor::set_shed` and the reason the payload shows cannot come
//! apart (README principle 5). The manager calls `set_shed` from exactly one
//! place, with [`LiveDecision::parked`].
//!
//! **Invariant 7.** Thermal shedding caps at rung 1 inside the ladder
//! (`boot_policy::decide_shed_transition`); [`effective_record`] stops video
//! recording only at rung 2 and never looks at telemetry — the telemetry
//! recorder's plan ([`plan_telemetry`]) takes no shed or gate input at all.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::boot_policy::{
    BootDeferConfig, BootDeferState, BootDeferVerdict, ShedLevel, ShedVerdict,
};
use crate::recording_config::ResolvedVideo;
use crate::telemetry::LiveBlock;
use crate::video_capture::CaptureCamera;

// ---------------------------------------------------------------------------
// Timer constants (`manager.ts:60-74`)
// ---------------------------------------------------------------------------

/// `SETTINGS_POLL_MS` (`manager.ts:60`): settings.json re-check cadence.
pub const SETTINGS_POLL_MS: u64 = 5_000;
/// `TIME_PROBE_TIMEOUT_MS` (`manager.ts:66`): bound on the startup probe await.
pub const TIME_PROBE_TIMEOUT_MS: u64 = 2_000;
/// `DISK_ENFORCE_MS` (`manager.ts:68`): slow disk-pressure sweep.
pub const DISK_ENFORCE_MS: u64 = 60_000;
/// `SHED_SAMPLE_MS` (`manager.ts:74`): load-shed power sample cadence.
pub const SHED_SAMPLE_MS: u64 = 1_000;

// ---------------------------------------------------------------------------
// Event strings (`onEvent` → `[agent] <msg>` + state.json event)
// ---------------------------------------------------------------------------

/// `manager.ts:300`: `recording: finalized N orphan segment(s) from previous run` (info).
pub fn orphans_finalized_event(n: usize) -> String {
    format!("recording: finalized {n} orphan segment(s) from previous run")
}

/// `manager.ts:306`: `recording: backfilled N video sidecar(s) from previous run` (info).
pub fn sidecars_backfilled_event(n: usize) -> String {
    format!("recording: backfilled {n} video sidecar(s) from previous run")
}

/// `manager.ts:774`: `telemetry recording started (session <8>…, <rate> Hz)` (info).
/// `sessionId.slice(0, 8)` — session ids are ASCII uuids, so 8 bytes = 8 UTF-16 units.
pub fn telemetry_started_event(session_id: &str, rate_hz: u32) -> String {
    let head: String = session_id.chars().take(8).collect();
    format!("telemetry recording started (session {head}…, {rate_hz} Hz)")
}

/// `manager.ts:782`: `telemetry recording stopped (<why>)` (info).
pub fn telemetry_stopped_event(why: &str) -> String {
    format!("telemetry recording stopped ({why})")
}

/// `stopRecorder` reasons (`manager.ts:440`, `:360`).
pub const STOP_RATE_CHANGED: &str = "rateHz changed";
pub const STOP_DISABLED: &str = "recording disabled";
pub const STOP_SHUTDOWN: &str = "agent shutdown";

/// `manager.ts:607` (info), on disarming a shed monitor that had published a rung.
pub const SHED_DISARMED_EVENT: &str = "power shedding disarmed — restoring full operation";

/// `manager.ts:558-563` (warn): video wanted but no bridged camera survived.
pub const NO_BRIDGED_CAMERA_WARNING: &str =
    "recording.video.enabled is on but no bridged camera is available \
(media off, every camera disabled, or no `xorgate pipeline bridge <streamKey>` run) \
— video recording idle";

// ---------------------------------------------------------------------------
// apply() — telemetry recorder plan (`manager.ts:436-443`)
// ---------------------------------------------------------------------------

/// What `apply()` does to the telemetry recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryPlan {
    /// Stop the running recorder with this reason first.
    pub stop: Option<&'static str>,
    /// Then start a new one (new session).
    pub start: bool,
}

/// `apply()`'s recorder block (`manager.ts:436-443`). `running_rate` is
/// `activeRateHz` while a recorder runs (`None` = no recorder). Takes no
/// shed, gate or video input: telemetry recording — the crash evidence — is
/// never deferred, gated or shed (invariant 7).
pub fn plan_telemetry(running_rate: Option<u32>, want: bool, rate_hz: u32) -> TelemetryPlan {
    let running = running_rate.is_some();
    let rate_changed = running && running_rate != Some(rate_hz);
    let stop = (running && (!want || rate_changed)).then_some(if rate_changed {
        STOP_RATE_CHANGED
    } else {
        STOP_DISABLED
    });
    let still_running = running && stop.is_none();
    TelemetryPlan {
        stop,
        start: want && !still_running,
    }
}

// ---------------------------------------------------------------------------
// applyVideo() — per-camera enables (`manager.ts:466-483`)
// ---------------------------------------------------------------------------

/// The stream keys explicitly turned OFF in `recording.video.cameras`,
/// sorted (`Object.entries(…).filter(on === false).map(key).sort()`,
/// `manager.ts:466-469`). JS's default sort compares UTF-16 code units; for
/// the ASCII stream keys the agent uses (`cam0`, `cam1`) that is byte order.
pub fn disabled_cameras(cameras: &IndexMap<String, bool>) -> Vec<String> {
    let mut out: Vec<String> = cameras
        .iter()
        .filter(|(_, on)| !**on)
        .map(|(k, _)| k.clone())
        .collect();
    out.sort();
    out
}

/// The dedupe key of the published disabled set (`disabled.join(",")`).
pub fn disabled_key(disabled: &[String]) -> String {
    disabled.join(",")
}

/// `manager.ts:471-478` (info), only when the set is non-empty.
pub fn cameras_disabled_event(disabled: &[String]) -> Option<String> {
    (!disabled.is_empty()).then(|| {
        format!(
            "camera(s) disabled by settings: {} — no capture, bridge or live stream",
            disabled.join(", ")
        )
    })
}

/// `(mediaEnabled ? cameras() : []).filter((c) => cfg.video.cameras[c.streamKey] !== false)`
/// (`manager.ts:483-485`): an absent key is enabled; an unknown key is inert.
pub fn enabled_cameras(
    cameras: Vec<CaptureCamera>,
    settings: &IndexMap<String, bool>,
) -> Vec<CaptureCamera> {
    cameras
        .into_iter()
        .filter(|c| settings.get(&c.stream_key) != Some(&false))
        .collect()
}

/// `updateShedMonitor`'s arming rule (`manager.ts:596-597`): armed only while
/// a bridged camera exists, the feature is enabled, and a power sampler is
/// wired (sim/dev host: none → inert).
pub fn shed_armed(cameras_present: bool, shedding_enabled: bool, has_sampler: bool) -> bool {
    cameras_present && shedding_enabled && has_sampler
}

/// The `BootDeferConfig` `applyVideo` hands the boot policy
/// (`manager.ts:497-504`). Replay mode's `forceBootPolicyOff` is not ported (P14).
pub fn boot_defer_config(video: &ResolvedVideo) -> BootDeferConfig {
    BootDeferConfig {
        mode: video.boot_defer,
        max_secs: video.boot_defer_max_secs,
        min_budget_ma: video.boot_defer_min_budget_ma,
        gate_mode: video.gps_gate,
        gate_max_secs: video.gps_gate_max_secs,
        gate_no_progress_secs: video.gps_gate_no_progress_secs,
    }
}

// ---------------------------------------------------------------------------
// THE choke point (`manager.ts:509-533`)
// ---------------------------------------------------------------------------

/// `Omit<LiveBlock, "since">` — the reason/detail of a block before the
/// manager stamps `since`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveBlockCandidate {
    pub reason: String,
    pub detail: String,
}

/// The one live verdict of an `applyVideo` pass: whether the KVS masters are
/// parked, and why. Both halves come from the same inputs (invariant 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveDecision {
    /// `liveParked = (shed !== null && shed.level >= 1) || gateHoldsLive`.
    pub parked: bool,
    /// The `noteLiveBlock` argument: `None` = live allowed.
    pub block: Option<LiveBlockCandidate>,
}

/// `manager.ts:509-533`: live is held by shed rung ≥ 1 OR the gate. The gate
/// wins the REASON when both hold (it is the one with a knowable end); a shed
/// rung ≥ 1 always carries a latched reason by construction (`reason !==
/// null` in the TS is type narrowing), but the check is kept exactly so a
/// reason-less rung parks live without publishing a block — as the TS does.
pub fn decide_live(
    shed: Option<&ShedVerdict>,
    gate_holds_live: bool,
    gate_why: &str,
) -> LiveDecision {
    let shed_parks = shed.is_some_and(|s| s.level >= ShedLevel::NoLive);
    let parked = shed_parks || gate_holds_live;
    let block = if gate_holds_live {
        Some(LiveBlockCandidate {
            reason: "gps-gate".to_string(),
            detail: format!("waiting for LTE attach + GPS fix ({gate_why})"),
        })
    } else {
        match shed {
            Some(s) if s.level >= ShedLevel::NoLive => s.reason.map(|r| LiveBlockCandidate {
                reason: r.as_str().to_string(),
                detail: s.detail.clone().unwrap_or_default(),
            }),
            _ => None,
        }
    };
    LiveDecision { parked, block }
}

/// `noteLiveBlock` (`manager.ts:660-678`): `since` (epoch ms, the payload's
/// clock domain) is stamped on the transition into blocked and re-stamped
/// only when the REASON changes; a fresher detail keeps the stamp. Returns
/// `None` when nothing changed (no `notifyStatus`), else the new value
/// (`Some(None)` = the key goes ABSENT — never null, never `{}`).
pub fn note_live_block(
    prev: Option<&LiveBlock>,
    next: Option<&LiveBlockCandidate>,
    now_ms: i64,
) -> Option<Option<LiveBlock>> {
    match (prev, next) {
        (None, None) => None,
        (Some(_), None) => Some(None),
        (prev, Some(n)) => match prev {
            Some(p) if p.reason == n.reason => (p.detail != n.detail).then(|| {
                Some(LiveBlock {
                    reason: n.reason.clone(),
                    detail: n.detail.clone(),
                    since: p.since,
                })
            }),
            _ => Some(Some(LiveBlock {
                reason: n.reason.clone(),
                detail: n.detail.clone(),
                since: now_ms,
            })),
        },
    }
}

/// Shed rung 2 (`manager.ts:535`): video recording off, bridge-only. The
/// boot policy's `allow:false` has already cleared `record` (`:506`).
/// Telemetry recording is not an input — it is never shed.
pub fn effective_record(video_enabled: bool, boot_allow: bool, shed: Option<&ShedVerdict>) -> bool {
    let record = video_enabled && boot_allow;
    record && !shed.is_some_and(|s| s.level >= ShedLevel::BridgeOnly)
}

// ---------------------------------------------------------------------------
// noteBootDefer (`manager.ts:679-741`)
// ---------------------------------------------------------------------------

/// The published `bootDefer` status (`manager.ts:680`): `released:<why>` or the state.
pub fn boot_defer_status(v: &BootDeferVerdict) -> String {
    if v.state == BootDeferState::Released {
        format!("released:{}", v.why)
    } else {
        v.state.as_str().to_string()
    }
}

/// The transition line `noteBootDefer` logs (info), `manager.ts:683-698`.
pub fn boot_defer_event(v: &BootDeferVerdict, video: &ResolvedVideo) -> String {
    let why = &v.why;
    match v.state {
        BootDeferState::Deferring => format!(
            "video boot-defer armed ({why}) — bridge-only until the modem settles (cap {}s)",
            video.boot_defer_max_secs
        ),
        BootDeferState::Gating => {
            let cap = video.gps_gate_max_secs;
            let cap_text = if cap > 0 {
                format!("cap {cap}s")
            } else {
                "no time cap".to_string()
            };
            let np = video.gps_gate_no_progress_secs;
            let np_text = if np > 0 {
                format!(", no-progress escape {np}s")
            } else {
                String::new()
            };
            format!(
                "video gps gate armed ({why}) — bridge-only + live parked until LTE is up with a GPS fix ({cap_text}{np_text})"
            )
        }
        BootDeferState::Skipped => format!("video boot-defer skipped: {why}"),
        BootDeferState::Released => {
            format!("video boot-defer released ({why}) — starting full capture")
        }
        BootDeferState::Off => format!("video boot-defer off ({why})"),
    }
}

// ---------------------------------------------------------------------------
// applyVideo() — the capture plan (`manager.ts:544-593`)
// ---------------------------------------------------------------------------

/// The effective video tuple EXCLUDING the camera set (`manager.ts:538-547`,
/// `JSON.stringify({record, preset, seg, stagger, encoder})`): a change
/// replaces the capture supervisor. Key order = the TS literal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoShape {
    pub record: bool,
    pub preset: String,
    pub seg: u32,
    pub stagger: u32,
    pub encoder: String,
}

impl VideoShape {
    /// `JSON.stringify(...)` of the tuple — the `videoKey` dedupe string.
    pub fn key(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// `JSON.stringify(cameras)` — the `videoCamerasKey` dedupe string.
pub fn cameras_key(cameras: &[CaptureCamera]) -> String {
    serde_json::to_string(cameras).unwrap_or_default()
}

/// What `applyVideo` does to the capture supervisor after the keys moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum CapturePlan {
    /// Both keys unchanged: nothing (`:557`).
    Unchanged,
    /// No camera survived: queue `stopAndWait` of the current supervisor (if
    /// any); `warn` = log [`NO_BRIDGED_CAMERA_WARNING`] (`record` wanted) (`:562-575`).
    StopAll { warn: bool },
    /// Camera-set-only change with a live supervisor: `setCameras` in place
    /// so the other cameras keep their sessions (`:579-583`).
    InPlace,
    /// Shape change (or no supervisor): queue stopAndWait(prev) → start a new
    /// supervisor unless superseded (`:585-593`). `ensure_disk` when recording.
    Replace,
}

/// `manager.ts:549-593` (the branch structure, after the keys are compared).
pub fn plan_capture(
    prev_key: &str,
    prev_cameras_key: &str,
    key: &str,
    cameras_key: &str,
    cameras_empty: bool,
    record: bool,
    has_supervisor: bool,
) -> CapturePlan {
    if key == prev_key && cameras_key == prev_cameras_key {
        return CapturePlan::Unchanged;
    }
    let shape_changed = key != prev_key;
    if cameras_empty {
        return CapturePlan::StopAll { warn: record };
    }
    if !shape_changed && has_supervisor {
        return CapturePlan::InPlace;
    }
    CapturePlan::Replace
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot_policy::ShedBlockReason;

    fn shed(
        level: ShedLevel,
        reason: Option<ShedBlockReason>,
        detail: Option<&str>,
    ) -> ShedVerdict {
        ShedVerdict {
            level,
            parked: false,
            event: None,
            reason,
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn telemetry_plan_matches_apply() {
        assert_eq!(
            plan_telemetry(None, false, 1),
            TelemetryPlan {
                stop: None,
                start: false
            }
        );
        assert_eq!(
            plan_telemetry(None, true, 1),
            TelemetryPlan {
                stop: None,
                start: true
            }
        );
        assert_eq!(
            plan_telemetry(Some(1), true, 1),
            TelemetryPlan {
                stop: None,
                start: false
            }
        );
        assert_eq!(
            plan_telemetry(Some(20), true, 5),
            TelemetryPlan {
                stop: Some(STOP_RATE_CHANGED),
                start: true
            }
        );
        assert_eq!(
            plan_telemetry(Some(20), false, 20),
            TelemetryPlan {
                stop: Some(STOP_DISABLED),
                start: false
            }
        );
        // rateChanged wins the reason even when disabling.
        assert_eq!(
            plan_telemetry(Some(20), false, 5),
            TelemetryPlan {
                stop: Some(STOP_RATE_CHANGED),
                start: false
            }
        );
    }

    #[test]
    fn choke_point_one_evaluation_two_outputs() {
        // nothing blocks
        assert_eq!(
            decide_live(None, false, ""),
            LiveDecision {
                parked: false,
                block: None
            }
        );
        let s0 = shed(ShedLevel::Full, None, None);
        assert!(!decide_live(Some(&s0), false, "").parked);
        // rung 1 undervoltage
        let s1 = shed(
            ShedLevel::NoLive,
            Some(ShedBlockReason::Undervoltage),
            Some("rail 4.70V < 4.85V for 3s"),
        );
        let d = decide_live(Some(&s1), false, "");
        assert!(d.parked);
        assert_eq!(
            d.block.unwrap(),
            LiveBlockCandidate {
                reason: "undervoltage".into(),
                detail: "rail 4.70V < 4.85V for 3s".into()
            }
        );
        // the gate wins the reason
        let d = decide_live(Some(&s1), true, "mode=always");
        assert!(d.parked);
        assert_eq!(d.block.unwrap().reason, "gps-gate");
        let d = decide_live(None, true, "mode=always");
        assert_eq!(
            d.block.unwrap().detail,
            "waiting for LTE attach + GPS fix (mode=always)"
        );
        // a reason-less rung still parks (type-narrowing branch kept)
        let s2 = shed(ShedLevel::NoLive, None, None);
        assert_eq!(
            decide_live(Some(&s2), false, ""),
            LiveDecision {
                parked: true,
                block: None
            }
        );
        // thermal rung 1
        let st = shed(ShedLevel::NoLive, Some(ShedBlockReason::Thermal), None);
        assert_eq!(decide_live(Some(&st), false, "").block.unwrap().detail, "");
    }

    #[test]
    fn rung_two_stops_video_only() {
        let s1 = shed(ShedLevel::NoLive, Some(ShedBlockReason::Thermal), None);
        let s2 = shed(
            ShedLevel::BridgeOnly,
            Some(ShedBlockReason::Undervoltage),
            None,
        );
        assert!(effective_record(true, true, None));
        assert!(
            effective_record(true, true, Some(&s1)),
            "thermal/rung 1 keeps recording"
        );
        assert!(!effective_record(true, true, Some(&s2)));
        assert!(
            !effective_record(true, false, None),
            "boot defer → bridge-only"
        );
        assert!(!effective_record(false, true, None));
    }

    #[test]
    fn live_block_since_moves_only_on_reason_change() {
        let gate = LiveBlockCandidate {
            reason: "gps-gate".into(),
            detail: "a".into(),
        };
        let b = note_live_block(None, Some(&gate), 100).unwrap().unwrap();
        assert_eq!(b.since, 100);
        assert_eq!(
            note_live_block(Some(&b), Some(&gate), 200),
            None,
            "same block: no notify"
        );
        let fresher = LiveBlockCandidate {
            reason: "gps-gate".into(),
            detail: "b".into(),
        };
        let b2 = note_live_block(Some(&b), Some(&fresher), 300)
            .unwrap()
            .unwrap();
        assert_eq!((b2.since, b2.detail.as_str()), (100, "b"));
        let uv = LiveBlockCandidate {
            reason: "undervoltage".into(),
            detail: "b".into(),
        };
        assert_eq!(
            note_live_block(Some(&b2), Some(&uv), 400)
                .unwrap()
                .unwrap()
                .since,
            400
        );
        assert_eq!(note_live_block(Some(&b2), None, 500), Some(None));
        assert_eq!(note_live_block(None, None, 500), None);
    }

    #[test]
    fn capture_plan_branches() {
        let k = |r: bool| {
            VideoShape {
                record: r,
                preset: "720p15-1mbps".into(),
                seg: 60,
                stagger: 4,
                encoder: "sw-h264".into(),
            }
            .key()
        };
        assert_eq!(
            k(true),
            r#"{"record":true,"preset":"720p15-1mbps","seg":60,"stagger":4,"encoder":"sw-h264"}"#
        );
        assert_eq!(
            plan_capture(&k(true), "c", &k(true), "c", false, true, true),
            CapturePlan::Unchanged
        );
        assert_eq!(
            plan_capture("", "", &k(true), "[]", true, true, false),
            CapturePlan::StopAll { warn: true }
        );
        assert_eq!(
            plan_capture(&k(true), "a", &k(true), "b", false, true, true),
            CapturePlan::InPlace
        );
        assert_eq!(
            plan_capture(&k(true), "a", &k(true), "b", false, true, false),
            CapturePlan::Replace
        );
        assert_eq!(
            plan_capture(&k(false), "a", &k(true), "a", false, true, true),
            CapturePlan::Replace
        );
    }

    #[test]
    fn camera_enables() {
        let mut m = IndexMap::new();
        m.insert("cam1".to_string(), false);
        m.insert("cam2".to_string(), true);
        m.insert("cam0".to_string(), false);
        assert_eq!(disabled_cameras(&m), vec!["cam0", "cam1"]);
        assert_eq!(
            cameras_disabled_event(&disabled_cameras(&m)).unwrap(),
            "camera(s) disabled by settings: cam0, cam1 — no capture, bridge or live stream"
        );
        assert_eq!(cameras_disabled_event(&[]), None);
        let cams = vec![
            CaptureCamera {
                stream_key: "cam0".into(),
                camera_name: "/a".into(),
            },
            CaptureCamera {
                stream_key: "cam3".into(),
                camera_name: "/b".into(),
            },
        ];
        assert_eq!(enabled_cameras(cams, &m).len(), 1);
    }
}
