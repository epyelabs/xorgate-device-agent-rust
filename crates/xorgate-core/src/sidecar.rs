//! Video segment sidecars (plan D2/D6). Port of the pure parts of
//! `src/recording/video-segments.ts` (375 lines): splitmuxsink writes
//! `%05d.mp4` fragments into `recordings/video/<streamKey>/<sessionId>/`
//! and each CLOSED fragment becomes an upload-ready pair by getting a `.json`
//! sidecar next to it. "mp4 + sidecar exist" IS the pending-upload state.
//!
//! Contract (agent-anatomy invariant 12 / backend-device-contract §4):
//! sidecar PUT before mp4; `00000.mp4` drives thumbnails (so `seq` and the
//! zero-padded name must agree); `finalized:false` fragments (cut off by a
//! crash, maybe moov-less) STILL get a sidecar and ride to the cloud —
//! remux salvage is possible offline, deletion isn't. The S3 key
//! (`media-sessions/v1/<deviceId>/<streamKey>/<sessionId>/NNNNN.{json,mp4}`)
//! is built by `src/recording/video-source.ts:50-55` — Phase 3 (uploader).
//!
//! Out of core (Phase 3): `writeSessionManifest`/`readSessionManifest` file
//! I/O (`video-segments.ts:114-128`; the validation rule is
//! [`parse_session_manifest`]), the default `Date.now()/monotonicMs()` pair
//! (`:197,:263` — an explicit [`ClockPair`] here), the `readdirSync`/
//! `statSync`/`unlinkSync`/`atomicWriteJson` of `sweepSessionDir`
//! (`:248-327` — the DECISION is [`plan_sweep`] over a supplied listing) and
//! the directory walk of `recoverOrphanVideoSessions` (`:341-375` — its
//! per-session rule is [`plan_recover_orphan_sessions`]).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::clock::Clock;
use crate::round::js_math_round;
use crate::types::TimeSource;
use crate::video_capture::posix_join;

/// `#[serde(with = "js_num")]` for this module: WP1's
/// [`crate::telemetry::JsNumber`] prints integral doubles in [2^53, 1e21)
/// through ryu's exponent form (`1e+20`), where JS `JSON.stringify` prints
/// the shortest round-trip digits zero-padded (`100000000000000000000`;
/// `2**60` → `1152921504606847000`). Local workaround (WP1 bug reported in
/// the WP6 evidence file, `telemetry.rs:52`); everything else delegates.
mod js_num {
    use crate::telemetry::JsNumber;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        let a = v.abs();
        if v.is_finite() && v.fract() == 0.0 && (9_007_199_254_740_992.0..1e21).contains(&a) {
            // `{:e}` = shortest round-trip digits, e.g. `1.152921504606847e18`.
            let sci = format!("{a:e}");
            let (mant, exp) = sci.split_once('e').expect("{:e} has an exponent");
            let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
            let exp: usize = exp.parse().expect("positive exponent");
            let int = format!("{digits:0<width$}", width = exp + 1);
            let n: u128 = int.parse().expect("integer < 1e21");
            return if *v < 0.0 {
                s.serialize_i128(-(n as i128))
            } else {
                s.serialize_u128(n)
            };
        }
        JsNumber(*v).serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        f64::deserialize(d)
    }
}

/// `VideoSessionManifest` (`video-segments.ts:43-60`): per-session capture
/// metadata, written once at capture-daemon spawn as `session.json`. Key
/// order = interface order; every JS `number` is an `f64` printed like JS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSessionManifest {
    pub v: u8,
    pub device_id: String,
    pub stream_key: String,
    pub session_id: String,
    pub boot_id: String,
    pub time_source: TimeSource,
    pub preset: String,
    #[serde(with = "js_num")]
    pub width: f64,
    #[serde(with = "js_num")]
    pub height: f64,
    #[serde(with = "js_num")]
    pub fps: f64,
    #[serde(with = "js_num")]
    pub bitrate_kbps: f64,
    #[serde(with = "js_num")]
    pub segment_seconds: f64,
    /// Wall-clock epoch ms at capture-daemon spawn.
    #[serde(with = "js_num")]
    pub anchor_ts: f64,
    /// `process.hrtime.bigint()` ns at capture-daemon spawn (stringified bigint).
    pub anchor_mono: String,
}

