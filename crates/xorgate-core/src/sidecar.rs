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
//! [`is_valid_session_manifest`], the result [`read_session_manifest`]), the default `Date.now()/monotonicMs()` pair
//! (`:197,:263` — an explicit [`ClockPair`] here), the `readdirSync`/
//! `statSync`/`unlinkSync`/`atomicWriteJson` of `sweepSessionDir`
//! (`:248-327` — the DECISION is [`plan_sweep`] over a supplied listing) and
//! the directory walk of `recoverOrphanVideoSessions` (`:341-375` — its
//! per-session rule is [`plan_recover_orphan_sessions`]).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::clock::Clock;
use crate::js::{trim as js_trim, JsJsonMap};
use crate::round::js_math_round;
use crate::telemetry::js_num;
use crate::types::TimeSource;
use crate::video_capture::posix_join;

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
    /// Phase 7a (Q-A2): the capture host's exact stamp of the fragment's
    /// first frame (`XG-FRAGMENT`, buffer PTS on the pipeline clock mapped to
    /// wall time). Emitted only with `XORGATE_SIDECAR_TIMING=exact`, through
    /// [`EXACT_ANCHOR`]. The backend enum must accept `"pts"` BEFORE any
    /// device enables `exact` (an unknown value DLQs the segment, K15).
    Pts,
}

/// The `anchor` every exact-timed sidecar carries (phase-7a §8.7; user
/// answer Q-A2 = a new `"pts"`). One constant so the label is a one-line
/// change, pinned by `exact_anchor_is_pts`.
pub const EXACT_ANCHOR: SidecarAnchor = SidecarAnchor::Pts;

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

/// A manifest as a typed [`VideoSessionManifest`]: the TS rule
/// ([`is_valid_session_manifest`]) AND every declared field present and
/// well-typed. Stricter than `readSessionManifest` — decisions that must
/// match the TS (orphan recovery) go through [`read_session_manifest`].
/// `v: 1.0` is accepted like JS's `1.0 === 1`.
pub fn parse_session_manifest(raw: &Value) -> Option<VideoSessionManifest> {
    if !is_valid_session_manifest(raw) {
        return None;
    }
    let mut obj = raw.clone();
    obj["v"] = Value::from(1u8);
    serde_json::from_value(obj).ok()
}

/// [`parse_session_manifest`] over the file TEXT; unparseable text is `None`.
pub fn parse_session_manifest_text(text: &str) -> Option<VideoSessionManifest> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| parse_session_manifest(&v))
}

/// What `readSessionManifest` (`video-segments.ts:118-128`) hands to
/// `sweepSessionDir`: the parsed object, gated ONLY by the TS rule
/// ([`is_valid_session_manifest`]). The TS returns that object as is, so a
/// manifest with a missing, mistyped or extra field (or an unknown
/// `timeSource`) is still swept and its oddities ride into the sidecars;
/// `Raw` carries such an object so the port does the same (WP6 review S1).
#[derive(Debug, Clone, PartialEq)]
pub enum SessionManifest {
    /// Every declared field present and well-typed ([`parse_session_manifest`]).
    Typed(VideoSessionManifest),
    /// Passes the TS rule but not the typed parser: the object as parsed.
    Raw(Map<String, Value>),
}

impl From<VideoSessionManifest> for SessionManifest {
    fn from(m: VideoSessionManifest) -> Self {
        SessionManifest::Typed(m)
    }
}

/// `readSessionManifest` over a parsed value: `None` iff the TS returns
/// `null`; `Typed` whenever the typed parser accepts it (then
/// `buildSidecar` over the raw object and over the typed one agree byte for
/// byte), else `Raw`.
pub fn read_session_manifest(raw: &Value) -> Option<SessionManifest> {
    if !is_valid_session_manifest(raw) {
        return None;
    }
    if let Some(m) = parse_session_manifest(raw) {
        return Some(SessionManifest::Typed(m));
    }
    match raw {
        Value::Object(o) => Some(SessionManifest::Raw(o.clone())),
        _ => None, // unreachable: the rule needs object fields
    }
}

/// `readSessionManifest` over the file TEXT (`JSON.parse` + the TS rule);
/// unparseable text is `None` like the TS `catch`.
pub fn read_session_manifest_text(text: &str) -> Option<SessionManifest> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| read_session_manifest(&v))
}

