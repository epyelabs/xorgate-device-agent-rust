//! Phase 7c gate row C4: the Rust BNO085 client (`bno08x_client`) against the
//! Python ORACLE, on all 29 differential scenario rows (plan §WP2), plus the
//! two BNO086 rows of agent 0.2.2 (30: SHTP error lists on channel 0 and a
//! reset on channel 1 skipped; 31: a hub that checks the host's write
//! numbering).
//!
//! Re-baselined for agent 0.2.2 (the helper's SHTP fix, sha256 `8852852b…`):
//! regenerated with the same oracle image (`xorgate-p7c-oracle:a5d68f3db3f9`)
//! after a control run of the old helper reproduced the old fixtures byte for
//! byte. Against the old fixtures the 72 transcripts differ only in the meta
//! (`helper_sha256`, three titles), the sequence byte (offset 3) of host
//! writes, and the `ready` line's added identity, except row 15 (the sensor's
//! own reset: its channel 0/1 packets are no longer parsed, so the
//! `KeyError(0)` / `Unprocessable` loop errors and their D80 dumps are gone).
//!
//! `fixtures/imu_oracle/c4/*.imutx` were written by the oracle
//! (`tools/imu/oracle/oracle.py scenario all`): the UNMODIFIED
//! `python/bno085_read.py` on the bench's real stack (python3 3.11.2,
//! Blinka 9.1.0, PlatformDetect 3.88.0, PureIO 1.1.11, busdevice 5.2.17,
//! bno08x 1.3.3), faked only below PureIO, driven by a scripted SHTP sensor
//! and a virtual clock. Each transcript holds the op stream with the answers
//! the bus gave, every `time.monotonic()`/`time.sleep()` call with its exact
//! double, the helper's stdout in emission order and its exit.
//!
//! The client runs in lockstep with each one (`imu_lockstep`): the same op
//! sequence (ioctl/read/write bytes and lengths), the same clock calls, stdout
//! byte-equal at the same positions (D80 dumps filtered) and the same exit
//! code or signal. All inputs are synthetic.

mod common;

use std::collections::BTreeSet;

use serde_json::Value;
use sha2::{Digest, Sha256};
use xorgate_core::bno08x_client::ReaderArgs;
use xorgate_core::imu_lockstep::{self, ErrnoTable};
use xorgate_core::py_fmt::BUS_MISSING_BCM2712_PROVISIONAL;

const HELPER_SHA256: &str = "8852852bf754d19e084546e581ce40b0406e360734240eda1b670ac9f906e3cd";

fn errno() -> ErrnoTable {
    let v: Value = serde_json::from_str(&common::fixture_text("imu_oracle/errno.json")).unwrap();
    ErrnoTable::from_json(&v).unwrap()
}

fn transcripts() -> Vec<(String, Vec<Value>)> {
    let dir = common::fixture_path("imu_oracle/c4");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{dir}: {e}"))
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".imutx"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let text = common::fixture_text(&format!("imu_oracle/c4/{n}"));
            let recs = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            (n, recs)
        })
        .collect()
}

#[test]
fn c4_the_rust_client_equals_the_python_oracle_on_every_scenario() {
    let errno = errno();
    let all = transcripts();
    let summary: Value =
        serde_json::from_str(&common::fixture_text("imu_oracle/c4/summary.json")).unwrap();
    assert_eq!(
        all.len(),
        summary.as_array().unwrap().len(),
        "one transcript per oracle run"
    );
    let mut rows = BTreeSet::new();
    let mut failures = Vec::new();
    for (name, recs) in &all {
        let meta = &recs[0];
        assert_eq!(meta["op"], "meta", "{name}");
        assert_eq!(meta["clock"], "virtual", "{name}: a model transcript");
        assert_eq!(
            meta["helper_sha256"], HELPER_SHA256,
            "{name}: the vendored helper"
        );
        assert_eq!(meta["synthetic"], true, "{name}");
        rows.insert(meta["row"].as_u64().unwrap());
        let run = imu_lockstep::run(recs, &errno, &ReaderArgs::default());
        let problems = imu_lockstep::verdict(recs, &run);
        eprintln!(
            "C4 {name:<34} row {:>2} {} (records {}, stdout {} B, exit {:?}/{:?}{})",
            meta["row"],
            if problems.is_empty() { "PASS" } else { "FAIL" },
            run.consumed,
            run.stdout().len(),
            run.exit.map(|e| e.code()),
            run.killed,
            if run.teardown_close {
                ", teardown close"
            } else if run.python_only_close {
                ", python-only close (D83a)"
            } else {
                ""
            }
        );
        if !problems.is_empty() {
            failures.push(format!("{name}: {}", problems.join("; ")));
        }
    }
    assert_eq!(
        rows,
        (1..=31).collect::<BTreeSet<u64>>(),
        "all 29 plan rows and the two BNO086 rows covered"
    );
    assert!(failures.is_empty(), "C4 FAIL:\n{}", failures.join("\n"));
}

