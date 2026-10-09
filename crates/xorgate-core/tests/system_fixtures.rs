//! Fixture-driven tests for `xorgate_core::system`, extracted from the TS
//! agent's vitest suites (`fixtures/system/*.json`, each tagged with the JS
//! source file + commit it came from).

use std::collections::BTreeMap;

use serde::Deserialize;
use xorgate_core::system::*;
use xorgate_core::telemetry::MetricGroup;

fn fixture<T: for<'de> Deserialize<'de>>(name: &str) -> T {
    let path = format!(
        "{}/../../fixtures/system/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[derive(Deserialize)]
struct Tagged<C> {
    #[serde(rename = "jsSourceFile")]
    js_source_file: String,
    #[serde(rename = "jsCommit")]
    js_commit: String,
    cases: Vec<C>,
}

fn check_tag<C>(f: &Tagged<C>) {
    assert!(f.js_source_file.ends_with(".ts") || f.js_source_file.contains(".ts"));
    assert_eq!(f.js_commit.len(), 40, "jsCommit must be a full sha");
}

#[test]
fn mem_available() {
    #[derive(Deserialize)]
    struct Expected {
        #[serde(rename = "totalBytes")]
        total_bytes: f64,
        #[serde(rename = "availableBytes")]
        available_bytes: f64,
    }
    #[derive(Deserialize)]
    struct Case {
        name: String,
        input: String,
        expected: Option<Expected>,
    }
    let f: Tagged<Case> = fixture("mem_available.json");
    check_tag(&f);
    for c in &f.cases {
        let got = parse_mem_available(&c.input);
        match &c.expected {
            None => assert!(got.is_none(), "{}", c.name),
            Some(e) => {
                let g = got.unwrap_or_else(|| panic!("{}: expected Some", c.name));
                assert_eq!(g.total_bytes, e.total_bytes, "{}", c.name);
                assert_eq!(g.available_bytes, e.available_bytes, "{}", c.name);
            }
        }
    }
}

#[test]
fn cpu_load() {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Times {
        user: f64,
        nice: f64,
        sys: f64,
        idle: f64,
        irq: f64,
    }
    #[derive(Deserialize)]
    struct Expected {
        overall: f64,
        #[serde(rename = "perCore")]
        per_core: Vec<f64>,
    }
    #[derive(Deserialize)]
    struct Case {
        name: String,
        prev: Vec<Times>,
        curr: Vec<Times>,
        expected: Expected,
    }
    let f: Tagged<Case> = fixture("cpu_load.json");
    check_tag(&f);
    let conv = |v: &[Times]| -> CpuSample {
        v.iter()
            .map(|t| CpuTimes {
                user: t.user,
                nice: t.nice,
                sys: t.sys,
                idle: t.idle,
                irq: t.irq,
            })
            .collect()
    };
    for c in &f.cases {
        let got = compute_cpu_load(&conv(&c.prev), &conv(&c.curr));
        assert_eq!(got.overall, c.expected.overall, "{}", c.name);
        assert_eq!(got.per_core, c.expected.per_core, "{}", c.name);
    }
}

#[test]
fn df() {
    #[derive(Deserialize)]
    struct Expected {
        #[serde(rename = "totalBytes")]
        total_bytes: f64,
        #[serde(rename = "usedBytes")]
        used_bytes: f64,
        #[serde(rename = "usedPct")]
        used_pct: f64,
    }
    #[derive(Deserialize)]
    struct Case {
        name: String,
        input: String,
        expected: Option<Expected>,
    }
    let f: Tagged<Case> = fixture("df.json");
    check_tag(&f);
    for c in &f.cases {
        let got = parse_df(&c.input);
        match &c.expected {
            None => assert!(got.is_none(), "{}", c.name),
            Some(e) => {
                let g = got.unwrap_or_else(|| panic!("{}: expected Some", c.name));
                assert_eq!(g.total_bytes, e.total_bytes, "{}", c.name);
                assert_eq!(g.used_bytes, e.used_bytes, "{}", c.name);
                assert_eq!(g.used_pct, e.used_pct, "{}", c.name);
            }
        }
    }
}

#[test]
fn throttled() {
    #[derive(Deserialize)]
    struct Expected {
        current: u32,
        sticky: u32,
    }
    #[derive(Deserialize)]
    struct Case {
        name: String,
        input: String,
        expected: Option<Expected>,
    }
    let f: Tagged<Case> = fixture("throttled.json");
    check_tag(&f);
    for c in &f.cases {
        let got = parse_throttled(&c.input);
        match &c.expected {
            None => assert!(got.is_none(), "{}", c.name),
            Some(e) => {
                let g = got.unwrap_or_else(|| panic!("{}: expected Some", c.name));
                assert_eq!((g.current, g.sticky), (e.current, e.sticky), "{}", c.name);
            }
        }
    }
}

#[test]
fn pmic_adc() {
    #[derive(Deserialize)]
    struct Rail {
        amps: Option<f64>,
        volts: Option<f64>,
    }
    #[derive(Deserialize)]
    struct Case {
        name: String,
        input: String,
        #[serde(rename = "expectedRails")]
        expected_rails: Option<BTreeMap<String, Rail>>,
        #[serde(rename = "expectedRailCount")]
        expected_rail_count: Option<usize>,
        #[serde(rename = "expectedRail5v")]
        expected_rail_5v: Option<f64>,
    }
    #[derive(Deserialize)]
    struct Rail5vCase {
        name: String,
        adc: BTreeMap<String, Rail>,
        expected: Option<f64>,
    }
    #[derive(Deserialize)]
    struct File {
        #[serde(flatten)]
        tagged: Tagged<Case>,
        dump: String,
        #[serde(rename = "rail5vCases")]
        rail5v_cases: Vec<Rail5vCase>,
    }
    let f: File = fixture("pmic_adc.json");
    check_tag(&f.tagged);
    for c in &f.tagged.cases {
        let input = if c.input == "@dump" {
            f.dump.as_str()
        } else {
            c.input.as_str()
        };
        let got = parse_pmic_adc(input);
        match &c.expected_rails {
            None => assert!(got.is_none(), "{}", c.name),
            Some(rails) => {
                let g = got.unwrap_or_else(|| panic!("{}: expected Some", c.name));
                for (name, rail) in rails {
                    let r = g
                        .get(name)
                        .unwrap_or_else(|| panic!("{}: missing rail {name}", c.name));
                    assert_eq!(r.amps, rail.amps, "{}: {name}.amps", c.name);
                    assert_eq!(r.volts, rail.volts, "{}: {name}.volts", c.name);
                }
                if let Some(n) = c.expected_rail_count {
                    assert_eq!(g.len(), n, "{}: rail count", c.name);
                }
                if let Some(v) = c.expected_rail_5v {
                    assert_eq!(rail_5v_from(&g), Some(v), "{}: rail5v", c.name);
                }
            }
        }
    }
    for c in &f.rail5v_cases {
        let adc: PmicAdc = c
            .adc
            .iter()
            .map(|(k, r)| {
                (
                    k.clone(),
                    PmicRail {
                        amps: r.amps,
                        volts: r.volts,
                    },
                )
            })
            .collect();
        assert_eq!(rail_5v_from(&adc), c.expected, "{}", c.name);
    }
}

#[test]
fn max_current() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        bytes: Vec<u8>,
        expected: Option<u32>,
    }
    let f: Tagged<Case> = fixture("max_current.json");
    check_tag(&f);
    for c in &f.cases {
        assert_eq!(
            max_current_ma_from_bytes(&c.bytes),
            c.expected,
            "{}",
            c.name
        );
    }
}

#[test]
fn collector_group() {
    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct Reading {
        cpu_overall: f64,
        ram_used_pct: Option<f64>,
        cpu_temp_c: Option<f64>,
        disk_used_pct: Option<f64>,
        throttled: Option<u32>,
        rail5v: Option<f64>,
        psu_max_current_ma: Option<u32>,
    }
    #[derive(Deserialize)]
    struct Case {
        name: String,
        reading: Reading,
        #[serde(rename = "expectedJson")]
        expected_json: String,
    }
    let f: Tagged<Case> = fixture("collector_group.json");
    check_tag(&f);
    for c in &f.cases {
        let r = &c.reading;
        let g: MetricGroup = system_group(&SystemReading {
            cpu_overall: r.cpu_overall,
            ram_used_pct: r.ram_used_pct,
            cpu_temp_c: r.cpu_temp_c,
            disk_used_pct: r.disk_used_pct,
            throttled: r.throttled,
            rail_5v: r.rail5v,
            psu_max_current_ma: r.psu_max_current_ma,
        });
        assert_eq!(
            serde_json::to_string(&g).unwrap(),
            c.expected_json,
            "{}",
            c.name
        );
    }
}
