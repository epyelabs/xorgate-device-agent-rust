//! `agent.pid` — JSON identity of the running worker. Port of
//! `src/daemon/pidfile.ts:12-152` minus the start-lock helpers
//! (`lockPath`/`tryCreateLock`/`readLock`/`removeLock`), which only the TS
//! self-managed `xorgate start` uses (README §3.3 drops it; WP6 lists the
//! manager's start-lock cases as not ported).
//!
//! The Rust record omits `nodeVersion` (there is no Node) and writes
//! `worker` = this binary's path; the TS `parsePidRecord` fills absent keys
//! with defaults, so either agent's `status` reads the other's file (C10).
//! `argv` is the Rust argv after the program name (`["run", …]`; tolerated,
//! T8 / CF4).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use xorgate_core::telemetry::JsNumber;

/// What the Rust worker writes (`PidRecord`, `pidfile.ts:22-34`, without `nodeVersion`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PidRecord {
    pub pid: u32,
    /// Epoch ms at start — anchors the identity check against process start time.
    pub started_at: i64,
    pub device_id: String,
    pub mode: String,
    /// `JsNumber` so `1.0` prints as `1`, byte-shaped like the TS pidfile.
    pub rate_hz: JsNumber,
    pub offline: bool,
    /// Absolute path of the worker binary (the TS writes `dist/worker.js`).
    pub worker: String,
    pub argv: Vec<String>,
}

/// `parsePidRecord`'s result: every field present, defaults applied
/// (`pidfile.ts:37-58`). Read by `status` for either agent's file.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPidRecord {
    pub pid: u32,
    pub started_at: f64,
    pub device_id: String,
    /// `"real" | "replay" | "sim"` (anything else reads as `sim`).
    pub mode: String,
    pub rate_hz: f64,
    pub offline: bool,
    pub worker: String,
    pub argv: Vec<String>,
    pub node_version: String,
}

/// `pidPath()` (`runtime.ts:24`).
pub fn pid_path(home: &Path) -> PathBuf {
    home.join("agent.pid")
}

