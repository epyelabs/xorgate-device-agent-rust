//! S3 object keys + name rules of the uploader's two segment sources
//! (`src/recording/telemetry-source.ts`, `src/recording/video-source.ts` @
//! `89b5c64`), as pure functions. The directory walks that feed them live in
//! `xorgate-agent::recording::sources`.
//!
//! Key contracts (README shared contracts; consumed by the cloud ingest):
//! - telemetry: `telemetry/v1/<deviceId>/<sessionId>/<startTsMs>-<seq>.jsonl.gz`
//!   — the on-disk `seg-<ms>-<seq>.jsonl.gz` name minus its `seg-` prefix;
//! - video: `media-sessions/v1/<deviceId>/<streamKey>/<sessionId>/NNNNN.{mp4,json}`
//!   (the `00000.mp4` suffix is kept verbatim: the thumbnail picker keys on it).
//!
//! The TS regexes are hand-matched here (no `regex` crate in core): every
//! rule below names the regex it mirrors. JS `\d` is ASCII `[0-9]` only and a
//! non-multiline `$` matches only at the very end (a trailing `\n` fails).

use std::cmp::Ordering;

/// `contentType` of the telemetry source (`telemetry-source.ts:20`).
pub const TELEMETRY_CONTENT_TYPE: &str = "application/gzip";
/// `contentType` of the video source (`video-source.ts:79`).
pub const VIDEO_CONTENT_TYPE: &str = "video/mp4";
/// The sidecar descriptor's `contentType` (`video-source.ts:163`).
pub const SIDECAR_CONTENT_TYPE: &str = "application/json";
/// `priority` of the telemetry source (`telemetry-source.ts:19`).
pub const TELEMETRY_PRIORITY: u8 = 0;
/// `priority` of the video source (`video-source.ts:78`).
pub const VIDEO_PRIORITY: u8 = 1;
/// The closed-segment suffix the telemetry walk lists (`telemetry-source.ts:39`).
pub const TELEMETRY_CLOSED_SUFFIX: &str = ".jsonl.gz";
/// `REAP_MIN_AGE_MS` (`video-source.ts:35`): a drained session dir younger
/// than this is never reaped (its capture daemon may still be starting).
pub const REAP_MIN_AGE_MS: f64 = 10.0 * 60_000.0;

fn all_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `SEGMENT_NAME = /^seg-(\d+)-(\d+)\.jsonl\.gz$/` (`telemetry-source.ts:14`):
/// the `(startTs, seq)` capture groups, or `None`.
pub fn telemetry_segment_parts(name: &str) -> Option<(&str, &str)> {
    let mid = name.strip_prefix("seg-")?.strip_suffix(".jsonl.gz")?;
    // `\d+` cannot contain '-', so exactly one '-' splits two digit runs.
    let (ts, seq) = mid.split_once('-')?;
    (all_ascii_digits(ts) && all_ascii_digits(seq)).then_some((ts, seq))
}

/// `s3KeyFor` of the telemetry source (`telemetry-source.ts:45-51`): `name`
/// is the listing's `<session>/<file>`. `const [session, name] =
/// seg.name.split("/")` keeps only the first two parts (a third is ignored),
/// and an empty/missing part is `null`.
pub fn telemetry_s3_key(device_id: &str, seg_name: &str) -> Option<String> {
    let mut parts = seg_name.split('/');
    let session = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    let (ts, seq) = telemetry_segment_parts(name)?;
    Some(format!("telemetry/v1/{device_id}/{session}/{ts}-{seq}.jsonl.gz"))
}

/// `UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/`
/// (`video-source.ts:28`) — lowercase hex only.
pub fn is_session_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(&c),
        })
}

/// `FRAGMENT_RE = /^\d{5,}\.mp4$/` (`video-source.ts:29`).
pub fn is_fragment_name(s: &str) -> bool {
    s.strip_suffix(".mp4")
        .is_some_and(|d| d.len() >= 5 && all_ascii_digits(d))
}

