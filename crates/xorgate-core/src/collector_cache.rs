//! The recorder-side `sample()` caches of the lte and system collectors
//! (`src/agent/collectors/lte.ts:32-38,75-94`, `system.ts:89-114`): pure
//! decisions only — when to re-poll and what counts as "changed".
//!
//! TIME: the TS gates both caches on `Date.now() - lastPollAt >= 1000`. Here
//! the gate takes a MONOTONIC `now` (invariant 2; an addition to the five O2
//! intervals, recorded in the WP2 evidence file). Same values unless the
//! wall clock steps.

use serde_json::Value;

use crate::clock::MonoMs;
use crate::js::number_to_string;

/// The native cadence of both caches (`lte.ts:77`, `system.ts:106`).
pub const SAMPLE_POLL_MS: u64 = 1000;

/// `now - lastPollAt >= 1000` with `lastPollAt = 0` initially (`None` here:
/// the first call always polls).
pub fn poll_due(last_poll_at: Option<MonoMs>, now: MonoMs) -> bool {
    match last_poll_at {
        None => true,
        Some(at) => now.saturating_sub(at) >= SAMPLE_POLL_MS,
    }
}

/// The lte cache's change key `` `${t.timestamp}|${t.csq}` `` (`lte.ts:84`)
/// over the daemon file as parsed (`t` is the raw JSON document).
pub fn lte_change_key(doc: &Value) -> String {
    format!(
        "{}|{}",
        js_template(doc.get("timestamp")),
        js_template(doc.get("csq"))
    )
}

/// A JS template-literal substitution (`String(v)`) of a JSON value;
/// `None` = `undefined`.
pub fn js_template(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        // Array.prototype.toString = join(","), null/undefined → "".
        Some(Value::Array(a)) => a
            .iter()
            .map(|e| match e {
                Value::Null => String::new(),
                other => js_template(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn poll_gate() {
        assert!(poll_due(None, 0));
        assert!(!poll_due(Some(5_000), 5_999));
        assert!(poll_due(Some(5_000), 6_000));
    }

    #[test]
    fn change_key_is_the_template_literal() {
        assert_eq!(
            lte_change_key(&json!({"timestamp": "2026-06-13T08:21:45Z", "csq": 18})),
            "2026-06-13T08:21:45Z|18"
        );
        assert_eq!(lte_change_key(&json!({"csq": 18.5})), "undefined|18.5");
        assert_eq!(
            lte_change_key(&json!({"timestamp": null, "csq": [1, null, "a"]})),
            "null|1,,a"
        );
        assert_eq!(
            lte_change_key(&json!({"timestamp": {}, "csq": true})),
            "[object Object]|true"
        );
    }
}