/// The vendored helper is the one the fixtures were generated from.
#[test]
fn c4_fixtures_name_the_vendored_helper() {
    let helper = std::fs::read(format!(
        "{}/../../python/bno085_read.py",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert_eq!(hex::encode(Sha256::digest(&helper)), HELPER_SHA256);
}

/// Row 22 makes WP1's PROVISIONAL bus-missing constant final: the real
/// Blinka 9.1.0 on a forced BCM2712 printed exactly it on ENOENT.
#[test]
fn c4_row_22_the_bus_missing_constant_is_what_blinka_prints() {
    let (_, recs) = transcripts()
        .into_iter()
        .find(|(n, _)| n.starts_with("s22a_"))
        .unwrap();
    let line = recs
        .iter()
        .find(|r| r["op"] == "stdout")
        .and_then(|r| r["line"].as_str())
        .unwrap();
    let v: Value = serde_json::from_str(line).unwrap();
    assert_eq!(
        v["error"].as_str().unwrap(),
        format!("init failed at 0x4a: {BUS_MISSING_BCM2712_PROVISIONAL}")
    );
}

/// The harness is not vacuous: a transcript changed in one written byte, one
/// stdout byte, one clock double or its exit code FAILS.
#[test]
fn c4_negative_controls_fail() {
    let errno = errno();
    let (_, base) = transcripts()
        .into_iter()
        .find(|(n, _)| n.starts_with("s01_"))
        .unwrap();
    let args = ReaderArgs::default();
    assert!(imu_lockstep::verdict(&base, &imu_lockstep::run(&base, &errno, &args)).is_empty());

    let tamper = |f: &dyn Fn(&mut Vec<Value>)| {
        let mut t = base.clone();
        f(&mut t);
        imu_lockstep::verdict(&t, &imu_lockstep::run(&t, &errno, &args))
    };
    // a set-feature byte the client must write
    let p = tamper(&|t| {
        let w = t
            .iter_mut()
            .find(|r| r["op"] == "write" && r["len"] == 21)
            .unwrap();
        let h = w["hex"].as_str().unwrap().replacen("50c3", "51c3", 1);
        w["hex"] = Value::from(h);
    });
    assert!(p.iter().any(|m| m.contains("op mismatch")), "{p:?}");
    // a stdout byte
    let p = tamper(&|t| {
        let s = t.iter_mut().rev().find(|r| r["op"] == "stdout").unwrap();
        let l = s["line"].as_str().unwrap().replacen('0', "1", 1);
        s["line"] = Value::from(l);
    });
    assert!(p.iter().any(|m| m.contains("stdout differs")), "{p:?}");
    // the double the enable budget starts from (the client must use it)
    let p = tamper(&|t| {
        let w = t
            .iter()
            .position(|r| r["op"] == "write" && r["len"] == 21)
            .unwrap();
        let c = t[w..]
            .iter_mut()
            .find(|r| r["op"] == "clock" && r["fn"] == "monotonic")
            .unwrap();
        c["bits"] = Value::from((-100.0f64).to_bits());
    });
    assert!(p.iter().any(|m| m.contains("op mismatch")), "{p:?}");
    // the exit code
    let p = tamper(&|t| {
        let e = t.iter_mut().rev().find(|r| r["op"] == "exit").unwrap();
        e["code"] = Value::from(3);
    });
    assert!(p.iter().any(|m| m.starts_with("exit:")), "{p:?}");
}

/// review-wp2 F6/F2 in the Rust runner (next-op mode, as C5/C10b run it): the
/// exit record is MANDATORY; a capture without stdout records, or with a
/// capture defect in its meta counts, FAILS.
#[test]
fn c5_runner_refuses_incomplete_captures() {
    let errno = errno();
    let (_, recs) = transcripts()
        .into_iter()
        .find(|(n, _)| n.starts_with("s10_"))
        .unwrap();
    // what strace would show: no clock records, meta clock monotonic
    let base: Vec<Value> = recs
        .into_iter()
        .filter(|r| r["op"] != "clock")
        .map(|mut r| {
            if r["op"] == "meta" {
                r["clock"] = Value::from("monotonic");
            }
            r
        })
        .collect();
    let args = ReaderArgs::default();
    let check = |t: &Vec<Value>| imu_lockstep::verdict(t, &imu_lockstep::run(t, &errno, &args));
    assert!(check(&base).is_empty(), "control: {:?}", check(&base));

    let no_exit: Vec<Value> = base.iter().filter(|r| r["op"] != "exit").cloned().collect();
    let p = check(&no_exit);
    assert!(p.iter().any(|m| m.contains("no exit record")), "{p:?}");

    let no_stdout: Vec<Value> = base
        .iter()
        .filter(|r| r["op"] != "stdout")
        .cloned()
        .collect();
    let p = check(&no_stdout);
    assert!(p.iter().any(|m| m.contains("no stdout records")), "{p:?}");

    for key in imu_lockstep::CAPTURE_DEFECTS {
        let mut t = base.clone();
        t[0]["counts"] = serde_json::json!({ *key: 1 });
        let p = check(&t);
        assert!(
            p.iter().any(|m| m.contains("capture defect")),
            "{key}: {p:?}"
        );
    }
}
