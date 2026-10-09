//! KVS provisioning manifest (`kvs.json`) — pure half of
//! `src/media/manifest.ts` (`loadManifest` :44-84, `persistUploadBucket`
//! :99-115). The HAL wrapper (`xorgate_hal::kvs_manifest_io`) reads the file
//! as bytes, decodes it lossily (`readFileSync(path, "utf8")`) and hands the
//! text here; this module decides, the HAL writes.
//!
//! `loadManifest` NEVER fails: a missing/corrupt/incomplete file is "no media
//! config" (`None`), so callers self-gate instead of crashing.
//!
//! The `persistUploadBucket` rewrite is `JSON.stringify` of the re-parsed
//! document: V8 key order ([`crate::js::parsed_key_order`]) and V8 number
//! text ([`crate::js::to_json_string`]).

use serde::Serialize;
use serde_json::Value;

use crate::js;

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

/// The gate of `buildUploadOptions` (`worker.ts:391-398`, after the offline
/// early return): `bucket = process.env.XORGATE_UPLOAD_BUCKET ||
/// manifest?.uploadBucket` (an empty env value falls through). `Err` carries
/// the parenthesised reason of the verbatim log line
/// `[agent] segment upload disabled (<reason>)`; a missing manifest wins over
/// a missing bucket even when the env names one.
pub fn upload_bucket_decision(
    manifest: Option<&KvsManifest>,
    env_upload_bucket: Option<&str>,
) -> Result<String, &'static str> {
    let bucket = env_upload_bucket
        .filter(|b| !b.is_empty())
        .map(str::to_string)
        .or_else(|| manifest.and_then(|m| m.upload_bucket.clone()));
    match (manifest, bucket) {
        (None, _) => Err("no kvs.json manifest"),
        (Some(_), None) => Err("no upload bucket configured"),
        (Some(_), Some(b)) => Ok(b),
    }
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
    // `JSON.stringify(o)`: V8 own-key order (integer-like keys first, at
    // every level) and V8 number text.
    PersistDecision::Apply {
        text: js::to_json_string(&js::parsed_key_order(Value::Object(o))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_bucket_decision_matches_build_upload_options() {
        let mut m = parse_manifest_text(
            r#"{"deviceId":"d","region":"r","credentialEndpoint":"c","roleAlias":"a"}"#,
        )
        .unwrap();
        assert_eq!(
            upload_bucket_decision(None, None),
            Err("no kvs.json manifest")
        );
        assert_eq!(
            upload_bucket_decision(None, Some("env")),
            Err("no kvs.json manifest")
        );
        assert_eq!(
            upload_bucket_decision(Some(&m), None),
            Err("no upload bucket configured")
        );
        assert_eq!(
            upload_bucket_decision(Some(&m), Some("")),
            Err("no upload bucket configured")
        );
        assert_eq!(
            upload_bucket_decision(Some(&m), Some("env")),
            Ok("env".into())
        );
        m.upload_bucket = Some("file".into());
        assert_eq!(upload_bucket_decision(Some(&m), None), Ok("file".into()));
        assert_eq!(
            upload_bucket_decision(Some(&m), Some("")),
            Ok("file".into())
        );
        assert_eq!(
            upload_bucket_decision(Some(&m), Some("env")),
            Ok("env".into())
        );
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
