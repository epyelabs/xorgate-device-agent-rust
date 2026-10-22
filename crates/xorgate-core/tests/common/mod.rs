//! Shared fixture loader for the WP1+ fixture tests (`fixtures/<module>/<suite>.json`,
//! each tagged with the JS source file + full commit sha it came from).

#![allow(dead_code)]

use serde::Deserialize;

/// The JS repo commit every Phase 1 fixture was extracted from.
pub const JS_COMMIT: &str = "89b5c6407ef88e20f7168bd767d22a7be04ebcb6";
/// The Phase 5b reference (`09da38d`): fixtures dumped or re-dumped there
/// carry it (CF78 re-stamps the rest at the end of the phase).
pub const JS_COMMIT_P5B: &str = "09da38dcc8b75c248625a3a0bee43b70e7def55d";

#[derive(Deserialize)]
pub struct Tagged<C> {
    #[serde(rename = "jsSourceFile")]
    pub js_source_file: String,
    #[serde(rename = "jsCommit")]
    pub js_commit: String,
    #[serde(rename = "generatedBy")]
    pub generated_by: String,
    pub cases: Vec<C>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

pub fn fixture_path(rel: &str) -> String {
    format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"))
}

pub fn fixture_text(rel: &str) -> String {
    let path = fixture_path(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

pub fn fixture<C: for<'de> Deserialize<'de>>(rel: &str) -> Tagged<C> {
    let path = fixture_path(rel);
    let f: Tagged<C> =
        serde_json::from_str(&fixture_text(rel)).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert!(f.js_source_file.contains(".ts"), "{path}: jsSourceFile");
    assert!(
        f.js_commit == JS_COMMIT || f.js_commit == JS_COMMIT_P5B,
        "{path}: jsCommit {}",
        f.js_commit
    );
    assert!(!f.generated_by.is_empty(), "{path}: generatedBy");
    assert!(!f.cases.is_empty(), "{path}: no cases");
    f
}

/// `{"$undefined": true}` marks a JS `undefined` input in the fixtures.
pub fn is_undefined_marker(v: &serde_json::Value) -> bool {
    v.get("$undefined") == Some(&serde_json::Value::Bool(true))
}
