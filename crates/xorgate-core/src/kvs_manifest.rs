//! KVS provisioning manifest (`kvs.json`) — pure half of
//! `src/media/manifest.ts` (`loadManifest` :44-84, `persistUploadBucket`
//! :99-115). The HAL wrapper (`xorgate_hal::kvs_manifest_io`) reads the file
//! as bytes, decodes it lossily (`readFileSync(path, "utf8")`) and hands the
//! text here; this module decides, the HAL writes.
//!
//! `loadManifest` NEVER fails: a missing/corrupt/incomplete file is "no media
//! config" (`None`), so callers self-gate instead of crashing.
//!
//! JS-compat helpers kept PRIVATE here (phase-2 timing note: the Phase 1
//! integration is moving `js_fmt` into `xorgate_core::js`): `js_number_to_string`
//! (`Number.prototype.toString()`) and `js_json_stringify` (`JSON.stringify`
//! of a parsed document). Fold them into `xorgate_core::js` at integration.

use serde::Serialize;
use serde_json::{Map, Value};

/// `KvsChannel` (`manifest.ts:19-22`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KvsChannel {
    #[serde(rename = "streamKey")]
    pub stream_key: String,
    #[serde(rename = "channelName")]
    pub channel_name: String,
}

/// `KvsManifest` (`manifest.ts:24-37`). Field order = the TS object literal
/// of `loadManifest` (:76-83); `uploadBucket` omitted when absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KvsManifest {
    #[serde(rename = "deviceId")]
    pub device_id: String,
    pub region: String,
    #[serde(rename = "credentialEndpoint")]
    pub credential_endpoint: String,
    #[serde(rename = "roleAlias")]
    pub role_alias: String,
    pub channels: Vec<KvsChannel>,
    #[serde(rename = "uploadBucket", skip_serializing_if = "Option::is_none")]
    pub upload_bucket: Option<String>,
}

/// `nonEmptyString` (`manifest.ts:39-41`).
fn non_empty_string(v: Option<&Value>) -> Option<&str> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// `JSON.parse(text)` for the manifest: `None` where V8 throws. serde_json and
/// V8 agree on the grammar for every document the installer writes; the known
/// gaps (`1e400` → Infinity, lone-surrogate escapes) are accepted divergences
/// (phase-1 `accepted-divergences.md`, tolerance T7) and read as "no manifest".
fn json_parse(text: &str) -> Option<Value> {
    serde_json::from_str(text).ok()
}

/// `loadManifest` (`manifest.ts:44-84`) over the already-decoded file text.
/// `None` for unparseable text, a non-object root, or a missing/empty
/// `deviceId`/`region`/`credentialEndpoint`/`roleAlias`. Malformed channel
/// entries are dropped; `uploadBucket` is kept only when a non-empty string.
pub fn parse_manifest_text(text: &str) -> Option<KvsManifest> {
    parse_manifest_value(&json_parse(text)?)
}

/// `loadManifest` (`manifest.ts:51-83`) over a parsed document.
/// `!raw || typeof raw !== "object"` lets arrays through (`typeof [] ===
/// "object"`), which then fail the field checks — same result, `None`.
pub fn parse_manifest_value(raw: &Value) -> Option<KvsManifest> {
    let o = raw.as_object()?;
    let device_id = non_empty_string(o.get("deviceId"))?;
    let region = non_empty_string(o.get("region"))?;
    let credential_endpoint = non_empty_string(o.get("credentialEndpoint"))?;
    let role_alias = non_empty_string(o.get("roleAlias"))?;

    // `manifest.ts:63-74`: only `{streamKey, channelName}` objects with two
    // non-empty strings survive; `c && typeof c === "object"` admits arrays,
    // which then have neither key.
    let mut channels = Vec::new();
    if let Some(Value::Array(list)) = o.get("channels") {
        for c in list {
            if let Some(c) = c.as_object() {
                if let (Some(stream_key), Some(channel_name)) = (
                    non_empty_string(c.get("streamKey")),
                    non_empty_string(c.get("channelName")),
                ) {
                    channels.push(KvsChannel {
                        stream_key: stream_key.to_string(),
                        channel_name: channel_name.to_string(),
                    });
                }
            }
        }
    }

    Some(KvsManifest {
        device_id: device_id.to_string(),
        region: region.to_string(),
        credential_endpoint: credential_endpoint.to_string(),
        role_alias: role_alias.to_string(),
        channels,
        upload_bucket: non_empty_string(o.get("uploadBucket")).map(str::to_string),
    })
}

/// The three outcomes of `persistUploadBucket` (`manifest.ts:99-115`); the
/// `Apply` arm carries the exact bytes `atomicWriteJson` would write
/// (`JSON.stringify(o)`, compact, no trailing newline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistDecision {
    /// `"no-manifest"`: absent/unreadable/unparseable file, or a root that is
    /// not a plain object (`!raw || typeof raw !== "object" || Array.isArray(raw)`).
    NoManifest,
    /// `"unchanged"`: `o.uploadBucket === bucket` (retained redelivery no-op).
    Unchanged,
    /// `"applied"`: write `text` atomically over the manifest.
    Apply { text: String },
}

