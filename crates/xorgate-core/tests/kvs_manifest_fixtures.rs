//! Fixture-driven tests for `xorgate_core::kvs_manifest`, from
//! `test/manifest.test.ts` (11 it()s, F cases as file-text → result) + extras,
//! dumped by tsx from `src/media/manifest.ts` @ 89b5c64. The same fixture runs
//! through the real file I/O in `xorgate-hal/tests/kvs_manifest_io_fixtures.rs`.

mod common;

use serde::Deserialize;
use xorgate_core::kvs_manifest::*;

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

/// What `readFileSync(path, "utf8")` returns for the case's file (`None` = absent).
fn file_text(c: &Case) -> Option<String> {
    match &c.input_b64 {
        Some(b) => Some(String::from_utf8_lossy(&b64(b)).into_owned()),
        None => c.input_text.clone(),
    }
}

#[test]
fn manifest_test_ts_load_and_persist() {
    let f = common::fixture::<Case>("kvs_manifest/manifest.json");
    let (mut loads, mut persists) = (0, 0);
    for c in &f.cases {
        let text = file_text(c);
        match c.op.as_str() {
            "loadManifest" => {
                loads += 1;
                let got = text.as_deref().and_then(parse_manifest_text);
                assert_eq!(
                    Some(serde_json::to_string(&got).unwrap()),
                    c.expected_json,
                    "{}",
                    c.name
                );
            }
            "persistUploadBucket" => {
                persists += 1;
                let d =
                    persist_upload_bucket_decision(text.as_deref(), c.bucket.as_deref().unwrap());
                let (outcome, after) = match &d {
                    PersistDecision::NoManifest => ("no-manifest", text.clone()),
                    PersistDecision::Unchanged => ("unchanged", text.clone()),
                    PersistDecision::Apply { text } => ("applied", Some(text.clone())),
                };
                assert_eq!(Some(outcome), c.expected.as_deref(), "{}", c.name);
                let want = c
                    .after_b64
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(&b64(b)).into_owned());
                assert_eq!(after, want, "{}: file after", c.name);
            }
            other => panic!("unknown op {other}"),
        }
    }
    assert_eq!((loads, persists), (26, 21));
}