/// `SESSION_MANIFEST_NAME` (`video-segments.ts:62`).
pub const SESSION_MANIFEST_NAME: &str = "session.json";

/// `anchor` (`video-segments.ts:99`): how `startTs` was derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidecarAnchor {
    /// mp4 birthtime.
    Fstat,
    /// `anchorTs + seq × segmentSeconds` arithmetic.
    Computed,
    /// Fragment 0 only: the NEXT fragment's birthtime minus the nominal
    /// segment length (Phase 6.6) — fragment 0's own birthtime is the
    /// capture-process spawn, ~2 s before real frames.
    Boundary,
}

/// The literal `codec: "h264"` of [`VideoSidecar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SidecarCodec {
    #[default]
    #[serde(rename = "h264")]
    H264,
}

/// `VideoSidecar` (`video-segments.ts:65-102`): README contract +
/// `anchor`/`finalized` provenance fields, in interface order. `startMono`
/// is derived from a wall/mono pair read at SIDECAR-WRITE time (Phase 8),
/// never from the session's spawn anchor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSidecar {
    pub v: u8,
    pub device_id: String,
    pub stream_key: String,
    pub session_id: String,
    #[serde(with = "js_num")]
    pub seq: f64,
    #[serde(with = "js_num")]
    pub start_ts: f64,
    #[serde(with = "js_num")]
    pub start_mono: f64,
    #[serde(with = "js_num")]
    pub duration_ms: f64,
    #[serde(with = "js_num")]
    pub size_bytes: f64,
    pub time_source: TimeSource,
    pub codec: SidecarCodec,
    #[serde(with = "js_num")]
    pub width: f64,
    #[serde(with = "js_num")]
    pub height: f64,
    #[serde(with = "js_num")]
    pub fps: f64,
    #[serde(with = "js_num")]
    pub bitrate_kbps: f64,
    pub anchor: SidecarAnchor,
    /// False when the fragment was cut off by a crash (may lack its moov atom).
    pub finalized: bool,
}

/// `sidecarPathFor` (`video-segments.ts:104-106`): `.mp4` → `.json`
/// (`replace(/\.mp4$/, ".json")`; a path without the suffix is unchanged).
pub fn sidecar_path_for(mp4_path: &str) -> String {
    match mp4_path.strip_suffix(".mp4") {
        Some(stem) => format!("{stem}.json"),
        None => mp4_path.to_string(),
    }
}

/// `segmentSeq` (`video-segments.ts:109-112`): splitmuxsink's fragment
/// number out of `00042.mp4` (`/^(\d{5,})\.mp4$/`, then `Number(...)`, hence
/// an `f64` — the JS number the sidecar's `seq` carries).
pub fn segment_seq(name: &str) -> Option<f64> {
    let digits = name.strip_suffix(".mp4")?;
    if digits.len() < 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<f64>().ok()
}

/// The validation inside `readSessionManifest` (`video-segments.ts:121-123`),
/// exactly: a truthy object (arrays included — `typeof [] === "object"`)
/// with `v === 1`, a string `sessionId` and a numeric `anchorTs`.
pub fn is_valid_session_manifest(raw: &Value) -> bool {
    let field = |k: &str| match raw {
        Value::Object(o) => o.get(k),
        _ => None,
    };
    matches!(raw, Value::Object(_) | Value::Array(_))
        && field("v").and_then(Value::as_f64) == Some(1.0)
        && field("sessionId").is_some_and(Value::is_string)
        && field("anchorTs").is_some_and(Value::is_number)
}

/// `readSessionManifest`'s result as a typed manifest: the TS rule
/// ([`is_valid_session_manifest`]) and then — where the TS returns the raw
/// object AS IS and lets `buildSidecar` copy `undefined`s through — every
/// declared field present and well-typed (WP6 evidence, ambiguity "strict
/// manifest"). `v: 1.0` is accepted like JS's `1.0 === 1`.
pub fn parse_session_manifest(raw: &Value) -> Option<VideoSessionManifest> {
    if !is_valid_session_manifest(raw) {
        return None;
    }
    let mut obj = raw.clone();
    obj["v"] = Value::from(1u8);
    serde_json::from_value(obj).ok()
}

