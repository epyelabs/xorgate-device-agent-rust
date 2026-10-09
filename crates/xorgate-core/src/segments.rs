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
/// keeps insertion order (`serde_json` `preserve_order`), so the line prints
/// exactly as `JSON.stringify` did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleLine {
    #[serde(with = "js_num")]
    pub ts: f64,
    #[serde(with = "js_num")]
    pub mono: f64,
    #[serde(with = "js_num")]
    pub seq: f64,
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
        }
    }

    /// `activePath()` (`segments.ts:111-113`): the active plain `.jsonl`, or
    /// `None` when no segment is open.
    pub fn active_path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// `writeLine(line, now)` (`segments.ts:116-121`). Segments open lazily
    /// on the first sample, so idle sessions leave no files. `header` is what
    /// `opts.header()` would return right now (the recorder builds it at open
    /// so `timeSource` is current); `now` names the file, `mono` ages it.
    pub fn write_line(
        &mut self,
        line: &SampleLine,
        now: WallMs,
        mono: MonoMs,
        header: &SegmentHeader,
    ) -> Vec<SegmentOp> {
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
    pub fn note_time_source(&mut self, ts: TimeSource, header: &SegmentHeader) -> Vec<SegmentOp> {
        if self.path.is_none() || self.last_time_source == Some(ts) {
            self.last_time_source = Some(ts);
            return Vec::new();
        }
        self.last_time_source = Some(ts);
        let mut h = header.clone();
        h.time_source = ts;
        self.append(serde_json::to_string(&h).expect("SegmentHeader serialises"))
    }

    /// `maybeRotate()` (`segments.ts:141-143`): age/size check without a
    /// write — an idle recorder still closes segments on time.
    pub fn maybe_rotate(&mut self, mono: MonoMs) -> Vec<SegmentOp> {
        if self.path.is_some() && self.should_rotate(mono) {
            self.rotate()
        } else {
            Vec::new()
        }
    }

    /// `finalize()` (`segments.ts:146-148`): close + gzip the active segment
    /// (shutdown / recording-off path). Safe to re-call.
    pub fn finalize(&mut self) -> Vec<SegmentOp> {
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
    fn open(&mut self, now: WallMs, mono: MonoMs, header: &SegmentHeader) -> SegmentOp {
        let path = posix_join(&self.dir, &segment_file_name(now, self.seg_seq));
        self.seg_seq += 1;
        self.path = Some(path.clone());
        self.bytes = 0;
        self.sample_lines = 0;
        self.opened_at_mono = mono;
        self.last_time_source = Some(header.time_source);
        let line = serde_json::to_string(header).expect("SegmentHeader serialises");
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
        let h = header();
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
        let h = header();
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