/// ECMAScript `StringToNumber` (the `Number("…")` coercion): JS-trimmed;
/// empty → 0; `Infinity` with an optional sign; unsigned `0x`/`0o`/`0b`
/// integers; else a decimal literal (`1`, `1.`, `.5`, `1e3`, optional sign)
/// — anything else (`inf`, `1_0`, `-0x10`) is NaN.
fn js_string_to_number(s: &str) -> f64 {
    let t = js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = match t.get(..2) {
        Some("0x" | "0X") => Some(16),
        Some("0o" | "0O") => Some(8),
        Some("0b" | "0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        let digits = &t[2..];
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return f64::NAN;
        }
        return digits.chars().fold(0.0, |acc, c| {
            acc * f64::from(radix) + f64::from(c.to_digit(radix).expect("checked"))
        });
    }
    let b = t.as_bytes();
    let mut i = usize::from(matches!(b[0], b'+' | b'-'));
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut mantissa_digits = i - int_start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        mantissa_digits += i - frac_start;
    }
    if mantissa_digits == 0 {
        return f64::NAN;
    }
    if i < b.len() && matches!(b[i], b'e' | b'E') {
        i += 1;
        if i < b.len() && matches!(b[i], b'+' | b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return f64::NAN;
        }
    }
    if i != b.len() {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// ECMAScript `ToNumber` of a JSON value (`None` = `undefined`), as the
/// arithmetic in `buildSidecar` applies it to `manifest.segmentSeconds`:
/// undefined → NaN, null → 0, booleans → 0/1, strings → StringToNumber,
/// arrays → StringToNumber of `String(array)` (`[]` → 0, `[x]` → via `x`,
/// two or more elements always carry a comma → NaN), objects → NaN.
fn js_to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => js_string_to_number(s),
        Some(Value::Array(a)) => match a.as_slice() {
            [] => 0.0,
            // `String([x])` = `String(x)`, except null → "" and `true` → "true".
            [Value::Null] => 0.0,
            [Value::Bool(_)] | [Value::Object(_)] => f64::NAN,
            [x] => js_to_number(Some(x)),
            _ => f64::NAN,
        },
        Some(Value::Object(_)) => f64::NAN,
    }
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
    build_sidecar_typed(manifest, seq, stat, opts, None)
}

/// [`build_sidecar`] with an optional exact override (phase-7a); `None` is
/// the legacy derivation, byte for byte.
fn build_sidecar_typed(
    manifest: &VideoSessionManifest,
    seq: f64,
    stat: FragmentStat,
    opts: &SidecarOpts,
    exact: Option<ExactTimes>,
) -> VideoSidecar {
    let (start_ts, duration_ms, anchor) = sidecar_times(
        manifest.anchor_ts,
        manifest.segment_seconds,
        seq,
        stat,
        opts,
        exact,
    );
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

/// The `startTs`/`durationMs`/`anchor` derivation of `buildSidecar`
/// (`video-segments.ts:170-191`), shared by the typed and the raw build.
/// `segment_seconds` is the JS `ToNumber` of the manifest field.
///
/// Phase 7a: `exact` (computed by [`exact_times`] from the capture host's
/// stamps) takes precedence and is labelled [`EXACT_ANCHOR`]; `None` runs
/// the TS derivation below unchanged.
fn sidecar_times(
    anchor_ts: f64,
    segment_seconds: f64,
    seq: f64,
    stat: FragmentStat,
    opts: &SidecarOpts,
    exact: Option<ExactTimes>,
) -> (f64, f64, SidecarAnchor) {
    if let Some(x) = exact {
        return (x.start_ts, x.duration_ms, EXACT_ANCHOR);
    }
    // ext4 gives real birthtimes; a zero/bogus one (some tmpfs, copied trees)
    // falls back to anchor arithmetic — that's exactly what `anchor` records.
    let nominal_start = anchor_ts + seq * segment_seconds * 1000.0;
    let have_birth = stat.birthtime_ms > SANE_EPOCH_MS;
    let have_boundary = opts.boundary_end_ms.is_some_and(|b| b > SANE_EPOCH_MS);
    if have_boundary {
        let duration = segment_seconds * 1000.0;
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
            segment_seconds * 1000.0,
            SidecarAnchor::Computed,
        )
    }
}

/// A sidecar as `buildSidecar` returns it for either kind of
/// [`SessionManifest`]. `Raw` is the object `JSON.stringify` would see, in
/// interface key order: copy-through keys the manifest lacks are absent
/// (`undefined` is dropped), non-finite numbers are already `null`.
#[derive(Debug, Clone, PartialEq)]
pub enum SidecarBody {
    Typed(VideoSidecar),
    Raw(Map<String, Value>),
}

impl SidecarBody {
    /// The bytes `atomicWriteJson` writes (compact `JSON.stringify`; JSON
    /// numbers printed like JS).
    pub fn to_json(&self) -> String {
        match self {
            SidecarBody::Typed(sc) => serde_json::to_string(sc),
            SidecarBody::Raw(m) => serde_json::to_string(&JsJsonMap(m)),
        }
        .expect("a sidecar always serializes")
    }

    /// The typed sidecar, when the manifest was well-typed.
    pub fn typed(&self) -> Option<&VideoSidecar> {
        match self {
            SidecarBody::Typed(sc) => Some(sc),
            SidecarBody::Raw(_) => None,
        }
    }
}

/// `buildSidecar` over either kind of manifest. A `Raw` manifest is read the
/// way the TS reads it: `anchorTs` is a number (the gate), `segmentSeconds`
/// goes through JS `ToNumber` in the arithmetic, and `deviceId`,
/// `streamKey`, `sessionId`, `timeSource` (`opts.timeSource ?? …`), `width`,
/// `height`, `fps`, `bitrateKbps` are copied as they are (absent → key
/// omitted, any JSON value → that value).
pub fn build_sidecar_body(
    manifest: &SessionManifest,
    seq: f64,
    stat: FragmentStat,
    opts: &SidecarOpts,
) -> SidecarBody {
    build_sidecar_body_exact(manifest, seq, stat, opts, None)
}