/// `readSessionManifest` over the file TEXT (`JSON.parse` + the rule above);
/// unparseable text is `None` like the TS `catch`.
pub fn parse_session_manifest_text(text: &str) -> Option<VideoSessionManifest> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| parse_session_manifest(&v))
}

/// The three `statSync` fields `buildSidecar` reads (`video-segments.ts:137`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FragmentStat {
    pub size_bytes: f64,
    pub birthtime_ms: f64,
    pub mtime_ms: f64,
}

/// A wall/monotonic pair read within microseconds of each other
/// (`video-segments.ts:163`): both readings sit on the same side of any
/// clock correction, so `startMono` derived from it stays sane.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockPair {
    pub wall_ms: f64,
    pub mono_ms: f64,
}

impl ClockPair {
    /// The default `{ wallMs: Date.now(), monoMs: monotonicMs() }`.
    pub fn from_clock(clock: &dyn Clock) -> Self {
        Self {
            wall_ms: clock.wall_ms() as f64,
            mono_ms: clock.mono_ms() as f64,
        }
    }
}

/// `opts` of `buildSidecar` (`video-segments.ts:138-164`) with the clock
/// pair mandatory (no default clock in core).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SidecarOpts {
    pub finalized: bool,
    /// Birthtime of the NEXT fragment (the seq→seq+1 rotation point), passed
    /// by the sweep for fragment 0 only.
    pub boundary_end_ms: Option<f64>,
    /// Time provenance AT SIDECAR-WRITE time; absent (crash recovery of a
    /// dead session) = the manifest's spawn-time value.
    pub time_source: Option<TimeSource>,
    pub clock_pair: ClockPair,
}

/// Sanity bound for a birthtime/boundary: after 2001 (`video-segments.ts:169-171`).
const SANE_EPOCH_MS: f64 = 1_000_000_000_000.0;

/// `buildSidecar` (`video-segments.ts:134-217`): the sidecar for one closed
/// fragment from the manifest + file stats. Anchor precedence: a sane
/// `boundaryEndMs` → `boundary`; a sane birthtime → `fstat` (duration =
/// mtime − birthtime, floored at 0); else `computed` from the spawn anchor.
/// `startMono = round(pair.monoMs − (pair.wallMs − startTs))` — a fresh
/// pair, never `anchorMono + (startTs − anchorTs)` (which charged every
/// clock correction since spawn into a monotonic field).
pub fn build_sidecar(
    manifest: &VideoSessionManifest,
    seq: f64,
    stat: FragmentStat,
    opts: &SidecarOpts,
) -> VideoSidecar {
    // ext4 gives real birthtimes; a zero/bogus one (some tmpfs, copied trees)
    // falls back to anchor arithmetic — that's exactly what `anchor` records.
    let nominal_start = manifest.anchor_ts + seq * manifest.segment_seconds * 1000.0;
    let have_birth = stat.birthtime_ms > SANE_EPOCH_MS;
    let have_boundary = opts.boundary_end_ms.is_some_and(|b| b > SANE_EPOCH_MS);
    let (start_ts, duration_ms, anchor) = if have_boundary {
        let duration = manifest.segment_seconds * 1000.0;
        let boundary = opts.boundary_end_ms.expect("checked");
        (
            js_math_round(boundary - duration),
            duration,
            SidecarAnchor::Boundary,
        )
    } else if have_birth {
        let start = js_math_round(stat.birthtime_ms);
        let end = if stat.mtime_ms > start {
            stat.mtime_ms
        } else {
            start
        };
        (start, js_math_round(end - start), SidecarAnchor::Fstat)
    } else {
        (
            nominal_start,
            manifest.segment_seconds * 1000.0,
            SidecarAnchor::Computed,
        )
    };
    let pair = opts.clock_pair;
    VideoSidecar {
        v: 1,
        device_id: manifest.device_id.clone(),
        stream_key: manifest.stream_key.clone(),
        session_id: manifest.session_id.clone(),
        seq,
        start_ts,
        start_mono: js_math_round(pair.mono_ms - (pair.wall_ms - start_ts)),
        duration_ms,
        size_bytes: stat.size_bytes,
        time_source: opts.time_source.unwrap_or(manifest.time_source),
        codec: SidecarCodec::H264,
        width: manifest.width,
        height: manifest.height,
        fps: manifest.fps,
        bitrate_kbps: manifest.bitrate_kbps,
        anchor,
        finalized: opts.finalized,
    }
}

