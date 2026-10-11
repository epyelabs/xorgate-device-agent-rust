//! Telemetry segment files (plan D2): the naming / rotation / header-re-emit
//! decisions of `src/recording/segments.ts` (270 lines) with the fd I/O
//! lifted out. Rotated JSONL, gzipped on close; "a closed segment exists" IS
//! the pending-upload state — there is no journal.
//!
//! Layout (under the recordings root):
//! `telemetry/<sessionId>/seg-<startTsMs>-<seq5>.jsonl[.gz]`; the S3 key
//! (`telemetry/v1/<deviceId>/<sessionId>/<startTsMs>-<seq>.jsonl.gz`) is
//! built by `src/recording/telemetry-source.ts:50` — Phase 3 (uploader).
//!
//! Clock stance (optimize-system-process-cycle Phase 8, `segments.ts:18-23`):
//! the wall clock names segments and stamps samples but never SCHEDULES
//! anything; rotation age is measured on the MONOTONIC clock because a
//! battery-less device corrects its clock minutes into every boot — a
//! backward correction once made `now - openedAt` negative and wedged the
//! roller for the rest of the boot (field-observed 2026-07-29).
//!
//! Out of core (Phase 3): `open/append/rotate` fd I/O (`segments.ts:156-191`:
//! mkdir/open/write/fsync/close), `gzipFinalize` (`:198-213`: gzip, temp +
//! fsync + rename + dir fsync, unlink) and the directory walk of
//! `finalizeOrphanSegments` (`:224-270`). What they DECIDE is here:
//! [`SegmentWriterCore`] emits [`SegmentOp`]s for the I/O layer to execute in
//! order, and [`finalize_plan`] / [`orphan_entry_action`] are the per-file
//! rules of the orphan sweep.

use serde::{Deserialize, Serialize};
use serde_json::Map;

use crate::clock::{MonoMs, WallMs};
use crate::telemetry::js_num;
use crate::types::TimeSource;
use crate::video_capture::posix_join;

/// `SEGMENT_MAX_BYTES` (`segments.ts:44`): rotate on ~1 MB of JSONL …
pub const SEGMENT_MAX_BYTES: u64 = 1024 * 1024;
/// `SEGMENT_MAX_AGE_MS` (`segments.ts:45`): … or 60 s of (monotonic) age, whichever first.
pub const SEGMENT_MAX_AGE_MS: u64 = 60_000;

/// The literal `type: "header"` tag of [`SegmentHeader`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HeaderTag {
    #[default]
    #[serde(rename = "header")]
    Header,
}

/// `SegmentHeader` (`segments.ts:47-56`): the first JSONL line of every
/// segment, re-emitted mid-segment on a time-source change. Key order is the
/// TS interface order (the backend parses `type`, `deviceId`, `sessionId`,
/// `bootId?`, `timeSource?`, `rateHz?`, `startedAt?`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentHeader {
    #[serde(rename = "type")]
    pub kind: HeaderTag,
    pub v: u8,
    pub device_id: String,
    pub session_id: String,
    pub boot_id: String,
    pub time_source: TimeSource,
    #[serde(with = "js_num")]
    pub rate_hz: f64,
    #[serde(with = "js_num")]
    pub started_at: f64,
}

/// `SampleLine` (`segments.ts:58-63`): `{ts, mono, seq, metrics}`. `metrics`
/// keeps insertion order (`serde_json` `preserve_order`) and every number in
/// it is printed by [`crate::js::JsJson`], so the line serialises exactly as
/// `JSON.stringify` did however the `Value` was built (`Value::from(1.0)` →
/// `1`, a gps `lat` of `0.000004` → `0.000004`, not serde_json's `4e-6`;
/// review-integration M1). `mono` is `Number(process.hrtime.bigint())` —
/// NANO-seconds (`telemetry-recorder.ts:116`), past 2^53 after ~104 days of
/// uptime, hence the exact-integer printer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleLine {
    #[serde(with = "js_num")]
    pub ts: f64,
    #[serde(with = "js_num")]
    pub mono: f64,
    #[serde(with = "js_num")]
    pub seq: f64,
    #[serde(serialize_with = "crate::js::json_map::serialize")]
    pub metrics: Map<String, serde_json::Value>,
}