/// [`build_sidecar_body`] with an optional exact override (phase-7a):
/// `Some` replaces `startTs`/`durationMs`/`anchor` (= [`EXACT_ANCHOR`]) and
/// nothing else — `startMono` keeps its derivation from the write-time pair,
/// so it moves with `startTs` by construction (§8.6). `None` = legacy, byte
/// for byte.
pub fn build_sidecar_body_exact(
    manifest: &SessionManifest,
    seq: f64,
    stat: FragmentStat,
    opts: &SidecarOpts,
    exact: Option<ExactTimes>,
) -> SidecarBody {
    let m = match manifest {
        SessionManifest::Typed(m) => {
            return SidecarBody::Typed(build_sidecar_typed(m, seq, stat, opts, exact))
        }
        SessionManifest::Raw(m) => m,
    };
    let anchor_ts = m
        .get("anchorTs")
        .and_then(Value::as_f64)
        .unwrap_or(f64::NAN);
    let (start_ts, duration_ms, anchor) = sidecar_times(
        anchor_ts,
        js_to_number(m.get("segmentSeconds")),
        seq,
        stat,
        opts,
        exact,
    );
    let pair = opts.clock_pair;
    let start_mono = js_math_round(pair.mono_ms - (pair.wall_ms - start_ts));
    let mut out = Map::new();
    let copy = |out: &mut Map<String, Value>, k: &str| {
        if let Some(v) = m.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    };
    out.insert("v".into(), Value::from(1u8));
    copy(&mut out, "deviceId");
    copy(&mut out, "streamKey");
    copy(&mut out, "sessionId");
    out.insert("seq".into(), Value::from(seq));
    out.insert("startTs".into(), Value::from(start_ts));
    out.insert("startMono".into(), Value::from(start_mono));
    out.insert("durationMs".into(), Value::from(duration_ms));
    out.insert("sizeBytes".into(), Value::from(stat.size_bytes));
    match opts.time_source {
        Some(ts) => {
            out.insert("timeSource".into(), serde_json::to_value(ts).expect("enum"));
        }
        None => copy(&mut out, "timeSource"),
    }
    out.insert("codec".into(), Value::from("h264"));
    copy(&mut out, "width");
    copy(&mut out, "height");
    copy(&mut out, "fps");
    copy(&mut out, "bitrateKbps");
    out.insert("anchor".into(), serde_json::to_value(anchor).expect("enum"));
    out.insert("finalized".into(), Value::from(opts.finalized));
    SidecarBody::Raw(out)
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
    pub sidecar: SidecarBody,
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
    /// Phase 7a, `exact` only: closed fragments (seq) NOT sidecar'd this
    /// pass because their own or their successor's stamp may still be in
    /// flight (§8.5). Always empty for legacy plans and final sweeps.
    pub deferred: Vec<u64>,
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
    manifest: &SessionManifest,
    entries: &[DirEntry],
    opts: &SweepOpts,
) -> SweepPlan {
    plan_sweep_timed(dir, manifest, entries, opts, None)
}

/// [`plan_sweep`] with the run's exact timing (phase-7a §8.4–8.5). `None`
/// is the legacy plan, byte for byte (`plan_sweep` is exactly this with
/// `None`). With `Some`:
/// - a fragment with an [`Stamp::Exact`] stamp gets [`exact_times`]
///   (anchor [`EXACT_ANCHOR`]); one without (lost line, rejected stamp, an
///   untimed session) gets the legacy derivation, fragment 0's `boundary`
///   walk-back included — for that fragment only;
/// - on a LIVE sweep (`!include_newest`) a closed fragment whose own stamp
///   or successor's stamp is [`TimingTable::pending`] is deferred
///   (`deferred`), until it has been deferred for `defer_max_ms`; then it is
///   written with the fallback. The FINAL sweep (`include_newest`) never
///   defers.
pub fn plan_sweep_timed(
    dir: &str,
    manifest: &SessionManifest,
    entries: &[DirEntry],
    opts: &SweepOpts,
    exact: Option<&ExactSweep<'_>>,
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
        // The newest fragment of a dead session may have been cut
        // mid-write; all others were finalised by splitmuxsink.
        let finalized = opts.finalized.unwrap_or(!is_newest);
        let exact_times = match exact {
            None => None,
            Some(x) => match x.decide(*seq, finalized, st.mtime_ms, opts.include_newest) {
                ExactDecision::Defer(id) => {
                    plan.deferred.push(id);
                    continue;
                }
                ExactDecision::Exact(t) => Some(t),
                ExactDecision::Legacy => None,
            },
        };
        let sidecar = build_sidecar_body_exact(
            manifest,
            *seq,
            st,
            &SidecarOpts {
                finalized,
                boundary_end_ms,
                time_source: opts.time_source,
                clock_pair: opts.clock_pair,
            },
            exact_times,
        );
        plan.writes.push(SidecarWrite {
            sidecar_path: sidecar_path_for(&mp4_path),
            mp4_path,
            sidecar,
        });
    }
    plan
}

// ---------------------------------------------------------------------------
// Phase 7a: exact fragment timing (phase-7a-inprocess-gstreamer.md §8.1–8.7)
//
// The capture host stamps every fragment's first frame in splitmuxsink's
// `format-location-full` (`XG-FRAGMENT {id, startTs, lagMs, …}`) and the
// clean-EOS end of the last one (`XG-FRAGMENT-END {id, endTs, …}`). Both are
// "host wall at callback − measured mono lag", so a wall-clock step moves
// them exactly as it moves a birthtime. The agent keeps them per run in a
// [`TimingTable`]; the sweep turns them into `startTs`/`durationMs` here.
// No media-time (PTS) quantity ever reaches this code: [`FragmentTiming`]
// carries wall stamps and the lag only, so `durationMs` is wall-derived by
// construction and the backend's clock-step detection
// (`clock-step.ts:50-62`, `durationMs > nominal + minStep`) keeps working.