/// `SweepResult` (`video-segments.ts:219-224`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepResult {
    /// Sidecars written this sweep (mp4 paths).
    pub closed: Vec<String>,
    /// The newest (active) fragment path, or `None` when the dir has no mp4s.
    pub active_path: Option<String>,
}

/// One `readdirSync` name with its `statSync` result (`None` = the stat
/// threw: the fragment raced an eviction). Non-mp4 names carry whatever the
/// caller has; only `.json` names and `NNNNN.mp4` names matter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stat: Option<FragmentStat>,
}

/// `opts` of `sweepSessionDir` (`video-segments.ts:238-244`), clock pair mandatory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SweepOpts {
    /// Capture stopped (EOS finalised the last fragment) or orphan recovery:
    /// the newest fragment is closed too.
    pub include_newest: bool,
    /// Override for every sidecar's `finalized`; absent = `!isNewest`.
    pub finalized: Option<bool>,
    pub time_source: Option<TimeSource>,
    pub clock_pair: ClockPair,
}

/// One sidecar the I/O layer must `atomicWriteJson` (sidecar BEFORE the
/// fragment counts as closed — invariant 12).
#[derive(Debug, Clone, PartialEq)]
pub struct SidecarWrite {
    pub mp4_path: String,
    pub sidecar_path: String,
    pub sidecar: VideoSidecar,
}

/// What one `sweepSessionDir` pass does, as data.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SweepPlan {
    /// Sidecars to write, in fragment order.
    pub writes: Vec<SidecarWrite>,
    /// 0-byte fragments to `unlinkSync` (final sweep only); the TS logs
    /// `[recording] dropped empty fragment <path>` for each.
    pub reap: Vec<String>,
    /// `activePath` of the result.
    pub active_path: Option<String>,
}

impl SweepPlan {
    /// The `SweepResult` the TS returns once every write succeeded
    /// (`closed` is appended per successful `atomicWriteJson`, `:322-323`).
    pub fn result(&self) -> SweepResult {
        SweepResult {
            closed: self.writes.iter().map(|w| w.mp4_path.clone()).collect(),
            active_path: self.active_path.clone(),
        }
    }
}

/// The decision part of `sweepSessionDir` (`video-segments.ts:235-332`)
/// over a supplied listing: every `.mp4` except the newest is closed —
/// plan missing sidecars; the newest is the fragment splitmuxsink is still
/// writing unless `include_newest`. Idempotent: fragments that already have
/// a sidecar are skipped. Fragment 0 gets a boundary anchor from fragment
/// 1's birthtime (`:281-288`); 0-byte fragments are never sidecar'd and are
/// reaped only on a final sweep (`:297-307`).
///
/// An unreadable dir is the empty listing (`:247-251` → empty plan).
pub fn plan_sweep(
    dir: &str,
    manifest: &VideoSessionManifest,
    entries: &[DirEntry],
    opts: &SweepOpts,
) -> SweepPlan {
    // `.map(segmentSeq).filter(seq !== null).sort((a, b) => a.seq - b.seq)` — stable.
    let mut frags: Vec<(&DirEntry, f64)> = entries
        .iter()
        .filter_map(|e| segment_seq(&e.name).map(|seq| (e, seq)))
        .collect();
    frags.sort_by(|a, b| a.1.partial_cmp(&b.1).expect("finite seq"));
    let Some(newest) = frags.last().map(|f| f.0) else {
        return SweepPlan::default();
    };
    let sidecars: Vec<&str> = entries
        .iter()
        .filter(|e| e.name.ends_with(".json"))
        .map(|e| e.name.as_str())
        .collect();
    let mut plan = SweepPlan {
        active_path: (!opts.include_newest).then(|| posix_join(dir, &newest.name)),
        ..SweepPlan::default()
    };
    for (i, (f, seq)) in frags.iter().enumerate() {
        let is_newest = f.name == newest.name;
        if is_newest && !opts.include_newest {
            continue;
        }
        let mp4_path = posix_join(dir, &f.name);
        if sidecars.contains(&sidecar_path_for(&f.name).as_str()) {
            continue;
        }
        let Some(st) = f.stat else {
            continue; // raced an eviction — skip
        };
        // Fragment 0's boundary anchor (Phase 6.6): the 0→1 rotation point is
        // the NEXT fragment's birthtime; a next fragment that raced an
        // eviction (no stat) falls back to fstat anchoring.
        let boundary_end_ms = if *seq == 0.0 && i + 1 < frags.len() {
            frags[i + 1].0.stat.map(|s| s.birthtime_ms)
        } else {
            None
        };
        // A 0-byte fragment (power cut, or a source that never delivered a
        // frame): nothing to salvage, a sidecar would send garbage through
        // the uploader. Reaped only when the session is closed; live, the
        // newest is still filling.
        if st.size_bytes == 0.0 {
            if opts.include_newest {
                plan.reap.push(mp4_path);
            }
            continue;
        }
        let sidecar = build_sidecar(
            manifest,
            *seq,
            st,
            &SidecarOpts {
                // The newest fragment of a dead session may have been cut
                // mid-write; all others were finalised by splitmuxsink.
                finalized: opts.finalized.unwrap_or(!is_newest),
                boundary_end_ms,
                time_source: opts.time_source,
                clock_pair: opts.clock_pair,
            },
        );
        plan.writes.push(SidecarWrite {
            sidecar_path: sidecar_path_for(&mp4_path),
            mp4_path,
            sidecar,
        });
    }
    plan
}

