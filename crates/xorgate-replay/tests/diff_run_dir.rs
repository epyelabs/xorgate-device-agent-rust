//! End to end: `xorgate-replay diff <run>` on a hand-built run dir (JS and
//! Rust captures + the tape the run names), checked through `diff.json`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn tel(ts: i64, seq: i64, gps: Value) -> String {
    json!({"v":1,"deviceId":"DEV","ts":ts,"seq":seq,"metrics":{"gps":gps}}).to_string()
}

fn agent(run: &Path, label: &str, kind: &str, dev: &str, ticks: &[(i64, Value)], kvs: bool) {
    let d = run.join(label);
    write(
        &d.join("meta.json"),
        &json!({"label":label,"kind":kind,"deviceId":dev}).to_string(),
    );
    let mut lines = String::new();
    for (i, (ts, g)) in ticks.iter().enumerate() {
        let payload = tel(*ts, i as i64 + 1, g.clone()).replace("DEV", dev);
        lines.push_str(
            &json!({"t":0,"w":ts,"topic":format!("xorgate/devices/{dev}/telemetry"),
                "retain":false,"qos":0,"payload":payload})
            .to_string(),
        );
        lines.push('\n');
    }
    write(&d.join("mqtt.ndjson"), &lines);
    let kvs_v = if kvs {
        json!({"present":true,"len":2,"b64":"e30=","text":"{}"})
    } else {
        json!({"present":false})
    };
    write(
        &d.join("final.json"),
        &json!({"w":1,"files":{"etc/kvs.json":kvs_v,"refresh-modem.flag":{"present":false}}})
            .to_string(),
    );
}

#[test]
fn diff_reads_the_tape_beside_the_run_and_final_json() {
    let run = std::env::temp_dir().join(format!("xr-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&run);
    // The tape: NMEA, 40 s silence, then a GGA chunk (the TS loses it).
    let meta = json!({"v":1,"k":"meta","deviceId":"TAPE","build":"x","startWall":0});
    let nmea = |t: u64, l: &str| json!({"t":t,"w":t,"k":"nmea","line":l}).to_string();
    let tape = [
        meta.to_string(),
        nmea(2_000, "$GPGGA,0,4807.0,N,01131.0,E,1,08,0.9,545.4,M,,,,*00"),
        nmea(
            42_000,
            "$GPGGA,1,4807.0,N,01131.0,E,1,08,0.9,545.4,M,,,,*00",
        ),
        nmea(42_008, "$GPRMC,1,A,4807.0,N,01131.0,E,0,0,010125,,,A*00"),
    ]
    .join("\n");
    write(&run.join("synth.ndjson"), &(tape + "\n"));
    write(
        &run.join("play.json"),
        &json!({"tape":"/rig/tapes/synth.ndjson","startWall":0,"endWall":100_000,
            "nmeaResumes":[42_000]})
        .to_string(),
    );
    // The player's ACTUAL release instants (R1): the input facts come from
    // these, not from tape t + startWall.
    let rel = [(2_000, 2_000), (42_000, 42_000), (42_008, 42_008)]
        .iter()
        .map(|(t, w)| json!({"w":w,"due":w,"t":t,"k":"nmea"}).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    write(&run.join("releases.ndjson"), &(rel + "\n"));
    let full = json!({"fix_quality":{"value":1},"satellites":{"value":8}});
    let lost = json!({"fix_quality":{"value":0}});
    agent(
        &run,
        "a",
        "js",
        "dev-a",
        &[
            (41_000, full.clone()),
            (42_500, lost),
            (43_500, full.clone()),
        ],
        true,
    );
    agent(
        &run,
        "b",
        "rust",
        "dev-b",
        &[
            (41_010, full.clone()),
            (42_510, full.clone()),
            (43_510, full),
        ],
        false,
    );
    let out = Command::new(env!("CARGO_BIN_EXE_xorgate-replay"))
        .arg("diff")
        .arg(&run)
        .arg("--tolerances")
        .arg(repo().join("tools/replay/tolerances.toml"))
        .output()
        .unwrap();
    // kvs.json written by one agent only → FAIL.
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rep: Value =
        serde_json::from_slice(&std::fs::read(run.join("diff.json")).unwrap()).unwrap();
    let ms = rep["mismatches"].as_array().unwrap();
    let tol = |stream: &str, path: &str| {
        ms.iter()
            .find(|m| m["stream"] == stream && m["path"] == path)
            .map(|m| m["tol"].as_str().unwrap().to_string())
    };
    assert_eq!(
        tol("file:etc/kvs.json", "(file)").as_deref(),
        Some("UNTOLERATED")
    );
    // D23 from the tape found beside the run: the JS tick after the resume.
    assert_eq!(
        tol("telemetry", "metrics.gps.fix_quality.value").as_deref(),
        Some("T7/D23")
    );
    assert_eq!(
        tol("telemetry", "metrics.gps.satellites").as_deref(),
        Some("T7/D23")
    );
    assert!(
        rep["inputs"]["tape"]
            .as_str()
            .unwrap()
            .ends_with("synth.ndjson"),
        "{}",
        rep["inputs"]
    );
    assert_eq!(rep["inputs"]["nmea reopens (T7/D23)"], "1");
    assert_eq!(rep["stats"]["file:refresh-modem.flag"]["absent_both"], 1);

    // R1: without releases.ndjson the run is not judgeable — the diff fails
    // loudly instead of estimating the input instants.
    std::fs::remove_file(run.join("releases.ndjson")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_xorgate-replay"))
        .arg("diff")
        .arg(&run)
        .arg("--tolerances")
        .arg(repo().join("tools/replay/tolerances.toml"))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("releases.ndjson"), "{err}");
    let _ = std::fs::remove_dir_all(&run);
}