/// `seg-${now}-${String(segSeq).padStart(5, "0")}.jsonl` (`segments.ts:159`).
pub fn segment_file_name(now: WallMs, seg_seq: u32) -> String {
    format!("seg-{now}-{seg_seq:05}.jsonl")
}

/// `shouldRotate` (`segments.ts:150-154`): size OR monotonic age. Age is
/// `monoMs() - openedAtMono` — monotonic, so a wall-clock step can never
/// make it negative (Phase 8). Saturating here only guards a caller feeding
/// a mono value older than the open stamp, which the TS would read as a
/// negative (never-rotating) age; a monotonic clock cannot produce it.
pub fn should_rotate(
    bytes: u64,
    max_bytes: u64,
    mono_now: MonoMs,
    opened_at_mono: MonoMs,
    max_age_ms: u64,
) -> bool {
    bytes >= max_bytes || mono_now.saturating_sub(opened_at_mono) >= max_age_ms
}

/// One filesystem effect the I/O layer executes, in order. Lines are handed
/// over WITHOUT the trailing `\n` the writer appends (`segments.ts:171`,
/// `Buffer.from(json + "\n")`); byte accounting here already counts it.
///
/// **Executor contract (review-wp6 S2, CF35).** The TS mutates its fields
/// and does the fd I/O interleaved, so a throw leaves it half-way through a
/// method (`segments.ts:116-121`, `:156-191`); the core instead advances its
/// whole state before handing the ops over. The executor therefore:
/// 1. runs the ops of ONE call strictly in order and STOPS at the first op
///    that fails (later ops of that call never run — exactly as the throw
///    unwinds `writeLine` before `open`);
/// 2. reports the failure with [`SegmentWriterCore::op_failed`]`(ops, i,
///    failure)`, which rewinds what the core advanced for `ops[i..]` so its
///    state equals the TS's at the throw; then
/// 3. surfaces the error to its caller (the recorder's `tick failed` line).
///
/// The canonical case: a gzip that fails on a full disk inside the `Close`
/// of a `write_line` leaves the segment closed (plain file on disk, temp gz
/// possibly too), `Open`/`Append` never run, `segSeq` not consumed, and the
/// NEXT call opens a fresh segment — the TS behaviour. A `Close` is
/// executed as [`GZIP_FINALIZE_STEPS`] in that order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentOp {
    /// `open()` (`segments.ts:156-167`): `mkdir -p dir`, create `path` (mode
    /// 0600) and write the header line.
    Open { path: String, line: String },
    /// `append()` (`:169-174`): write `line + "\n"` to the active segment.
    Append { path: String, line: String },
    /// `rotate()` (`:176-191`): fsync + close the plain file; `gz == None`
    /// means the segment never got a sample (header only) and is unlinked
    /// instead of shipped; `Some(gzPath)` means `gzipFinalize(plain)` →
    /// `<plain>.gz` (temp + fsync + rename + dir fsync, plain deleted) and
    /// the `onSegmentClosed(gzPath)` hook.
    Close { plain: String, gz: Option<String> },
}

/// The decision core of `class SegmentWriter` (`segments.ts:86-192`): the
/// same state, the same method boundaries, with every fd operation replaced
/// by a returned [`SegmentOp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentWriterCore {
    dir: String,
    max_bytes: u64,
    max_age_ms: u64,
    /// `fd !== null` ⇔ `path.is_some()`.
    path: Option<String>,
    bytes: u64,
    sample_lines: u64,
    /// Monotonic ms at segment open — rotation age is measured from here.
    opened_at_mono: MonoMs,
    seg_seq: u32,
    last_time_source: Option<TimeSource>,
    /// Which public method produced the last op batch — tells
    /// [`Self::op_failed`] whether an `Append` was `writeLine`'s sample
    /// (counted in `sampleLines`) or `noteTimeSource`'s header re-emit.
    last_call: LastCall,
}