/// One session directory as `recoverOrphanVideoSessions` walks it
/// (`video-segments.ts:352-372`): `<videoRoot>/<streamKey>/<sessionId>/`.
/// The caller lists only DIRECTORIES (the TS skips non-dirs, `:356,:365`)
/// and reads `session.json` through [`parse_session_manifest_text`].
#[derive(Debug, Clone, PartialEq)]
pub struct SessionListing {
    pub stream_key: String,
    pub session_id: String,
    /// `None` = no readable manifest: left untouched (can't attribute
    /// metadata; the disk manager ages it out).
    pub manifest: Option<VideoSessionManifest>,
    pub entries: Vec<DirEntry>,
}

/// The plans of a startup crash recovery.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecoverPlan {
    /// `(session dir, plan)` for every session that gets swept, in listing order.
    pub sweeps: Vec<(String, SweepPlan)>,
    /// `recovered`: the number of sidecars written (sum of `writes`).
    pub recovered: usize,
}

/// The per-session rule of `recoverOrphanVideoSessions`
/// (`video-segments.ts:341-375`): every session dir NOT in
/// `active_session_ids` belongs to a previous run — sidecar-backfill all of
/// its fragments (`includeNewest: true`, so the newest is tagged
/// `finalized:false` and 0-byte fragments are reaped). Sessions without a
/// manifest are skipped. Each swept session reads its OWN clock pair, as
/// each TS `sweepSessionDir` call defaults one (`:263`).
pub fn plan_recover_orphan_sessions(
    video_root: &str,
    sessions: &[SessionListing],
    active_session_ids: &[String],
    clock: &dyn Clock,
) -> RecoverPlan {
    let mut out = RecoverPlan::default();
    for s in sessions {
        if active_session_ids.contains(&s.session_id) {
            continue;
        }
        let Some(manifest) = &s.manifest else {
            continue;
        };
        let dir = posix_join(&posix_join(video_root, &s.stream_key), &s.session_id);
        let plan = plan_sweep(
            &dir,
            manifest,
            &s.entries,
            &SweepOpts {
                include_newest: true,
                finalized: None,
                time_source: None,
                clock_pair: ClockPair::from_clock(clock),
            },
        );
        out.recovered += plan.writes.len();
        out.sweeps.push((dir, plan));
    }
    out
}

/// The `console.warn` for a reaped 0-byte fragment (`video-segments.ts:301`).
pub fn dropped_empty_fragment_warning(mp4_path: &str) -> String {
    format!("[recording] dropped empty fragment {mp4_path}")
}

