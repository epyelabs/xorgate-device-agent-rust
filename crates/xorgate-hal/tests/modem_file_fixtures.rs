//! CF16 / CF24 (WP1 half): `readModemTelemetry`'s file cases through the HAL
//! reader. Every raw text of `fixtures/modem/telemetry-tolerance.json` (dumped
//! from the REAL `readModemTelemetry` / `readModemBias` over a temp
//! `LTE_TELEMETRY_FILE`) is written to a file and read ONCE with
//! `modem_file::read`; the one snapshot must yield both the TS "no telemetry"
//! verdict (`typed`) and `readModemBias` (`raw` → `modem_bias_from_telemetry`).

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::config_report::modem_bias_from_telemetry;
use xorgate_hal::io::NullTape;
use xorgate_hal::modem_file;

#[derive(Deserialize)]
struct Input {
    now: i64,
    raw: String,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    input: Input,
    expected: Value,
}

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "jsCommit")]
    js_commit: String,
    cases: Vec<Case>,
}

#[tokio::test]
async fn tolerance_fixture_through_the_file_reader() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/modem/telemetry-tolerance.json"
    );
    let f: Fixture = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(f.js_commit, "89b5c6407ef88e20f7168bd767d22a7be04ebcb6");
    let dir = std::env::temp_dir().join(format!("xg-lte-fx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("telemetry.json");
    for c in &f.cases {
        std::fs::write(&file, &c.input.raw).unwrap();
        let snap = modem_file::read(&file, &NullTape).await;
        assert_eq!(
            snap.typed.is_none(),
            c.expected["noTelemetry"].as_bool().unwrap(),
            "{}: noTelemetry",
            c.name
        );
        let bias = serde_json::to_value(modem_bias_from_telemetry(snap.raw.as_ref(), c.input.now))
            .unwrap();
        assert_eq!(
            bias, c.expected["readModemBias"],
            "{}: readModemBias",
            c.name
        );
        let raw_bias = snap.typed.as_ref().and_then(|t| t.gnss_ant_bias.clone());
        let want = &c.expected["gnssAntBiasRaw"];
        if want.get("$undefined").is_some() {
            assert!(raw_bias.is_none(), "{}: gnss_ant_bias", c.name);
        } else {
            assert_eq!(raw_bias.as_ref(), Some(want), "{}: gnss_ant_bias", c.name);
        }
    }
    assert!(f.cases.len() >= 17);
    let _ = std::fs::remove_dir_all(&dir);
}