/// The public method behind the last op batch (S2 bookkeeping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastCall {
    WriteLine,
    NoteTimeSource,
    Rotate,
}

impl SegmentWriterCore {
    /// `new SegmentWriter({dir, maxBytes?, maxAgeMs?})` (`segments.ts:99-108`):
    /// `??` per field, so an explicitly-absent option can never clobber a
    /// default (`{ maxAgeMs: undefined, ...defaults }` once did).
    pub fn new(dir: &str, max_bytes: Option<u64>, max_age_ms: Option<u64>) -> Self {
        Self {
            dir: dir.to_string(),
            max_bytes: max_bytes.unwrap_or(SEGMENT_MAX_BYTES),
            max_age_ms: max_age_ms.unwrap_or(SEGMENT_MAX_AGE_MS),
            path: None,
            bytes: 0,
            sample_lines: 0,
            opened_at_mono: 0,
            seg_seq: 0,
            last_time_source: None,
            last_call: LastCall::Rotate,
        }
    }

    /// `activePath()` (`segments.ts:111-113`): the active plain `.jsonl`, or
    /// `None` when no segment is open.
    pub fn active_path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// `writeLine(line, now)` (`segments.ts:116-121`). Segments open lazily
    /// on the first sample, so idle sessions leave no files. `header` is
    /// `opts.header()`, called only when a segment opens (so `timeSource` is
    /// current); `now` names the file, `mono` (`opts.monoMs()`) ages it.
    pub fn write_line(
        &mut self,
        line: &SampleLine,
        now: WallMs,
        mono: MonoMs,
        header: &dyn Fn() -> SegmentHeader,
    ) -> Vec<SegmentOp> {
        self.last_call = LastCall::WriteLine;
        let mut ops = Vec::new();
        if self.path.is_some() && self.should_rotate(mono) {
            ops.extend(self.rotate());
        }
        if self.path.is_none() {
            ops.push(self.open(now, mono, header));
        }
        let json = serde_json::to_string(line).expect("SampleLine serialises");
        ops.extend(self.append(json));
        self.sample_lines += 1;
        ops
    }

    /// `noteTimeSource(ts)` (`segments.ts:128-135`): record a time-provenance
    /// change mid-segment by re-emitting a header line (plan D6; readers treat
    /// the latest header as current). No-op when nothing is open or the value
    /// is unchanged — but the value is remembered either way.
    pub fn note_time_source(
        &mut self,
        ts: TimeSource,
        header: &dyn Fn() -> SegmentHeader,
    ) -> Vec<SegmentOp> {
        self.last_call = LastCall::NoteTimeSource;
        if self.path.is_none() || self.last_time_source == Some(ts) {
            self.last_time_source = Some(ts);
            return Vec::new();
        }
        self.last_time_source = Some(ts);
        // `{ ...this.opts.header(), timeSource: ts }`: same keys, same order.
        let mut h = header();
        h.time_source = ts;
        self.append(serde_json::to_string(&h).expect("SegmentHeader serialises"))
    }

    /// `maybeRotate()` (`segments.ts:141-143`): age/size check without a
    /// write — an idle recorder still closes segments on time.
    pub fn maybe_rotate(&mut self, mono: MonoMs) -> Vec<SegmentOp> {
        self.last_call = LastCall::Rotate;
        if self.path.is_some() && self.should_rotate(mono) {
            self.rotate()
        } else {
            Vec::new()
        }
    }

    /// `finalize()` (`segments.ts:146-148`): close + gzip the active segment
    /// (shutdown / recording-off path). Safe to re-call.
    pub fn finalize(&mut self) -> Vec<SegmentOp> {
        self.last_call = LastCall::Rotate;
        if self.path.is_some() {
            self.rotate()
        } else {
            Vec::new()
        }
    }

    fn should_rotate(&self, mono: MonoMs) -> bool {
        should_rotate(
            self.bytes,
            self.max_bytes,
            mono,
            self.opened_at_mono,
            self.max_age_ms,
        )
    }

