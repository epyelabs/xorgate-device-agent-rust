//! C9 (signer half): `xorgate_cloud::sigv4` is byte-equal to the TS `signPut`
//! / `encodeS3Key` on the tsx-dumped `fixtures/s3/sign-put.json`
//! (`test/recording-s3.test.ts` 3 signPut/encodeS3Key it()s + vectors: keys
//! with spaces/unicode/`+`/`=`, empty body, 30 MB body, session token,
//! 20261009T000000Z, leap days).

use serde::Deserialize;
use serde_json::Value;
use xorgate_cloud::sigv4::{encode_s3_key, sign_put, SigningCredentials};

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "jsSourceFile")]
    js_source_file: String,
    #[serde(rename = "jsCommit")]
    js_commit: String,
    #[serde(rename = "generatedBy")]
    generated_by: String,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
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
fn sign_put_matches_ts_byte_for_byte() {
    let path = format!(
        "{}/../../fixtures/s3/sign-put.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let f: Fixture = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(f.js_source_file.contains("s3-put.ts"));
    assert!(
        [
            "09da38dcc8b75c248625a3a0bee43b70e7def55d",
            "89b5c6407ef88e20f7168bd767d22a7be04ebcb6"
        ]
        .contains(&f.js_commit.as_str()),
        "jsCommit {} (CF78)",
        f.js_commit
    );
    assert!(!f.generated_by.is_empty());
    let (mut signs, mut encodes) = (0, 0);
    for c in &f.cases {
        match c.op.as_str() {
            "signPut" => {
                signs += 1;
                let i = &c.input;
                let s = |k: &str| i[k].as_str().unwrap().to_string();
                let body: Vec<u8> = if let Some(b) = i.get("bodyB64") {
                    b64(b.as_str().unwrap())
                } else {
                    let fill = &i["bodyFill"];
                    vec![
                        fill["byte"].as_u64().unwrap() as u8;
                        fill["len"].as_u64().unwrap() as usize
                    ]
                };
                let cr = &i["credentials"];
                let signed = sign_put(
                    &s("region"),
                    &s("bucket"),
                    &s("key"),
                    &body,
                    &s("contentType"),
                    SigningCredentials {
                        access_key_id: cr["accessKeyId"].as_str().unwrap(),
                        secret_access_key: cr["secretAccessKey"].as_str().unwrap(),
                        session_token: cr["sessionToken"].as_str().unwrap(),
                    },
                    i["nowMs"].as_f64().unwrap(),
                );
                assert_eq!(signed.host, c.expected["host"], "{}: host", c.name);
                assert_eq!(signed.path, c.expected["path"], "{}: path", c.name);
                let want: Vec<(String, String)> =
                    serde_json::from_value(c.expected["headers"].clone()).unwrap();
                assert_eq!(signed.headers, want, "{}: headers", c.name);
            }
            "encodeS3Key" => {
                encodes += 1;
                assert_eq!(
                    encode_s3_key(c.input.as_str().unwrap()),
                    c.expected.as_str().unwrap(),
                    "{}",
                    c.name
                );
            }
            other => panic!("unknown op {other}"),
        }
    }
    assert_eq!((signs, encodes), (15, 15));
}