/// JS `String(value)` for a JSON value (the `argv.map(String)` of `:55`).
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => xorgate_core::js::number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// `parsePidRecord` (`pidfile.ts:37-58`): garbage / the old bare-int format /
/// a non-positive or non-integer pid / a missing `startedAt` or `deviceId` → `None`.
pub fn parse_pid_record(text: &str) -> Option<ParsedPidRecord> {
    let v: Value = serde_json::from_str(text).ok()?;
    let r = v.as_object()?;
    let pid = r.get("pid")?.as_f64()?;
    if pid.fract() != 0.0 || pid <= 0.0 || pid > u32::MAX as f64 {
        return None;
    }
    let started_at = r.get("startedAt")?.as_f64()?;
    let device_id = r.get("deviceId")?.as_str()?.to_string();
    let mode = match r.get("mode").and_then(Value::as_str) {
        Some("real") => "real",
        Some("replay") => "replay",
        _ => "sim",
    };
    Some(ParsedPidRecord {
        pid: pid as u32,
        started_at,
        device_id,
        mode: mode.to_string(),
        rate_hz: r.get("rateHz").and_then(Value::as_f64).unwrap_or(1.0),
        offline: r.get("offline") == Some(&Value::Bool(true)),
        worker: r
            .get("worker")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        argv: r
            .get("argv")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(js_string).collect())
            .unwrap_or_default(),
        node_version: r
            .get("nodeVersion")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// `readPidfile` (`pidfile.ts:61-69`): `None` when missing/unreadable/garbage.
pub fn read_pidfile(path: &Path) -> Option<ParsedPidRecord> {
    parse_pid_record(&xorgate_hal::read_lossy::read_text(path).ok()?)
}

/// `writePidfileExclusive` (`pidfile.ts:76-91`): O_EXCL, 0600, fsync.
/// `Ok(false)` when the file already exists.
pub fn write_pidfile_exclusive(path: &Path, record: &PidRecord) -> std::io::Result<bool> {
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    };
    f.write_all(&serde_json::to_vec(record).map_err(std::io::Error::other)?)?;
    f.sync_all()?;
    Ok(true)
}

/// `writePidfile` (`pidfile.ts:94-96`): atomic (over)write.
pub fn write_pidfile(path: &Path, record: &PidRecord) -> std::io::Result<()> {
    xorgate_hal::atomic_write::atomic_write_json(path, record)
}

/// `removePidfile` (`pidfile.ts:99-101` + `unlinkIgnoring` `:146-152`):
/// ENOENT is fine, anything else is an error.
pub fn remove_pidfile(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-pid-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// `sample()` of pidfile.test.ts:19-29 (as the Rust record: no nodeVersion).
    fn sample() -> PidRecord {
        PidRecord {
            pid: 4321,
            started_at: 1_700_000_000_000,
            device_id: "dev-uuid".into(),
            mode: "sim".into(),
            rate_hz: JsNumber(1.0),
            offline: true,
            worker: "/x/worker.js".into(),
            argv: vec!["--offline".into()],
        }
    }

    /// pidfile.test.ts:32 "parses a valid record" (+ the TS shape with nodeVersion).
    #[test]
    fn parses_a_valid_record() {
        let r = parse_pid_record(&serde_json::to_string(&sample()).unwrap()).unwrap();
        assert_eq!(r.pid, 4321);
        assert_eq!(r.device_id, "dev-uuid");
        assert!(r.offline);
        assert_eq!(r.node_version, "");
        let js = r#"{"pid":4321,"startedAt":1700000000000,"deviceId":"dev-uuid","mode":"real","rateHz":1,"offline":false,"worker":"/opt/xorgate/dist/worker.js","argv":[],"nodeVersion":"v20.0.0"}"#;
        let r = parse_pid_record(js).unwrap();
        assert_eq!(
            (r.mode.as_str(), r.node_version.as_str()),
            ("real", "v20.0.0")
        );
    }

    /// pidfile.test.ts:39 "rejects a bare integer (old format)".
    #[test]
    fn rejects_a_bare_integer() {
        assert!(parse_pid_record("4321").is_none());
    }

    /// pidfile.test.ts:43 "rejects garbage and non-positive pids".
    #[test]
    fn rejects_garbage_and_non_positive_pids() {
        assert!(parse_pid_record("{not json").is_none());
        assert!(parse_pid_record(r#"{"pid":0,"startedAt":1,"deviceId":"x"}"#).is_none());
        assert!(parse_pid_record(r#"{"startedAt":1,"deviceId":"x"}"#).is_none());
        assert!(parse_pid_record(r#"{"pid":1.5,"startedAt":1,"deviceId":"x"}"#).is_none());
        // Defaults for everything optional; argv elements through String().
        let r = parse_pid_record(r#"{"pid":7,"startedAt":1,"deviceId":"x","mode":"bogus","argv":[1,null,true,[1,null,2]]}"#).unwrap();
        assert_eq!(r.mode, "sim");
        assert_eq!(r.rate_hz, 1.0);
        assert_eq!(r.argv, ["1", "null", "true", "1,,2"]);
    }

    /// pidfile.test.ts:51 "writes exclusively then reads back".
    #[test]
    fn writes_exclusively_then_reads_back() {
        let h = home("excl");
        let p = pid_path(&h);
        assert!(write_pidfile_exclusive(&p, &sample()).unwrap());
        let r = read_pidfile(&p).unwrap();
        assert_eq!(r.pid, 4321);
        assert_eq!(r.argv, ["--offline"]);
        let _ = std::fs::remove_dir_all(&h);
    }

    /// pidfile.test.ts:58 "the O_EXCL guard rejects a second write".
    #[test]
    fn o_excl_guard_rejects_a_second_write() {
        let h = home("guard");
        let p = pid_path(&h);
        assert!(write_pidfile_exclusive(&p, &sample()).unwrap());
        let second = PidRecord {
            pid: 9999,
            ..sample()
        };
        assert!(!write_pidfile_exclusive(&p, &second).unwrap());
        assert_eq!(read_pidfile(&p).unwrap().pid, 4321);
        let _ = std::fs::remove_dir_all(&h);
    }

    /// pidfile.test.ts:65 "readPidfile returns null when absent".
    #[test]
    fn read_pidfile_absent() {
        let h = home("absent");
        assert!(read_pidfile(&pid_path(&h)).is_none());
        let _ = std::fs::remove_dir_all(&h);
    }

    /// pidfile.test.ts:69 "removePidfile is idempotent".
    #[test]
    fn remove_pidfile_is_idempotent() {
        let h = home("rm");
        let p = pid_path(&h);
        write_pidfile_exclusive(&p, &sample()).unwrap();
        remove_pidfile(&p).unwrap();
        assert!(read_pidfile(&p).is_none());
        remove_pidfile(&p).unwrap();
        let _ = std::fs::remove_dir_all(&h);
    }

    /// Serde exactness of the Rust record (`rateHz` 1 → `1`, never `1.0`).
    #[test]
    fn record_bytes() {
        let rec = PidRecord {
            pid: 4242,
            started_at: 1_791_513_164_828,
            device_id: "dev".into(),
            mode: "real".into(),
            rate_hz: JsNumber(1.0),
            offline: false,
            worker: "/opt/xorgate-rs/bin/xorgate-rs".into(),
            argv: vec!["run".into()],
        };
        assert_eq!(
            serde_json::to_string(&rec).unwrap(),
            r#"{"pid":4242,"startedAt":1791513164828,"deviceId":"dev","mode":"real","rateHz":1,"offline":false,"worker":"/opt/xorgate-rs/bin/xorgate-rs","argv":["run"]}"#
        );
        let half = PidRecord {
            rate_hz: JsNumber(0.5),
            ..rec
        };
        assert!(serde_json::to_string(&half)
            .unwrap()
            .contains(r#""rateHz":0.5,"#));
    }
}