    /// `open(now)` (`segments.ts:156-167`).
    fn open(&mut self, now: WallMs, mono: MonoMs, header: &dyn Fn() -> SegmentHeader) -> SegmentOp {
        let header = header();
        let path = posix_join(&self.dir, &segment_file_name(now, self.seg_seq));
        self.seg_seq += 1;
        self.path = Some(path.clone());
        self.bytes = 0;
        self.sample_lines = 0;
        self.opened_at_mono = mono;
        self.last_time_source = Some(header.time_source);
        let line = serde_json::to_string(&header).expect("SegmentHeader serialises");
        self.bytes += line.len() as u64 + 1;
        SegmentOp::Open { path, line }
    }

    /// `append(json)` (`segments.ts:169-174`): no-op without an open segment;
    /// counts the UTF-8 bytes of `json + "\n"`.
    fn append(&mut self, json: String) -> Vec<SegmentOp> {
        let Some(path) = self.path.clone() else {
            return Vec::new();
        };
        self.bytes += json.len() as u64 + 1;
        vec![SegmentOp::Append { path, line: json }]
    }

    /// `rotate()` (`segments.ts:176-191`): a header-only segment carries no
    /// data — dropped rather than shipped as an empty gz.
    fn rotate(&mut self) -> Vec<SegmentOp> {
        let Some(plain) = self.path.take() else {
            return Vec::new();
        };
        let gz = (self.sample_lines > 0).then(|| format!("{plain}.gz"));
        vec![SegmentOp::Close { plain, gz }]
    }
}

impl SegmentWriterCore {
    /// The S2 rewind (CF35): the executor ran `ops[..index]`, then
    /// `ops[index]` failed with `failure`; nothing after it ran. Restores the
    /// state the TS has when the same step throws (`segments.ts:116-191`):
    ///
    /// | failed step | TS state at the throw | rewind here |
    /// |---|---|---|
    /// | `Close` (fsync/close/unlink/`gzipFinalize`, `:179-189`) | `fd`/`path` null; a following `open` never ran | if an `Open` follows in `ops`: `path = None`, `segSeq` given back |
    /// | `Open`, `mkdirSync` (`:157`) | untouched | `path = None`, `segSeq` given back |
    /// | `Open`, `openSync` (`:161`) | `path` set, `segSeq` consumed, `fd` null | `path = None` (TS keeps a stale `activePath()`; tolerated, see evidence wp2) |
    /// | `Open`, header `writeSync` (`:166`) | fd OPEN, `bytes = 0`, `sampleLines = 0` | `bytes = 0`, `sampleLines = 0` (executor keeps the handle) |
    /// | `Append` (`:172`) | `bytes` not counted; `writeLine`'s `sampleLines++` not run | `bytes -= len + 1`; `sampleLines -= 1` for `writeLine`'s sample |
    ///
    /// `lastTimeSource` is not rewound: while no segment is open it is only
    /// ever overwritten (`noteTimeSource` with `fd === null`, `open`), so the
    /// TS and the core cannot be told apart by any later call.
    pub fn op_failed(&mut self, ops: &[SegmentOp], index: usize, failure: OpFailure) {
        let Some(op) = ops.get(index) else {
            return;
        };
        let opened_later = ops[index + 1..]
            .iter()
            .any(|o| matches!(o, SegmentOp::Open { .. }));
        match (op, failure) {
            (SegmentOp::Close { .. }, _) => {
                if opened_later {
                    self.path = None;
                    self.seg_seq = self.seg_seq.saturating_sub(1);
                }
            }
            (SegmentOp::Open { .. }, OpFailure::OpenMkdir) => {
                self.path = None;
                self.seg_seq = self.seg_seq.saturating_sub(1);
            }
            (SegmentOp::Open { .. }, OpFailure::OpenCreate) => {
                self.path = None;
            }
            (SegmentOp::Open { .. }, _) => {
                // OpenHeader: the file exists and the fd stays open, but the
                // header never counted and the sample never ran.
                self.bytes = 0;
                self.sample_lines = 0;
            }
            (SegmentOp::Append { line, .. }, _) => {
                self.bytes = self.bytes.saturating_sub(line.len() as u64 + 1);
                let is_sample = self.last_call == LastCall::WriteLine && index + 1 == ops.len();
                if is_sample {
                    self.sample_lines = self.sample_lines.saturating_sub(1);
                }
            }
        }
    }
}

