//! `mockObdStream` parity (`hardware/obd.ts:469-558` @ 09da38d):
//! `fixtures/obd/mock-stream.json` holds every message the TS mock emitted
//! under vitest fake timers (4 configs, stopped at `stopAt`); the schedule of
//! [`MockObdStream`] must produce the same messages at the same virtual ms.
//! Messages compare as JSON values (the TS mock's literals put `ts` before
//! `fields`/`counters`; key order is irrelevant here — the collector never
//! re-serialises a sample or an audit line in arrival order).

use serde_json::Value;
use xorgate_core::js::to_json_string;
use xorgate_core::obd::ObdHelperConfig;
use xorgate_hal::obd_io::MockObdStream;

fn canon(v: &Value) -> Value {
    serde_json::from_str(&to_json_string(v)).unwrap()
}

#[test]
fn mock_stream_matches_ts() {
    let path = format!(
        "{}/../../fixtures/obd/mock-stream.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let f: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(f["jsCommit"], "09da38d");
    let t0 = f["t0"].as_f64().unwrap();
    let runs = f["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 4);
    for run in runs {
        let cfg: ObdHelperConfig = serde_json::from_value(run["cfg"].clone()).unwrap();
        let stop_at = run["stopAt"].as_u64().unwrap();
        let mut got: Vec<(u64, Value)> = vec![];
        let mut m = MockObdStream::new(&cfg);
        for msg in m.start(t0) {
            got.push((0, msg.to_json()));
        }
        while let Some(due) = m.next_due().filter(|d| *d <= stop_at) {
            let (at, msgs) = m.fire_next(t0 + due as f64);
            assert_eq!(at, due);
            got.extend(msgs.into_iter().map(|x| (at, x.to_json())));
        }
        let exp = run["msgs"].as_array().unwrap();
        assert_eq!(got.len(), exp.len(), "cfg {}", run["cfg"]);
        for (i, ((t, msg), e)) in got.iter().zip(exp).enumerate() {
            assert_eq!(*t, e["t"].as_u64().unwrap(), "msg {i}");
            assert_eq!(canon(msg), canon(&e["msg"]), "msg {i} at {t}");
        }
    }
}
