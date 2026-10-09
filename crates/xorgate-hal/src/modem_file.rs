//! The modem daemon's telemetry snapshot (`/run/sim7600-lte/telemetry.json`)
//! — `src/hardware/modem.ts:31` (`LTE_TELEMETRY_FILE`) and `:203-212`
//! (`readModemTelemetry`).
//!
//! CF16: the file is read ONCE per consumer tick into a [`ModemSnapshot`]
//! that carries both the parsed JSON value (`raw`, for
//! `config_report::modem_bias_from_telemetry` and the report's sim/apn/uplink
//! blocks) and the typed view (`typed`, for the lte group, the LED and the
//! clock stepper), so every consumer sees the same bytes.
//!
//! Semantics of the TS reader: any read error or a JSON parse failure is
//! `null` (the caller omits the group for that tick). The bytes are decoded
//! lossily before parsing (CF21). Every read is reported to the tape
//! (`lte_file`; the writer keeps only content changes).

use std::path::{Path, PathBuf};

use serde_json::Value;
use xorgate_core::types::ModemTelemetry;

use crate::io::TapeSink;
use crate::read_lossy::decode;

/// `LTE_TELEMETRY_FILE` default (`modem.ts:31`).
pub const DEFAULT_LTE_TELEMETRY_FILE: &str = "/run/sim7600-lte/telemetry.json";

/// `process.env.LTE_TELEMETRY_FILE ?? "/run/sim7600-lte/telemetry.json"`
/// (`??`: an empty value is kept, and then simply fails to read).
pub fn lte_telemetry_file(env_value: Option<String>) -> PathBuf {
    PathBuf::from(env_value.unwrap_or_else(|| DEFAULT_LTE_TELEMETRY_FILE.to_string()))
}

/// One read of the daemon's file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModemSnapshot {
    /// The parsed document (`JSON.parse` result, any JSON type); `None` when
    /// the file is absent/unreadable or not JSON.
    pub raw: Option<Value>,
    /// `readModemTelemetry`'s result: `None` exactly when the TS returns
    /// `null` or a falsy value (see `ModemTelemetry::from_value`).
    pub typed: Option<ModemTelemetry>,
}

impl ModemSnapshot {
    fn from_bytes(bytes: Option<Vec<u8>>) -> Self {
        let Some(bytes) = bytes else {
            return Self::default();
        };
        match serde_json::from_str::<Value>(&decode(bytes)) {
            Ok(v) => Self {
                typed: ModemTelemetry::from_value(&v),
                raw: Some(v),
            },
            Err(_) => Self::default(),
        }
    }
}

fn report(tape: &dyn TapeSink, res: &std::io::Result<Vec<u8>>) {
    match res {
        Ok(b) => tape.lte_file(Some(b)),
        Err(_) => tape.lte_file(None),
    }
}

/// `readModemTelemetry(file)` (async, the collector / config-sync path).
pub async fn read(path: &Path, tape: &dyn TapeSink) -> ModemSnapshot {
    let res = tokio::fs::read(path).await;
    report(tape, &res);
    ModemSnapshot::from_bytes(res.ok())
}

/// Blocking variant (the CLI's `set cellular.apn` watch).
pub fn read_blocking(path: &Path, tape: &dyn TapeSink) -> ModemSnapshot {
    let res = std::fs::read(path);
    report(tape, &res);
    ModemSnapshot::from_bytes(res.ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::NullTape;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Rec(Mutex<Vec<Option<Vec<u8>>>>);
    impl TapeSink for Rec {
        fn lte_file(&self, bytes: Option<&[u8]>) {
            self.0.lock().unwrap().push(bytes.map(<[u8]>::to_vec));
        }
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-lte-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// modem.test.ts:43 "parses the daemon's telemetry JSON".
    #[tokio::test]
    async fn parses_the_daemons_telemetry_json() {
        let d = dir("ok");
        let f = d.join("telemetry.json");
        std::fs::write(
            &f,
            r#"{"timestamp":"2026-06-13T08:21:45Z","csq":18,"rssi_dbm":-77,"csq_interval":30,"status":"connected","iface":"usb0","ip":"10.164.20.3"}"#,
        )
        .unwrap();
        let rec = Rec::default();
        let s = read(&f, &rec).await;
        let t = s.typed.expect("telemetry");
        assert_eq!(t.csq, Some(18.0));
        assert_eq!(t.rssi_dbm, Some(-77.0));
        assert_eq!(t.status.as_deref(), Some("connected"));
        assert!(s.raw.is_some());
        assert_eq!(rec.0.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// modem.test.ts:52 "returns null when the file is absent".
    #[tokio::test]
    async fn returns_none_when_absent() {
        let rec = Rec::default();
        let s = read(
            Path::new("/nonexistent/xorgate-no-such-telemetry.json"),
            &rec,
        )
        .await;
        assert_eq!(s, ModemSnapshot::default());
        assert_eq!(*rec.0.lock().unwrap(), vec![None]);
    }

    /// modem.test.ts:57 "returns null on unparseable JSON".
    #[tokio::test]
    async fn returns_none_on_unparseable_json() {
        let d = dir("bad");
        let f = d.join("telemetry.json");
        std::fs::write(&f, "{ not json").unwrap();
        assert_eq!(read(&f, &NullTape).await, ModemSnapshot::default());
        assert_eq!(read_blocking(&f, &NullTape), ModemSnapshot::default());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn env_default() {
        assert_eq!(
            lte_telemetry_file(None),
            PathBuf::from("/run/sim7600-lte/telemetry.json")
        );
        assert_eq!(
            lte_telemetry_file(Some("/tmp/t.json".into())),
            PathBuf::from("/tmp/t.json")
        );
    }
}