/// Which part of a [`SegmentOp`] failed (S2): the executor names the step,
/// because the TS state at a throw depends on it (see
/// [`SegmentWriterCore::op_failed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpFailure {
    /// `Open`: `mkdirSync(dir, {recursive})` threw — nothing created.
    OpenMkdir,
    /// `Open`: `openSync(path, "w", 0o600)` threw — no fd.
    OpenCreate,
    /// `Open`: the header `writeSync` threw — the file exists, fd open.
    OpenHeader,
    /// `Append`: `writeSync` threw.
    Append,
    /// `Close`: any step of the plain close or of [`GZIP_FINALIZE_STEPS`].
    Close,
}

/// `${gzPath}.tmp.${process.pid}` (`segments.ts:201`) — the temp name the
/// orphan sweep's `.gz.tmp.` rule ([`orphan_entry_action`]) removes at the
/// next boot (review-wp6 S3, CF36).
pub fn gz_temp_path(gz: &str, pid: u32) -> String {
    format!("{gz}.tmp.{pid}")
}

/// One step of `rotate()` + `gzipFinalize()` (`segments.ts:176-213`), in
/// the order of [`GZIP_FINALIZE_STEPS`] (review-wp6 S3, CF36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GzipFinalizeStep {
    /// `fsyncSync(fd)` of the plain segment (`:179`).
    FsyncPlain,
    /// `closeSync(fd)` (`:180`); from here `fd`/`path` are null.
    ClosePlain,
    /// Header-only segment: `unlinkSync(plain)` and stop (`:185-188`).
    UnlinkHeaderOnly,
    /// `readFileSync(plain)` — skipped when the orphan sweep passes `content` (`:199`).
    ReadPlain,
    /// `openSync(gz_temp_path(gz, pid), "w", 0o600)` (`:202`).
    OpenTemp,
    /// `writeSync(fd, gzipSync(data))` (`:204`) — flate2 level 6, mtime 0, OS 3 (P6).
    WriteGzip,
    /// `fsyncSync(fd)` (`:205`).
    FsyncTemp,
    /// `closeSync(fd)` (`:207`, in `finally`: also after a failed write/fsync;
    /// the temp is NOT unlinked — the next boot's sweep removes it).
    CloseTemp,
    /// `renameSync(tmp, gzPath)` (`:209`).
    RenameTemp,
    /// `fsyncDir(dirname(plain))` (`:210`, best effort — never fails the step list).
    FsyncDir,
    /// `unlinkSync(plain)` (`:211`).
    UnlinkPlain,
    /// `onSegmentClosed(gzPath)` (`:190`) — only after every step above.
    NotifyClosed,
}

/// The ordered step list a `Close { gz: Some(_) }` executes; a `Close { gz:
/// None }` runs `FsyncPlain, ClosePlain, UnlinkHeaderOnly`. A failure at any
/// step stops the list there ([`OpFailure::Close`]).
pub const GZIP_FINALIZE_STEPS: [GzipFinalizeStep; 11] = [
    GzipFinalizeStep::FsyncPlain,
    GzipFinalizeStep::ClosePlain,
    GzipFinalizeStep::ReadPlain,
    GzipFinalizeStep::OpenTemp,
    GzipFinalizeStep::WriteGzip,
    GzipFinalizeStep::FsyncTemp,
    GzipFinalizeStep::CloseTemp,
    GzipFinalizeStep::RenameTemp,
    GzipFinalizeStep::FsyncDir,
    GzipFinalizeStep::UnlinkPlain,
    GzipFinalizeStep::NotifyClosed,
];

/// The steps the orphan sweep's `gzipFinalize(path, whole)` runs
/// (`segments.ts:262`): no plain fd, no read (the torn-line-trimmed bytes
/// are passed in), no `onSegmentClosed`.
pub const ORPHAN_GZIP_STEPS: [GzipFinalizeStep; 7] = [
    GzipFinalizeStep::OpenTemp,
    GzipFinalizeStep::WriteGzip,
    GzipFinalizeStep::FsyncTemp,
    GzipFinalizeStep::CloseTemp,
    GzipFinalizeStep::RenameTemp,
    GzipFinalizeStep::FsyncDir,
    GzipFinalizeStep::UnlinkPlain,
];