/// The GOP of every capture preset: `key-int-max = 2 × fps`
/// (`video_capture.rs`, [`crate::video_capture::build_capture_pipeline_args`]).
pub const CAPTURE_GOP_MS: f64 = 2_000.0;

/// §8.2 validity guard: a stamp is accepted only if
/// `0 ≤ lagMs ≤ 3 × GOP + 1 s`. Outside it the clock domain is wrong and
/// the fragment keeps the legacy anchors. The host applies the guard
/// (WP1); the agent re-checks every line it is handed.
pub const LAG_GUARD_MAX_MS: f64 = 3.0 * CAPTURE_GOP_MS + 1_000.0;

/// One fragment's accepted stamps (all integer epoch ms / ms).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FragmentTiming {
    /// Wall time of the fragment's first frame (`XG-FRAGMENT.startTs`).
    pub start_ts: f64,
    /// Host-measured `clock_now − frameMono` at the callback (`lagMs`).
    pub lag_ms: f64,
    /// Wall time of the end of the last frame (`XG-FRAGMENT-END.endTs`),
    /// clean EOS of the final fragment only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ts: Option<f64>,
}

/// Why a timing line was not accepted (the agent warns once per session).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StampReject {
    /// `lagMs` outside `[0, LAG_GUARD_MAX_MS]` (or not a number).
    Lag(f64),
    /// `startTs` not a sane epoch ms (≤ 2001 or not a number).
    StartTs(f64),
    /// `endTs` not a sane epoch ms (the start stamp is kept).
    EndTs(f64),
    /// A second `XG-FRAGMENT` for an id already recorded (first wins).
    Duplicate,
    /// `XG-FRAGMENT-END` for an id with no accepted start stamp.
    EndWithoutStart,
}

impl std::fmt::Display for StampReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StampReject::Lag(v) => write!(
                f,
                "lagMs {} outside [0, {}]",
                js_num_text(*v),
                js_num_text(LAG_GUARD_MAX_MS)
            ),
            StampReject::StartTs(v) => write!(f, "startTs {} is not a sane epoch", js_num_text(*v)),
            StampReject::EndTs(v) => write!(f, "endTs {} is not a sane epoch", js_num_text(*v)),
            StampReject::Duplicate => f.write_str("duplicate fragment id"),
            StampReject::EndWithoutStart => f.write_str("end stamp without a start stamp"),
        }
    }
}

fn js_num_text(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// What the run knows about one fragment id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stamp {
    Exact(FragmentTiming),
    /// A line arrived but failed the guard: legacy anchors, nothing to wait for.
    Rejected,
}

/// The run's per-fragment timing, keyed by splitmuxsink's `fragment_id`
/// (= the `%05d` seq). Cleared per spawn: every spawn mints a new session
/// dir, so ids never carry across a respawn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimingTable {
    stamps: std::collections::BTreeMap<u64, Stamp>,
    /// The highest id any line (accepted or rejected) carried. The host
    /// writes lines in id order on one pipe, so a missing id BELOW it was
    /// never emitted (lost), not in flight.
    max_id: Option<u64>,
}

/// A JS-number ms as an integer, or `None` when not finite.
fn int_ms(v: f64) -> Option<f64> {
    v.is_finite().then(|| js_math_round(v))
}

impl TimingTable {
    /// `XG-FRAGMENT`: validate (§8.2 guard, sane epoch) and record. A
    /// rejected line is recorded as [`Stamp::Rejected`] (the fragment keeps
    /// the legacy anchors and is never waited for). Duplicates keep the first.
    pub fn record_start(&mut self, id: u64, start_ts: f64, lag_ms: f64) -> Result<(), StampReject> {
        self.max_id = Some(self.max_id.map_or(id, |m| m.max(id)));
        if self.stamps.contains_key(&id) {
            return Err(StampReject::Duplicate);
        }
        let stamp = match (int_ms(start_ts), int_ms(lag_ms)) {
            (_, None) => Err(StampReject::Lag(lag_ms)),
            (_, Some(l)) if !(0.0..=LAG_GUARD_MAX_MS).contains(&l) => Err(StampReject::Lag(lag_ms)),
            (Some(s), _) if s > SANE_EPOCH_MS => Ok(FragmentTiming {
                start_ts: s,
                lag_ms: int_ms(lag_ms).expect("checked"),
                end_ts: None,
            }),
            _ => Err(StampReject::StartTs(start_ts)),
        };
        match stamp {
            Ok(t) => {
                self.stamps.insert(id, Stamp::Exact(t));
                Ok(())
            }
            Err(e) => {
                self.stamps.insert(id, Stamp::Rejected);
                Err(e)
            }
        }
    }