/// `STREAM_KEY_RE = /^[A-Za-z0-9_-]+$/` (`video-source.ts:30`).
pub fn is_stream_key(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `keyFor(streamKey, sessionId, fragment)` (`video-source.ts:51-56`).
pub fn video_key_for(
    device_id: &str,
    stream_key: &str,
    session_id: &str,
    fragment: &str,
) -> Option<String> {
    if !is_stream_key(stream_key) || !is_session_uuid(session_id) || !is_fragment_name(fragment) {
        return None;
    }
    Some(format!(
        "media-sessions/v1/{device_id}/{stream_key}/{session_id}/{fragment}"
    ))
}

/// `s3KeyFor` of the video source (`video-source.ts:174-178`): `name` is
/// `<streamKey>/<sessionId>/<fragment>`; any other part count is `null`.
pub fn video_s3_key(device_id: &str, seg_name: &str) -> Option<String> {
    let parts: Vec<&str> = seg_name.split('/').collect();
    match parts.as_slice() {
        [stream, session, fragment] => video_key_for(device_id, stream, session, fragment),
        _ => None,
    }
}

/// `s.replace(/\.mp4$/, ".json")` (`video-source.ts:144,162`): the sidecar
/// name/key next to an mp4 name/key; unchanged when it does not end `.mp4`.
pub fn mp4_to_json(s: &str) -> String {
    match s.strip_suffix(".mp4") {
        Some(stem) => format!("{stem}.json"),
        None => s.to_string(),
    }
}

/// `n.replace(/\.json$/, ".mp4")` (`video-source.ts:113`): the mp4 a sidecar
/// pairs with; unchanged when it does not end `.json`.
pub fn json_to_mp4(s: &str) -> String {
    match s.strip_suffix(".json") {
        Some(stem) => format!("{stem}.mp4"),
        None => s.to_string(),
    }
}

/// The drained-session reap rule (`video-source.ts:122-139`): only an
/// inactive session whose manifest is at least [`REAP_MIN_AGE_MS`] old IN
/// MAGNITUDE (`Math.abs(now - mtimeMs)` — a future mtime from a wrong clock
/// must not make the husk immortal). `manifest_mtime_ms = None` (no manifest)
/// keeps `manifestAge = Infinity` → reap.
pub fn should_reap_drained_session(active: bool, now_ms: f64, manifest_mtime_ms: Option<f64>) -> bool {
    if active {
        return false;
    }
    let age = manifest_mtime_ms.map_or(f64::INFINITY, |m| (now_ms - m).abs());
    age >= REAP_MIN_AGE_MS
}

/// `name === SESSION_MANIFEST_NAME || name.endsWith(".json")` — what
/// `reapSession` unlinks before the `rmdir` (`video-source.ts:59-68`).
pub fn reaped_by_session_reap(name: &str) -> bool {
    name == crate::sidecar::SESSION_MANIFEST_NAME || name.ends_with(".json")
}

/// JS `a < b` / default `Array.prototype.sort()` order on strings: UTF-16
/// code-unit order (differs from Rust's byte order only above the BMP).
pub fn js_string_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// The video listing's comparator (`video-source.ts:171`):
/// `a.mtimeMs - b.mtimeMs || (a.name < b.name ? -1 : 1)`.
pub fn video_listing_cmp(a_mtime_ms: f64, a_name: &str, b_mtime_ms: f64, b_name: &str) -> Ordering {
    let d = a_mtime_ms - b_mtime_ms;
    if d != 0.0 && !d.is_nan() {
        return if d < 0.0 { Ordering::Less } else { Ordering::Greater };
    }
    if js_string_cmp(a_name, b_name) == Ordering::Less {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

/// Node's `Stats.mtimeMs` from a `timespec`: `sec * 1e3 + nsec / 1e6` as a
/// double (`lib/internal/fs/utils.js` `msFromTimeSpec`).
pub fn ms_from_timespec(sec: i64, nsec: i64) -> f64 {
    sec as f64 * 1e3 + nsec as f64 / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_edges() {
        assert_eq!(telemetry_segment_parts("seg-1-2.jsonl.gz"), Some(("1", "2")));
        assert_eq!(telemetry_segment_parts("seg-1-2-3.jsonl.gz"), None);
        assert!(is_fragment_name("00000.mp4"));
        assert!(!is_fragment_name("0000.mp4"));
        assert!(is_session_uuid("0198aaaa-0000-7000-8000-000000000001"));
        assert!(!is_session_uuid("0198AAAA-0000-7000-8000-000000000001"));
        assert!(is_stream_key("cam-0_A"));
        assert!(!is_stream_key("cam.0"));
    }

    #[test]
    fn reap_rule_uses_the_age_magnitude() {
        assert!(should_reap_drained_session(false, 1e12, None));
        assert!(!should_reap_drained_session(true, 1e12, None));
        assert!(should_reap_drained_session(false, 0.0, Some(REAP_MIN_AGE_MS)));
        assert!(!should_reap_drained_session(false, 0.0, Some(REAP_MIN_AGE_MS - 1.0)));
        assert!(should_reap_drained_session(false, 0.0, Some(-REAP_MIN_AGE_MS)));
    }

    #[test]
    fn listing_order_is_mtime_then_name() {
        assert_eq!(video_listing_cmp(1.0, "b", 2.0, "a"), Ordering::Less);
        assert_eq!(video_listing_cmp(2.0, "a", 2.0, "b"), Ordering::Less);
        assert_eq!(video_listing_cmp(2.0, "b", 2.0, "a"), Ordering::Greater);
        assert_eq!(js_string_cmp("\u{FFFF}", "\u{10000}"), Ordering::Greater);
    }
}