/// What the orphan sweep does with one directory entry
/// (`finalizeOrphanSegments`, `segments.ts:241-251`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanEntryAction {
    /// Not a plain segment (`.jsonl.gz`, `session.json`, …): left alone.
    Skip,
    /// A leftover gzip temp (`*.gz.tmp.*`): unlinked.
    DeleteTemp,
    /// A plain `.jsonl`: read it and apply [`finalize_plan`].
    Finalize,
}

/// `segments.ts:243-251`: `.gz.tmp.` anywhere in the name → delete; not
/// `.jsonl` → skip; else finalize.
pub fn orphan_entry_action(name: &str) -> OrphanEntryAction {
    if name.contains(".gz.tmp.") {
        OrphanEntryAction::DeleteTemp
    } else if !name.ends_with(".jsonl") {
        OrphanEntryAction::Skip
    } else {
        OrphanEntryAction::Finalize
    }
}

/// The verdict of [`finalize_plan`] for one plain segment's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanPlan {
    /// Fewer than a header AND one sample survive: `unlinkSync(path)`.
    Delete,
    /// Gzip the first `keep_len` bytes (`gzipFinalize(path, whole)`), i.e. the
    /// content up to and including its last newline; a torn tail is dropped.
    Finalize { keep_len: usize },
}

/// The torn-last-line rule of `finalizeOrphanSegments` (`segments.ts:253-263`):
/// `whole` = everything up to the last `\n` (empty when there is none); it
/// is kept only if it holds at least two non-empty lines (a header AND one
/// sample). Line counting is byte-level: `\n` never occurs inside a UTF-8
/// multibyte sequence, and the TS `toString("utf8")` replacement of invalid
/// bytes never produces an empty line from a non-empty one.
pub fn finalize_plan(raw: &[u8]) -> OrphanPlan {
    let Some(last_nl) = raw.iter().rposition(|&b| b == 0x0a) else {
        return OrphanPlan::Delete;
    };
    let whole = &raw[..=last_nl];
    let lines = whole
        .split(|&b| b == 0x0a)
        .filter(|l| !l.is_empty())
        .count();
    if lines < 2 {
        OrphanPlan::Delete
    } else {
        OrphanPlan::Finalize {
            keep_len: last_nl + 1,
        }
    }
}