/// The `console.error` for a failed sidecar write (`video-segments.ts:325`);
/// such a fragment is NOT counted in `closed`.
pub fn sidecar_write_failed_error(mp4_path: &str, message: &str) -> String {
    format!("[recording] sidecar write failed for {mp4_path}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: f64 = 1_750_000_000_000.0;

    fn manifest() -> VideoSessionManifest {
        VideoSessionManifest {
            v: 1,
            device_id: "dev-1".into(),
            stream_key: "cam0".into(),
            session_id: "sess-1".into(),
            boot_id: "boot-1".into(),
            time_source: TimeSource::Ntp,
            preset: "1080p15-2mbps".into(),
            width: 1920.0,
            height: 1080.0,
            fps: 15.0,
            bitrate_kbps: 2000.0,
            segment_seconds: 60.0,
            anchor_ts: T0,
            anchor_mono: "500000000000".into(),
        }
    }

    #[test]
    fn js_number_printing_above_2_pow_53() {
        #[derive(Serialize)]
        struct N(#[serde(with = "super::js_num")] f64);
        let p = |v: f64| serde_json::to_string(&N(v)).unwrap();
        assert_eq!(p(1e20), "100000000000000000000");
        assert_eq!(p(2f64.powi(60)), "1152921504606847000");
        assert_eq!(p(-2f64.powi(60)), "-1152921504606847000");
        assert_eq!(p(9_007_199_254_740_992.0), "9007199254740992");
        assert_eq!(p(1e21), "1e+21");
        assert_eq!(p(1_750_000_000_000.0), "1750000000000");
        assert_eq!(p(0.5), "0.5");
    }

    #[test]
    fn seq_and_paths() {
        assert_eq!(segment_seq("00007.mp4"), Some(7.0));
        assert_eq!(segment_seq("session.json"), None);
        assert_eq!(segment_seq("00007.json"), None);
        assert_eq!(segment_seq("notes.mp4"), None);
        assert_eq!(segment_seq("0007.mp4"), None);
        assert_eq!(segment_seq("000070.mp4"), Some(70.0));
        assert_eq!(sidecar_path_for("/a/b/00007.mp4"), "/a/b/00007.json");
        assert_eq!(sidecar_path_for("/a/b/x.bin"), "/a/b/x.bin");
    }

    #[test]
    fn manifest_validation_and_byte_shape() {
        let m = manifest();
        let text = serde_json::to_string(&m).unwrap();
        assert_eq!(
            text,
            r#"{"v":1,"deviceId":"dev-1","streamKey":"cam0","sessionId":"sess-1","bootId":"boot-1","timeSource":"ntp","preset":"1080p15-2mbps","width":1920,"height":1080,"fps":15,"bitrateKbps":2000,"segmentSeconds":60,"anchorTs":1750000000000,"anchorMono":"500000000000"}"#
        );
        assert_eq!(parse_session_manifest_text(&text), Some(m.clone()));
        assert_eq!(parse_session_manifest_text("{broken"), None);
        assert_eq!(parse_session_manifest_text("null"), None);
        assert_eq!(parse_session_manifest_text("[1]"), None);
        let mut v: Value = serde_json::from_str(&text).unwrap();
        v["v"] = Value::from(2);
        assert_eq!(parse_session_manifest(&v), None);
        v["v"] = Value::from(1.0);
        assert_eq!(parse_session_manifest(&v), Some(m.clone()));
        v["anchorTs"] = Value::from("x");
        assert_eq!(parse_session_manifest(&v), None);
    }

    #[test]
    fn sidecar_byte_shape() {
        let sc = build_sidecar(
            &manifest(),
            2.0,
            FragmentStat {
                size_bytes: 123.0,
                birthtime_ms: T0 + 120_000.0,
                mtime_ms: T0 + 180_000.0,
            },
            &SidecarOpts {
                finalized: true,
                boundary_end_ms: None,
                time_source: None,
                clock_pair: ClockPair {
                    wall_ms: T0 + 180_000.0,
                    mono_ms: 500_000.0 + 180_000.0,
                },
            },
        );
        assert_eq!(
            serde_json::to_string(&sc).unwrap(),
            r#"{"v":1,"deviceId":"dev-1","streamKey":"cam0","sessionId":"sess-1","seq":2,"startTs":1750000120000,"startMono":620000,"durationMs":60000,"sizeBytes":123,"timeSource":"ntp","codec":"h264","width":1920,"height":1080,"fps":15,"bitrateKbps":2000,"anchor":"fstat","finalized":true}"#
        );
        let back: VideoSidecar =
            serde_json::from_str(&serde_json::to_string(&sc).unwrap()).unwrap();
        assert_eq!(back, sc);
    }

    #[test]
    fn sweep_marks_unfinalized_newest_and_reaps_empty_on_close() {
        let entry = |i: u32, size: f64| DirEntry {
            name: format!("{i:05}.mp4"),
            stat: Some(FragmentStat {
                size_bytes: size,
                birthtime_ms: T0 + f64::from(i) * 60_000.0,
                mtime_ms: T0 + f64::from(i + 1) * 60_000.0,
            }),
        };
        let pair = ClockPair {
            wall_ms: T0 + 240_000.0,
            mono_ms: 740_000.0,
        };
        let entries = vec![
            DirEntry {
                name: "session.json".into(),
                stat: None,
            },
            entry(2, 0.0),
            entry(0, 1000.0),
            entry(1, 1001.0),
        ];
        let live = plan_sweep(
            "/v/cam0/sess-1",
            &manifest(),
            &entries,
            &SweepOpts {
                include_newest: false,
                finalized: None,
                time_source: None,
                clock_pair: pair,
            },
        );
        assert_eq!(
            live.result().closed,
            vec!["/v/cam0/sess-1/00000.mp4", "/v/cam0/sess-1/00001.mp4"]
        );
        assert_eq!(
            live.active_path.as_deref(),
            Some("/v/cam0/sess-1/00002.mp4")
        );
        assert_eq!(live.writes[0].sidecar.anchor, SidecarAnchor::Boundary);
        assert_eq!(live.writes[0].sidecar.start_ts, T0);
        assert!(live.writes[0].sidecar.finalized && live.writes[1].sidecar.finalized);
        assert_eq!(live.writes[0].sidecar_path, "/v/cam0/sess-1/00000.json");
        assert!(live.reap.is_empty());

        // Second pass with the sidecars present: idempotent.
        let mut again = entries.clone();
        again.push(DirEntry {
            name: "00000.json".into(),
            stat: None,
        });
        again.push(DirEntry {
            name: "00001.json".into(),
            stat: None,
        });
        let closing = plan_sweep(
            "/v/cam0/sess-1",
            &manifest(),
            &again,
            &SweepOpts {
                include_newest: true,
                finalized: None,
                time_source: Some(TimeSource::Gps),
                clock_pair: pair,
            },
        );
        assert!(closing.writes.is_empty());
        assert_eq!(closing.reap, vec!["/v/cam0/sess-1/00002.mp4"]);
        assert_eq!(closing.active_path, None);

        // A dead session: newest (non-empty) is unfinalized, next-fragment stat missing → fstat.
        let dead = vec![
            entry(0, 10.0),
            DirEntry {
                name: "00001.mp4".into(),
                stat: None,
            },
            entry(2, 5.0),
        ];
        let rec = plan_recover_orphan_sessions(
            "/v",
            &[
                SessionListing {
                    stream_key: "cam0".into(),
                    session_id: "sess-1".into(),
                    manifest: Some(manifest()),
                    entries: dead,
                },
                SessionListing {
                    stream_key: "cam1".into(),
                    session_id: "active".into(),
                    manifest: Some(manifest()),
                    entries: vec![entry(0, 1.0)],
                },
                SessionListing {
                    stream_key: "cam1".into(),
                    session_id: "nomanifest".into(),
                    manifest: None,
                    entries: vec![entry(0, 1.0)],
                },
            ],
            &["active".to_string()],
            &crate::clock::FixedClock::new(1_750_000_240_000, 740_000),
        );
        assert_eq!(rec.recovered, 2);
        assert_eq!(rec.sweeps.len(), 1);
        let plan = &rec.sweeps[0].1;
        assert_eq!(rec.sweeps[0].0, "/v/cam0/sess-1");
        assert_eq!(plan.writes[0].sidecar.anchor, SidecarAnchor::Fstat);
        assert!(plan.writes[0].sidecar.finalized);
        assert!(!plan.writes[1].sidecar.finalized);
        assert_eq!(plan.writes[1].sidecar.seq, 2.0);
    }
}