    /// `XG-FRAGMENT-END`: attach `endTs` to an accepted start stamp.
    pub fn record_end(&mut self, id: u64, end_ts: f64) -> Result<(), StampReject> {
        let Some(Stamp::Exact(t)) = self.stamps.get_mut(&id) else {
            return Err(StampReject::EndWithoutStart);
        };
        match int_ms(end_ts) {
            Some(e) if e > SANE_EPOCH_MS => {
                t.end_ts = Some(e);
                Ok(())
            }
            _ => Err(StampReject::EndTs(end_ts)),
        }
    }

    pub fn get(&self, id: u64) -> Option<&Stamp> {
        self.stamps.get(&id)
    }

    /// The accepted stamp of `id`, if any.
    pub fn exact(&self, id: u64) -> Option<&FragmentTiming> {
        match self.stamps.get(&id) {
            Some(Stamp::Exact(t)) => Some(t),
            _ => None,
        }
    }

    /// At least one timing line arrived this run: the host emits them (an
    /// untimed run — `gst-launch`, a host whose clock precondition failed —
    /// never waits for one).
    pub fn timed(&self) -> bool {
        self.max_id.is_some()
    }

    /// `id`'s line may still be in flight: the run is timed, nothing is
    /// recorded for `id`, and no line with a higher id has arrived (lines
    /// come in id order, so one above it proves `id`'s was never written).
    pub fn pending(&self, id: u64) -> bool {
        self.timed() && !self.stamps.contains_key(&id) && self.max_id.is_some_and(|m| m < id)
    }

    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// The exact `startTs`/`durationMs` of one sidecar (integers, `durationMs ≥ 0`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExactTimes {
    pub start_ts: f64,
    pub duration_ms: f64,
}

/// §8.4 — `durationMs` stays WALL-derived, so a clock step between two
/// fragments' callbacks inflates (or shrinks) exactly the fragment open
/// across it, as `mtime − birthtime` does today:
/// - finalized with the successor's accepted stamp: `startTs(N+1) − startTs(N)`;
/// - else with an end stamp (clean EOS of the final fragment): `endTs − startTs`;
/// - else (crash-cut `finalized:false`, or a successor/end stamp that never
///   came): `mtime − lag(N) − startTs` (= mtime − the callback's wall time).
///
/// Every result is floored at 0 (backend `durationMs: int ≥ 0`).
pub fn exact_times(
    own: &FragmentTiming,
    successor: Option<&FragmentTiming>,
    finalized: bool,
    mtime_ms: f64,
) -> ExactTimes {
    let raw = match (finalized, successor, own.end_ts) {
        (true, Some(next), _) => next.start_ts - own.start_ts,
        (_, _, Some(end)) => end - own.start_ts,
        _ => js_math_round(mtime_ms - own.lag_ms - own.start_ts),
    };
    ExactTimes {
        start_ts: own.start_ts,
        duration_ms: if raw.is_finite() && raw > 0.0 { raw } else { 0.0 },
    }
}

/// The run's timing as one sweep sees it (`exact` only).
#[derive(Debug, Clone, Copy)]
pub struct ExactSweep<'a> {
    pub table: &'a TimingTable,
    /// seq → `now_ms` of the sweep that first deferred it (agent-held,
    /// updated from [`SweepPlan::deferred`]).
    pub deferred_since: &'a std::collections::BTreeMap<u64, f64>,
    /// The sweep's monotonic ms (same clock as `deferred_since`).
    pub now_ms: f64,
    /// How long a fragment may stay deferred: one sweep-poll period (§8.5:
    /// a deferral never delays a sidecar beyond one poll + line latency).
    pub defer_max_ms: f64,
}

/// What [`plan_sweep_timed`] does with one closed fragment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExactDecision {
    Exact(ExactTimes),
    Legacy,
    Defer(u64),
}

/// `seq` as a table id (an integral, non-negative, exactly representable seq).
pub fn seq_id(seq: f64) -> Option<u64> {
    (seq.is_finite() && seq >= 0.0 && seq.fract() == 0.0 && seq <= 9_007_199_254_740_991.0)
        .then_some(seq as u64)
}

impl ExactSweep<'_> {
    /// The decision for one closed, non-empty fragment (§8.4–8.5).
    pub fn decide(&self, seq: f64, finalized: bool, mtime_ms: f64, final_sweep: bool) -> ExactDecision {
        let Some(id) = seq_id(seq) else {
            return ExactDecision::Legacy;
        };
        let t = self.table;
        let waiting = t.pending(id) || (finalized && t.pending(id + 1));
        if waiting && !final_sweep {
            let expired = self
                .deferred_since
                .get(&id)
                .is_some_and(|since| self.now_ms - since >= self.defer_max_ms);
            if !expired {
                return ExactDecision::Defer(id);
            }
        }
        match t.exact(id) {
            Some(own) => ExactDecision::Exact(exact_times(own, t.exact(id + 1), finalized, mtime_ms)),
            None => ExactDecision::Legacy,
        }
    }
}

/// The one-per-session warning for a rejected timing line.
pub fn stamp_rejected_warning(stream_key: &str, id: u64, why: &StampReject) -> String {
    format!("[video-capture {stream_key}] fragment {id} timing ignored ({why}); legacy anchors for it")
}