/// The `console.error` of a failed orphan finalize (`segments.ts:265`); the
/// file is left in place and not counted.
pub fn orphan_finalize_failed_error(path: &str, message: &str) -> String {
    format!("[recording] orphan finalize failed for {path}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> SegmentHeader {
        SegmentHeader {
            kind: HeaderTag::Header,
            v: 1,
            device_id: "dev-1".into(),
            session_id: "session-a".into(),
            boot_id: "boot-1".into(),
            time_source: TimeSource::Ntp,
            rate_hz: 10.0,
            started_at: 1000.0,
        }
    }

    #[test]
    fn header_and_sample_byte_shape() {
        assert_eq!(
            serde_json::to_string(&header()).unwrap(),
            r#"{"type":"header","v":1,"deviceId":"dev-1","sessionId":"session-a","bootId":"boot-1","timeSource":"ntp","rateHz":10,"startedAt":1000}"#
        );
        let line: SampleLine = serde_json::from_str(
            r#"{"ts":1000,"mono":1,"seq":0,"metrics":{"imu":{"a":1},"gps":{}}}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&line).unwrap(),
            r#"{"ts":1000,"mono":1,"seq":0,"metrics":{"imu":{"a":1},"gps":{}}}"#
        );
        assert!(serde_json::from_str::<SegmentHeader>(r#"{"type":"sample","v":1}"#).is_err());
    }

    #[test]
    fn rotation_rules() {
        assert!(should_rotate(
            SEGMENT_MAX_BYTES,
            SEGMENT_MAX_BYTES,
            0,
            0,
            60_000
        ));
        assert!(!should_rotate(10, 200, 59_999, 0, 60_000));
        assert!(should_rotate(10, 200, 61_000, 1_000, 60_000));
        assert_eq!(segment_file_name(1000, 0), "seg-1000-00000.jsonl");
        assert_eq!(
            segment_file_name(1_750_000_000_000, 123_456),
            "seg-1750000000000-123456.jsonl"
        );
    }

    #[test]
    fn writer_core_lifecycle() {
        let mut w = SegmentWriterCore::new("/r/telemetry/session-a", None, Some(1000));
        let h = header;
        let sample = |seq: f64| SampleLine {
            ts: 1000.0 + seq,
            mono: seq,
            seq,
            metrics: Map::new(),
        };
        assert_eq!(w.active_path(), None);
        assert_eq!(w.maybe_rotate(5_000), vec![]);
        let ops = w.write_line(&sample(0.0), 1000, 1000, &h);
        assert_eq!(ops.len(), 2);
        assert!(
            matches!(&ops[0], SegmentOp::Open { path, .. } if path == "/r/telemetry/session-a/seg-1000-00000.jsonl")
        );
        assert_eq!(
            w.active_path(),
            Some("/r/telemetry/session-a/seg-1000-00000.jsonl")
        );
        // Unchanged time source: nothing; changed: a header re-emit with the new value.
        assert_eq!(w.note_time_source(TimeSource::Ntp, &h), vec![]);
        let re = w.note_time_source(TimeSource::Unsynced, &h);
        assert!(
            matches!(&re[0], SegmentOp::Append { line, .. } if line.contains(r#""timeSource":"unsynced""#))
        );
        // Age crossed: rotate BEFORE the next write, which opens #2.
        let ops = w.write_line(&sample(1.0), 2500, 2500, &h);
        assert_eq!(
            ops[0],
            SegmentOp::Close {
                plain: "/r/telemetry/session-a/seg-1000-00000.jsonl".into(),
                gz: Some("/r/telemetry/session-a/seg-1000-00000.jsonl.gz".into())
            }
        );
        assert!(
            matches!(&ops[1], SegmentOp::Open { path, .. } if path.ends_with("seg-2500-00001.jsonl"))
        );
        assert_eq!(w.finalize().len(), 1);
        assert_eq!(w.finalize(), vec![]);
    }

    #[test]
    fn size_rotation_emits_close_open_append() {
        let mut w = SegmentWriterCore::new("/d", Some(10), None);
        let h = header;
        // First write: header + sample → bytes ≥ 10 → the NEXT write rotates.
        w.write_line(
            &SampleLine {
                ts: 1.0,
                mono: 1.0,
                seq: 0.0,
                metrics: Map::new(),
            },
            1,
            1,
            &h,
        );
        let ops = w.write_line(
            &SampleLine {
                ts: 2.0,
                mono: 2.0,
                seq: 1.0,
                metrics: Map::new(),
            },
            2,
            2,
            &h,
        );
        assert!(matches!(&ops[0], SegmentOp::Close { gz: Some(_), .. }));
        assert_eq!(ops.len(), 3);
    }

    /// S2 (CF35): `op_failed` rewinds core to the TS state at each throw.
    #[test]
    fn op_failed_rewinds_to_the_ts_state() {
        let h = header;
        let sample = |seq: f64| SampleLine {
            ts: seq,
            mono: seq,
            seq,
            metrics: Map::new(),
        };
        // Failed gzip inside writeLine's rotation: closed, open never ran.
        let mut w = SegmentWriterCore::new("/d", None, Some(1000));
        w.write_line(&sample(0.0), 1, 0, &h);
        let ops = w.write_line(&sample(1.0), 2, 1000, &h);
        assert!(matches!(ops[0], SegmentOp::Close { .. }));
        w.op_failed(&ops, 0, OpFailure::Close);
        assert_eq!(w.active_path(), None);
        let ops = w.write_line(&sample(2.0), 3, 1100, &h);
        assert!(matches!(&ops[0], SegmentOp::Open { path, .. } if path == "/d/seg-3-00001.jsonl"));
        // Failed rotation from maybe_rotate: nothing to rewind.
        let ops = w.maybe_rotate(5000);
        w.op_failed(&ops, 0, OpFailure::Close);
        assert_eq!(w.active_path(), None);
        // mkdir failure: seq given back; create failure: seq consumed.
        let ops = w.write_line(&sample(3.0), 4, 6000, &h);
        w.op_failed(&ops, 0, OpFailure::OpenMkdir);
        let ops = w.write_line(&sample(4.0), 5, 6001, &h);
        assert!(matches!(&ops[0], SegmentOp::Open { path, .. } if path == "/d/seg-5-00002.jsonl"));
        w.op_failed(&ops, 0, OpFailure::OpenCreate);
        assert_eq!(w.active_path(), None);
        let ops = w.write_line(&sample(5.0), 6, 6002, &h);
        assert!(matches!(&ops[0], SegmentOp::Open { path, .. } if path == "/d/seg-6-00003.jsonl"));
        // Header write failure: still open, nothing counted → header-only at rotate.
        w.op_failed(&ops, 0, OpFailure::OpenHeader);
        assert_eq!(w.active_path(), Some("/d/seg-6-00003.jsonl"));
        let ops = w.finalize();
        assert_eq!(
            ops,
            vec![SegmentOp::Close {
                plain: "/d/seg-6-00003.jsonl".into(),
                gz: None
            }]
        );
        // Sample append failure: not counted; a header re-emit failure leaves samples alone.
        let mut w = SegmentWriterCore::new("/e", Some(10_000), None);
        let ops = w.write_line(&sample(0.0), 1, 0, &h);
        let last = ops.len() - 1;
        w.op_failed(&ops, last, OpFailure::Append);
        assert_eq!(
            w.finalize()[0],
            SegmentOp::Close {
                plain: "/e/seg-1-00000.jsonl".into(),
                gz: None
            }
        );
        let ops = w.write_line(&sample(1.0), 2, 0, &h);
        assert_eq!(ops.len(), 2);
        let re = w.note_time_source(TimeSource::Rtc, &h);
        w.op_failed(&re, 0, OpFailure::Append);
        assert!(matches!(
            &w.finalize()[0],
            SegmentOp::Close { gz: Some(_), .. }
        ));
        assert_eq!(gz_temp_path("/d/a.jsonl.gz", 42), "/d/a.jsonl.gz.tmp.42");
        assert_eq!(
            orphan_entry_action(&gz_temp_path("seg-1-00000.jsonl.gz", 7)),
            OrphanEntryAction::DeleteTemp
        );
        assert_eq!(
            GZIP_FINALIZE_STEPS.last(),
            Some(&GzipFinalizeStep::NotifyClosed)
        );
    }

    #[test]
    fn orphan_rules() {
        assert_eq!(
            orphan_entry_action("seg-3-00002.jsonl.gz.tmp.123"),
            OrphanEntryAction::DeleteTemp
        );
        assert_eq!(
            orphan_entry_action("seg-1-00000.jsonl.gz"),
            OrphanEntryAction::Skip
        );
        assert_eq!(
            orphan_entry_action("seg-1-00000.jsonl"),
            OrphanEntryAction::Finalize
        );
        assert_eq!(finalize_plan(b""), OrphanPlan::Delete);
        assert_eq!(finalize_plan(b"{\"ts\":1,\"mo"), OrphanPlan::Delete);
        assert_eq!(
            finalize_plan(b"{\"type\":\"header\"}\n"),
            OrphanPlan::Delete
        );
        assert_eq!(
            finalize_plan(b"h\ns\n{\"ts\":2,\"mo"),
            OrphanPlan::Finalize { keep_len: 4 }
        );
        assert_eq!(
            finalize_plan(b"h\n\n\ns\n"),
            OrphanPlan::Finalize { keep_len: 6 }
        );
        assert_eq!(finalize_plan(b"h\n\n\n"), OrphanPlan::Delete);
    }
}
