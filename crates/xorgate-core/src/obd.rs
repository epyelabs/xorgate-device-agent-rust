//! OBD2 peripheral — the PURE half of `src/hardware/obd.ts` and the pure
//! helpers of `src/agent/collectors/obd.ts` (JS @ 09da38d).
//!
//! Phase 5b step 0 (WP1) declares only the command-result types the spine
//! needs (`obd_scan` → [`ObdScanResult`], `effective.obd.scan`); WP2 owns
//! this module and fills in the rest (contract-1 parser, resolve, argv,
//! scan normalisation, units, status line, …).

use serde::{Deserialize, Serialize};

use crate::telemetry::{js_num, js_num_opt};

/// One discovered device (`ObdScanCandidate`, `ts:src/hardware/obd.ts:329-333`
/// @ 09da38d), as `effective.obd.scan.candidates[]` carries it. Key order =
/// the TS literal `{ mac, name, rssi }`; `rssi` is `null` when unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObdScanCandidate {
    pub mac: String,
    pub name: String,
    /// `Math.round(rssi)` (a JS number) or `null`.
    #[serde(with = "js_num_opt")]
    pub rssi: Option<f64>,
}

/// Result of an `obd_scan` (`ObdScanResult`,
/// `ts:src/agent/collectors/obd.ts:122-129` @ 09da38d), as
/// `effective.obd.scan` carries it. `error` is present only when the scan
/// helper failed (candidates then empty).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObdScanResult {
    /// Epoch ms the scan finished (a JS number).
    #[serde(with = "js_num")]
    pub ts: f64,
    pub candidates: Vec<ObdScanCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_result_key_order_and_optional_error() {
        let r = ObdScanResult {
            ts: 1_700_000_000_000.0,
            candidates: vec![ObdScanCandidate {
                mac: "AA:BB:CC:DD:EE:FF".into(),
                name: "OBDII".into(),
                rssi: None,
            }],
            error: None,
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"ts":1700000000000,"candidates":[{"mac":"AA:BB:CC:DD:EE:FF","name":"OBDII","rssi":null}]}"#
        );
        let e = ObdScanResult {
            ts: 1.0,
            candidates: vec![],
            error: Some("boom".into()),
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"ts":1,"candidates":[],"error":"boom"}"#
        );
    }
}