/// One session directory as `recoverOrphanVideoSessions` walks it
/// (`video-segments.ts:352-372`): `<videoRoot>/<streamKey>/<sessionId>/`.
/// The caller lists only DIRECTORIES (the TS skips non-dirs, `:356,:365`)
/// and reads `session.json` through [`read_session_manifest_text`] — the
/// exact TS rule, NOT the typed parser (WP6 review S1).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionListing {
    pub stream_key: String,
    pub session_id: String,
    /// `None` = no manifest passing the TS rule: left untouched (can't
    /// attribute metadata; the disk manager ages it out).
    pub manifest: Option<SessionManifest>,
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
/// manifest that passes the TS rule are skipped; one that passes it with
/// odd fields is swept like the TS does ([`SessionManifest::Raw`]). Each swept session reads its OWN clock pair, as
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
        // (was the local `js_num_exact` workaround; now WP1's `js_num` is exact)
        #[derive(Serialize)]
        struct N(#[serde(with = "js_num")] f64);
        let p = |v: f64| serde_json::to_string(&N(v)).unwrap();
        assert_eq!(p(1e20), "100000000000000000000");
        assert_eq!(p(2f64.powi(60)), "1152921504606847000");
        assert_eq!(p(-2f64.powi(60)), "-1152921504606847000");
        assert_eq!(p(9_007_199_254_740_992.0), "9007199254740992");
        assert_eq!(p(1e21), "1e+21");
        assert_eq!(p(1_750_000_000_000.0), "1750000000000");
        assert_eq!(p(0.5), "0.5");
        assert_eq!(p(5e-6), "0.000005");
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
    fn raw_manifest_follows_the_ts_rule_not_the_typed_parser() {
        let text = serde_json::to_string(&manifest()).unwrap();
        assert_eq!(
            read_session_manifest_text(&text),
            Some(SessionManifest::Typed(manifest()))
        );
        let mut v: Value = serde_json::from_str(&text).unwrap();
        v["fps"] = Value::Null;
        v.as_object_mut().unwrap().remove("deviceId");
        assert_eq!(parse_session_manifest(&v), None);
        let Some(raw @ SessionManifest::Raw(_)) = read_session_manifest(&v) else {
            panic!("TS-valid manifest must be read raw");
        };
        let body = build_sidecar_body(
            &raw,
            1.0,
            FragmentStat {
                size_bytes: 5.0,
                birthtime_ms: 0.0,
                mtime_ms: 0.0,
            },
            &SidecarOpts {
                finalized: false,
                boundary_end_ms: None,
                time_source: None,
                clock_pair: ClockPair {
                    wall_ms: T0 + 120_000.0,
                    mono_ms: 620_000.0,
                },
            },
        );
        assert_eq!(body.typed(), None);
        assert_eq!(
            body.to_json(),
            r#"{"v":1,"streamKey":"cam0","sessionId":"sess-1","seq":1,"startTs":1750000060000,"startMono":560000,"durationMs":60000,"sizeBytes":5,"timeSource":"ntp","codec":"h264","width":1920,"height":1080,"fps":null,"bitrateKbps":2000,"anchor":"computed","finalized":false}"#
        );
        v["anchorTs"] = Value::from("1");
        assert_eq!(read_session_manifest(&v), None);
    }

    #[test]
    fn js_to_number_coercions() {
        let n = |v: Value| js_to_number(Some(&v));
        assert!(js_to_number(None).is_nan());
        assert_eq!(n(Value::Null), 0.0);
        assert_eq!(n(Value::from(true)), 1.0);
        assert_eq!(n(Value::from(" 0x3C\n")), 60.0);
        assert_eq!(n(Value::from("0b11")), 3.0);
        assert_eq!(n(Value::from("0o17")), 15.0);
        assert!(n(Value::from("-0x10")).is_nan());
        assert_eq!(n(Value::from("+.5e1")), 5.0);
        assert_eq!(n(Value::from("5.")), 5.0);
        assert_eq!(n(Value::from("-Infinity")), f64::NEG_INFINITY);
        assert!(n(Value::from("inf")).is_nan());
        assert!(n(Value::from("1_0")).is_nan());
        assert!(n(Value::from(".")).is_nan());
        assert!(n(Value::from("1e")).is_nan());
        assert_eq!(n(Value::from("")), 0.0);
        assert_eq!(n(serde_json::json!([])), 0.0);
        assert_eq!(n(serde_json::json!([[]])), 0.0);
        assert_eq!(n(serde_json::json!([null])), 0.0);
        assert_eq!(n(serde_json::json!(["60"])), 60.0);
        assert!(n(serde_json::json!([true])).is_nan());
        assert!(n(serde_json::json!([1, 2])).is_nan());
        assert!(n(serde_json::json!({})).is_nan());
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
            &manifest().into(),
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
        let sc = |p: &SweepPlan, k: usize| p.writes[k].sidecar.typed().unwrap().clone();
        assert_eq!(sc(&live, 0).anchor, SidecarAnchor::Boundary);
        assert_eq!(sc(&live, 0).start_ts, T0);
        assert!(sc(&live, 0).finalized && sc(&live, 1).finalized);
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
            &manifest().into(),
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
                    manifest: Some(manifest().into()),
                    entries: dead,
                },
                SessionListing {
                    stream_key: "cam1".into(),
                    session_id: "active".into(),
                    manifest: Some(manifest().into()),
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
        assert_eq!(sc(plan, 0).anchor, SidecarAnchor::Fstat);
        assert!(sc(plan, 0).finalized);
        assert!(!sc(plan, 1).finalized);
        assert_eq!(sc(plan, 1).seq, 2.0);
    }

    // -- Phase 7a WP2: exact timing ------------------------------------------

    #[test]
    fn exact_anchor_is_pts() {
        // Q-A2 (user, 2026-11-06): a NEW value "pts". Changing the label is
        // this one constant; the backend enum must accept it first (K15).
        assert_eq!(EXACT_ANCHOR, SidecarAnchor::Pts);
        assert_eq!(serde_json::to_string(&EXACT_ANCHOR).unwrap(), r#""pts""#);
        let back: SidecarAnchor = serde_json::from_str(r#""pts""#).unwrap();
        assert_eq!(back, SidecarAnchor::Pts);
        // The legacy labels are unchanged.
        for (a, s) in [
            (SidecarAnchor::Fstat, "fstat"),
            (SidecarAnchor::Computed, "computed"),
            (SidecarAnchor::Boundary, "boundary"),
        ] {
            assert_eq!(serde_json::to_string(&a).unwrap(), format!("\"{s}\""));
        }
    }

    /// The exact sidecar has the legacy key set and order; only `startTs`,
    /// `startMono`, `durationMs` and `anchor` carry new values.
    #[test]
    fn exact_sidecar_byte_shape() {
        let pair = ClockPair {
            wall_ms: T0 + 180_000.0,
            mono_ms: 500_000.0 + 180_000.0,
        };
        let opts = SidecarOpts {
            finalized: true,
            boundary_end_ms: Some(T0 + 62_400.0),
            time_source: None,
            clock_pair: pair,
        };
        let stat = FragmentStat {
            size_bytes: 123.0,
            birthtime_ms: T0 + 2_400.0,
            mtime_ms: T0 + 62_400.0,
        };
        let x = ExactTimes {
            start_ts: T0 + 300.0,
            duration_ms: 60_000.0,
        };
        let body = build_sidecar_body_exact(&manifest().into(), 0.0, stat, &opts, Some(x));
        assert_eq!(
            body.to_json(),
            r#"{"v":1,"deviceId":"dev-1","streamKey":"cam0","sessionId":"sess-1","seq":0,"startTs":1750000000300,"startMono":500300,"durationMs":60000,"sizeBytes":123,"timeSource":"ntp","codec":"h264","width":1920,"height":1080,"fps":15,"bitrateKbps":2000,"anchor":"pts","finalized":true}"#
        );
        // `None` is the legacy body (here: the fragment-0 boundary walk-back).
        assert_eq!(
            build_sidecar_body_exact(&manifest().into(), 0.0, stat, &opts, None),
            build_sidecar_body(&manifest().into(), 0.0, stat, &opts)
        );
        assert_eq!(
            build_sidecar_body(&manifest().into(), 0.0, stat, &opts)
                .typed()
                .unwrap()
                .start_ts,
            T0 + 2_400.0
        );
        // The raw-manifest path takes the same override.
        let raw: SessionManifest = SessionManifest::Raw(
            serde_json::to_value(manifest())
                .unwrap()
                .as_object()
                .unwrap()
                .clone(),
        );
        let rb = build_sidecar_body_exact(&raw, 0.0, stat, &opts, Some(x));
        assert_eq!(rb.to_json(), body.to_json());
    }

    #[test]
    fn timing_table_guard_order_and_pending() {
        let mut t = TimingTable::default();
        assert!(!t.timed() && !t.pending(0));
        t.record_start(0, T0 + 300.4, 2100.2).unwrap();
        assert_eq!(
            t.exact(0),
            Some(&FragmentTiming {
                start_ts: T0 + 300.0,
                lag_ms: 2100.0,
                end_ts: None
            })
        );
        assert!(t.timed());
        assert!(!t.pending(0) && t.pending(1) && t.pending(7));
        // First wins.
        assert_eq!(
            t.record_start(0, T0, 1.0),
            Err(StampReject::Duplicate)
        );
        assert_eq!(t.exact(0).unwrap().start_ts, T0 + 300.0);
        // Guard edges: [0, 7000] accepted, outside rejected (recorded, not pending).
        assert_eq!(LAG_GUARD_MAX_MS, 7_000.0);
        assert!(t.record_start(1, T0 + 60_300.0, 7_000.0).is_ok());
        assert!(t.record_start(2, T0 + 120_300.0, 0.0).is_ok());
        assert_eq!(
            t.record_start(3, T0 + 180_300.0, 7_000.6),
            Err(StampReject::Lag(7_000.6))
        );
        assert_eq!(
            t.record_start(4, T0, -0.6),
            Err(StampReject::Lag(-0.6))
        );
        assert_eq!(
            t.record_start(5, T0, f64::NAN).map_err(|e| matches!(e, StampReject::Lag(_))),
            Err(true)
        );
        assert_eq!(t.record_start(6, 0.0, 2100.0), Err(StampReject::StartTs(0.0)));
        assert_eq!(
            t.record_start(7, SANE_EPOCH_MS, 2100.0),
            Err(StampReject::StartTs(SANE_EPOCH_MS))
        );
        for id in 3..=7 {
            assert_eq!(t.get(id), Some(&Stamp::Rejected), "{id}");
            assert!(!t.pending(id));
        }
        // A gap below the highest id is lost, never pending.
        t.record_start(10, T0 + 600_300.0, 2100.0).unwrap();
        assert!(!t.pending(8) && !t.pending(9) && t.pending(11));
        // End stamps.
        assert_eq!(t.record_end(9, T0), Err(StampReject::EndWithoutStart));
        assert_eq!(t.record_end(3, T0), Err(StampReject::EndWithoutStart));
        assert_eq!(t.record_end(10, 5.0), Err(StampReject::EndTs(5.0)));
        assert_eq!(t.exact(10).unwrap().end_ts, None);
        t.record_end(10, T0 + 640_300.2).unwrap();
        assert_eq!(t.exact(10).unwrap().end_ts, Some(T0 + 640_300.0));
        t.clear();
        assert!(t.is_empty() && !t.timed());
    }

    /// §8.4: durationMs is wall-derived in every branch and never negative.
    #[test]
    fn exact_durations_are_wall_derived_and_floored() {
        let own = FragmentTiming {
            start_ts: T0 + 300.0,
            lag_ms: 2_100.0,
            end_ts: None,
        };
        let next = |dt: f64| FragmentTiming {
            start_ts: T0 + 300.0 + dt,
            lag_ms: 2_100.0,
            end_ts: None,
        };
        let mt = T0 + 62_400.0; // = the next callback's wall
        // Non-final: the stamp delta, whatever the mtime says (a +S step).
        assert_eq!(
            exact_times(&own, Some(&next(150_000.0)), true, mt).duration_ms,
            150_000.0
        );
        assert_eq!(exact_times(&own, Some(&next(40_000.0)), true, mt).duration_ms, 40_000.0);
        assert_eq!(exact_times(&own, Some(&next(-30_000.0)), true, mt).duration_ms, 0.0);
        // Clean final: endTs − startTs (before the successor-less mtime rule).
        let ended = FragmentTiming {
            end_ts: Some(T0 + 45_300.0),
            ..own
        };
        assert_eq!(exact_times(&ended, None, true, mt).duration_ms, 45_000.0);
        assert_eq!(
            exact_times(
                &FragmentTiming {
                    end_ts: Some(T0),
                    ..own
                },
                None,
                true,
                mt
            )
            .duration_ms,
            0.0
        );
        // Crash-cut (finalized:false): mtime − lag − startTs even with a successor stamp.
        assert_eq!(
            exact_times(&own, Some(&next(60_000.0)), false, T0 + 32_400.4).duration_ms,
            30_000.0
        );
        // No successor, no end: the same mtime rule.
        assert_eq!(exact_times(&own, None, true, mt).duration_ms, 60_000.0);
        assert_eq!(exact_times(&own, None, true, T0).duration_ms, 0.0);
        assert_eq!(exact_times(&own, None, true, f64::NAN).duration_ms, 0.0);
        assert_eq!(exact_times(&own, None, true, mt).start_ts, T0 + 300.0);
    }

    /// §8.5: a live sweep defers while a stamp may be in flight, at most
    /// `defer_max_ms`; a final sweep never does.
    #[test]
    fn deferral_is_bounded_and_never_final() {
        let mut t = TimingTable::default();
        t.record_start(0, T0 + 300.0, 2_100.0).unwrap();
        let mut since = std::collections::BTreeMap::new();
        let x = |since: &std::collections::BTreeMap<u64, f64>, now: f64| {
            ExactSweep {
                table: &t,
                deferred_since: since,
                now_ms: now,
                defer_max_ms: 10_000.0,
            }
            .decide(0.0, true, T0 + 62_400.0, false)
        };
        assert_eq!(x(&since, 1_000.0), ExactDecision::Defer(0));
        since.insert(0, 1_000.0);
        assert_eq!(x(&since, 10_999.0), ExactDecision::Defer(0));
        assert!(matches!(x(&since, 11_000.0), ExactDecision::Exact(_)));
        let fin = ExactSweep {
            table: &t,
            deferred_since: &std::collections::BTreeMap::new(),
            now_ms: 0.0,
            defer_max_ms: 10_000.0,
        };
        assert!(matches!(
            fin.decide(0.0, true, T0 + 62_400.0, true),
            ExactDecision::Exact(_)
        ));
        // Own stamp missing (lost) → legacy; a non-integral seq → legacy.
        assert_eq!(fin.decide(5.0, true, T0, true), ExactDecision::Legacy);
        assert_eq!(fin.decide(0.5, true, T0, false), ExactDecision::Legacy);
        // A crash-cut fragment does not wait for its successor's stamp.
        assert!(matches!(
            fin.decide(0.0, false, T0 + 62_400.0, false),
            ExactDecision::Exact(_)
        ));
        assert_eq!(seq_id(3.0), Some(3));
        assert_eq!(seq_id(-1.0), None);
        assert_eq!(seq_id(f64::INFINITY), None);
    }

    #[test]
    fn stamp_rejected_warning_text() {
        assert_eq!(
            stamp_rejected_warning("cam0", 3, &StampReject::Lag(9_000.0)),
            "[video-capture cam0] fragment 3 timing ignored (lagMs 9000 outside [0, 7000]); legacy anchors for it"
        );
        assert_eq!(
            stamp_rejected_warning("cam1", 0, &StampReject::StartTs(0.0)),
            "[video-capture cam1] fragment 0 timing ignored (startTs 0 is not a sane epoch); legacy anchors for it"
        );
    }
}