/// `persistUploadBucket` decision (`manifest.ts:103-114`). `text` is `None`
/// when the read failed (`readFileSync` threw). Every other field — unknown
/// ones included — is preserved in its original position; a new
/// `uploadBucket` key is appended last (JS property order), an existing one is
/// overwritten in place. No validation: an incomplete `{}` is still backfilled.
pub fn persist_upload_bucket_decision(text: Option<&str>, bucket: &str) -> PersistDecision {
    let Some(Value::Object(mut o)) = text.and_then(json_parse) else {
        return PersistDecision::NoManifest;
    };
    if matches!(o.get("uploadBucket"), Some(Value::String(s)) if s == bucket) {
        return PersistDecision::Unchanged;
    }
    // serde_json's preserve_order map keeps an existing key's position on
    // insert and appends a new one — the JS `o.uploadBucket = bucket` order.
    o.insert(
        "uploadBucket".to_string(),
        Value::String(bucket.to_string()),
    );
    PersistDecision::Apply {
        text: js_json_stringify(&Value::Object(o)),
    }
}

/// `JSON.stringify(value)` (no indent) for a document that came out of
/// `JSON.parse`: every number is an IEEE double printed by
/// `Number.prototype.toString` (`1.0` → `1`, `1e21` → `1e+21`,
/// `12345678901234567890` → `12345678901234567000`); strings use the JSON
/// escapes both sides share (`"`, `\`, `\b\f\n\r\t`, other C0 as `\u00xx`);
/// object keys in JS property order (integer-like keys first).
fn js_json_stringify(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v);
    out
}

fn write_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match n.as_f64() {
            Some(x) if x.is_finite() => out.push_str(&js_number_to_string(x)),
            _ => out.push_str("null"),
        },
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, x);
            }
            out.push(']');
        }
        Value::Object(o) => write_object(out, o),
    }
}

/// An array-index property key (ES2023 §6.1.7): the canonical decimal form of
/// an integer in `0..=2^32-2`. V8 enumerates these first, ascending.
fn array_index(k: &str) -> Option<u32> {
    let n: u32 = k.parse().ok()?;
    (n != u32::MAX && n.to_string() == k).then_some(n)
}

fn write_object(out: &mut String, o: &Map<String, Value>) {
    // OrdinaryOwnPropertyKeys order: integer indices ascending, then string
    // keys in insertion (= parse) order.
    let mut entries: Vec<(&String, &Value)> = o.iter().collect();
    entries.sort_by_key(|(k, _)| match array_index(k) {
        Some(n) => (0u8, n),
        None => (1u8, 0),
    });
    out.push('{');
    for (i, (k, x)) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(k).unwrap_or_default());
        out.push(':');
        write_value(out, x);
    }
    out.push('}');
}

/// `Number.prototype.toString()` for a finite double (ES2023 §6.1.6.1.20):
/// Rust's `{:e}` yields the same shortest round-trip digit string V8 uses;
/// only the layout rules differ.
fn js_number_to_string(x: f64) -> String {
    if x == 0.0 {
        return "0".to_string();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let sci = format!("{:e}", x.abs());
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let e: i64 = exp.parse().unwrap_or(0);
    let k = digits.len() as i64;
    let n = e + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let es = if n >= 1 { "+" } else { "-" };
        let ea = (n - 1).abs();
        if k == 1 {
            format!("{digits}e{es}{ea}")
        } else {
            format!("{}.{}e{es}{ea}", &digits[..1], &digits[1..])
        }
    };
    format!("{sign}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_to_string_layouts() {
        for (x, s) in [
            (1.0, "1"),
            (2.5, "2.5"),
            (1e21, "1e+21"),
            (1.5e300, "1.5e+300"),
            (1.5e-7, "1.5e-7"),
            (0.000001, "0.000001"),
            (1e-7, "1e-7"),
            (-0.0, "0"),
            (123456789012345680000.0, "123456789012345680000"),
            (0.30000000000000004, "0.30000000000000004"),
            (-12.75, "-12.75"),
            (100.0, "100"),
        ] {
            assert_eq!(js_number_to_string(x), s, "{x}");
        }
    }

    #[test]
    fn manifest_serialises_in_ts_key_order() {
        let m = KvsManifest {
            device_id: "d".into(),
            region: "r".into(),
            credential_endpoint: "c".into(),
            role_alias: "a".into(),
            channels: vec![KvsChannel {
                stream_key: "cam0".into(),
                channel_name: "n".into(),
            }],
            upload_bucket: None,
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"deviceId":"d","region":"r","credentialEndpoint":"c","roleAlias":"a","channels":[{"streamKey":"cam0","channelName":"n"}]}"#
        );
    }
}
