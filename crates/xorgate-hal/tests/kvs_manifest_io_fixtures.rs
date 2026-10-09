//! `xorgate_hal::kvs_manifest_io` against the tsx-dumped
//! `fixtures/kvs_manifest/manifest.json` (`test/manifest.test.ts` + extras),
//! through REAL files: absent path, raw bytes (incl. invalid UTF-8), the
//! atomic rewrite, and the byte-exact file left behind.

use std::path::PathBuf;

use serde::Deserialize;
use xorgate_hal::kvs_manifest_io::{load_manifest, persist_upload_bucket};

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "jsCommit")]
    js_commit: String,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    #[serde(rename = "inputText")]
    input_text: Option<String>,
    #[serde(rename = "inputB64")]
    input_b64: Option<String>,
    #[serde(rename = "expectedJson")]
    expected_json: Option<String>,
    bucket: Option<String>,
    expected: Option<String>,
    #[serde(rename = "afterB64")]
    after_b64: Option<String>,
    #[serde(rename = "afterLoadJson")]
    after_load_json: Option<String>,
}

fn b64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("bad b64"),
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= (val(*c) as u32) << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push((acc >> (16 - 8 * i)) as u8);
        }
    }
    out
}

#[test]
fn manifest_io_matches_ts_byte_for_byte() {
    let path = format!(
        "{}/../../fixtures/kvs_manifest/manifest.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let f: Fixture = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(f.js_commit, "89b5c6407ef88e20f7168bd767d22a7be04ebcb6");
    let dir: PathBuf = std::env::temp_dir().join(format!("xg-kvs-io-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (i, c) in f.cases.iter().enumerate() {
        let p = dir.join(format!("kvs-{i}.json"));
        let input: Option<Vec<u8>> = match (&c.input_b64, &c.input_text) {
            (Some(b), _) => Some(b64(b)),
            (None, Some(t)) => Some(t.as_bytes().to_vec()),
            (None, None) => None,
        };
        if let Some(bytes) = &input {
            std::fs::write(&p, bytes).unwrap();
        }
        match c.op.as_str() {
            "loadManifest" => {
                let got = serde_json::to_string(&load_manifest(&p)).unwrap();
                assert_eq!(Some(got), c.expected_json, "{}", c.name);
            }
            "persistUploadBucket" => {
                let got = persist_upload_bucket(&p, c.bucket.as_deref().unwrap()).unwrap();
                assert_eq!(Some(got.as_str()), c.expected.as_deref(), "{}", c.name);
                let after = std::fs::read(&p).ok();
                assert_eq!(after, c.after_b64.as_deref().map(b64), "{}: bytes", c.name);
                let reload = serde_json::to_string(&load_manifest(&p)).unwrap();
                assert_eq!(Some(reload), c.after_load_json, "{}: reload", c.name);
            }
            other => panic!("unknown op {other}"),
        }
    }
    assert_eq!(f.cases.len(), 47);
    // No temp files left behind by the atomic writer.
    let stray: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(stray.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn persist_write_failure_is_an_error_not_no_manifest() {
    // A readable manifest in a directory we cannot write into: atomicWriteJson
    // throws in the TS, so the worker logs "uploadBucket persist failed".
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores directory permissions
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("xg-kvs-ro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("kvs.json");
    std::fs::write(&p, "{}").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let r = persist_upload_bucket(&p, "b");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(r.is_err(), "{r:?}");
}
