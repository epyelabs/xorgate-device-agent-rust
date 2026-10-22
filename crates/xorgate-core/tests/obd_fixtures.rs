//! Phase 5b WP2: `fixtures/obd/{pure,supervisor-traces}.json`, dumped from
//! the REAL TS @ 09da38d by `tools/replay/js/zz-p5b-dump-obd.test.ts`.
//!
//! - `pure.json`: every pure helper of `hardware/obd.ts`,
//!   `collectors/obd.ts`, `commands/obd.ts` and `obdStatusLine` on the
//!   `test/obd.test.ts` / `test/obd-cli.test.ts` vectors plus edge vectors.
//! - `supervisor-traces.json`: every `createObdCollector` it() of
//!   `test/obd.test.ts` (27) + 6 extra paths as a step script; each step's
//!   observation (virtual time, output, helper runs + SIGINTs, settings-read
//!   count, NEW events and console lines, `status()`, `report()`) is compared
//!   byte for byte (`JSON.stringify` form) against [`ObdSupervisor`] driven by
//!   a fake timer wheel that fires like `vi.advanceTimersByTime`.

use serde_json::{json, Value};
use xorgate_core::js::to_json_string;
use xorgate_core::obd::*;
use xorgate_core::obd_supervisor::*;
use xorgate_core::settings::ObdSettings;
use xorgate_core::telemetry::ObdStatus;

fn load(rel: &str) -> Value {
    let path = format!("{}/../../fixtures/obd/{rel}", env!("CARGO_MANIFEST_DIR"));
    let v: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")),
    )
    .unwrap_or_else(|e| panic!("{path}: {e}"));
    assert_eq!(v["jsCommit"], "09da38d", "{path}: jsCommit");
    assert!(v["jsSourceFile"].is_array(), "{path}: jsSourceFile");
    v
}

fn j(v: &Value) -> String {
    to_json_string(v)
}

fn jv<T: serde::Serialize>(v: &T) -> String {
    to_json_string(&serde_json::to_value(v).unwrap())
}

fn env_of(v: &Value) -> ObdEnv {
    serde_json::from_value(v.clone()).unwrap()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// pure
// ---------------------------------------------------------------------------

#[test]
fn parse_obd_line_matches_ts() {
    let p = load("pure.json");
    let cases = p["parse"].as_array().unwrap();
    assert!(cases.len() >= 40);
    for c in cases {
        let line = c["line"].as_str().unwrap();
        let got = parse_obd_line(line).map_or(Value::Null, |m| m.to_json());
        assert_eq!(j(&got), j(&c["out"]), "line {line:?}");
    }
}

#[test]
fn env_config_and_argv_match_ts() {
    let p = load("pure.json");
    for c in p["env"].as_array().unwrap() {
        let cfg = read_obd_env_config(&env_of(&c["env"]));
        assert_eq!(jv(&cfg), j(&c["cfg"]), "env {}", c["env"]);
        assert_eq!(cfg.is_configured(), c["configured"].as_bool().unwrap());
        assert_eq!(build_obd_helper_args(&cfg), strs(&c["argv"]));
    }
    for c in p["argv"].as_array().unwrap() {
        let cfg: ObdHelperConfig = serde_json::from_value(c["cfg"].clone()).unwrap();
        assert_eq!(build_obd_helper_args(&cfg), strs(&c["argv"]), "{}", c["cfg"]);
    }
}

#[test]
fn resolve_matches_ts() {
    let p = load("pure.json");
    for c in p["resolve"].as_array().unwrap() {
        let settings: Option<ObdSettings> = match &c["settings"] {
            Value::String(s) if s == "<undefined>" => None,
            v => Some(serde_json::from_value(v.clone()).unwrap()),
        };
        let r = resolve_obd_config(settings.as_ref(), &env_of(&c["env"]));
        assert_eq!(jv(&r), j(&c["out"]), "settings {} env {}", c["settings"], c["env"]);
    }
}

#[test]
fn normalize_scan_candidates_matches_ts() {
    let p = load("pure.json");
    for c in p["normalize"].as_array().unwrap() {
        let raw = match &c["raw"] {
            Value::String(s) if s == "<undefined>" => Value::Null,
            v => v.clone(),
        };
        assert_eq!(jv(&normalize_scan_candidates(&raw)), j(&c["out"]), "raw {raw}");
    }
}

fn snapshot(v: &Value) -> Option<AuditSnapshot> {
    if v.is_null() {
        return None;
    }
    let nums = |k: &str| {
        v[k].as_object()
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(AuditSnapshot {
        counters: nums("counters"),
        inventory: nums("inventory"),
        last_stop: v["last_stop"].as_object().map(|o| LastStop {
            ts: o.get("ts").and_then(Value::as_f64),
            reason: o.get("reason").and_then(Value::as_str).map(str::to_string),
        }),
        ts: v["ts"].as_f64().unwrap(),
    })
}

#[test]
fn collector_pure_helpers_match_ts() {
    let p = load("pure.json");
    for c in p["auditLines"].as_array().unwrap() {
        let a = snapshot(&c["audit"]).unwrap();
        let got = audit_line(c["seq"].as_u64().unwrap(), &a, c["rate"].as_f64());
        assert_eq!(got, c["out"].as_str().unwrap());
    }
    for c in p["bound"].as_array().unwrap() {
        let inv = snapshot(&json!({"inventory": c["inventory"], "ts": 0})).unwrap().inventory;
        let got = bound_inventory(&inv, c["max"].as_u64().unwrap() as usize);
        assert_eq!(jv(&got), j(&c["out"]), "{}", c["inventory"]);
    }
    for c in p["rate"].as_array().unwrap() {
        let got = request_rate(snapshot(&c["prev"]).as_ref(), snapshot(&c["cur"]).as_ref());
        assert_eq!(got, c["out"].as_f64(), "{} → {}", c["prev"], c["cur"]);
    }
    for c in p["reportDue"].as_array().unwrap() {
        let st = |v: &Value| -> Option<ObdStatus> {
            (!v.is_null()).then(|| serde_json::from_value(v.clone()).unwrap())
        };
        let got = obd_report_due(st(&c["prev"]).as_ref(), st(&c["next"]).as_ref());
        assert_eq!(got, c["out"].as_bool().unwrap(), "{} → {}", c["prev"], c["next"]);
    }
    for c in p["statusLine"].as_array().unwrap() {
        let st: ObdStatus = serde_json::from_value(c["status"].clone()).unwrap();
        assert_eq!(obd_status_line(&st), c["out"].as_str().unwrap());
    }
    for c in p["profiles"].as_array().unwrap() {
        let got = profile_summary(c["profile"].as_object());
        assert_eq!(got.as_deref(), c["out"].as_str(), "{}", c["profile"]);
    }
}

#[test]
fn obd_cli_helpers_match_ts() {
    let p = load("pure.json");
    for c in p["obdArgs"].as_array().unwrap() {
        let got = parse_obd_args(&strs(&c["args"]));
        assert_eq!(j(&got.to_json()), j(&c["out"]), "{}", c["args"]);
    }
    for c in p["tables"].as_array().unwrap() {
        let cands: Vec<ObdScanCandidate> = serde_json::from_value(c["candidates"].clone()).unwrap();
        assert_eq!(format_scan_table(&cands, "xorgate"), c["out"].as_str().unwrap());
    }
}

// ---------------------------------------------------------------------------
// supervisor traces
// ---------------------------------------------------------------------------

struct Run {
    handle: u64,
    cfg: ObdHelperConfig,
    stopped: Option<&'static str>,
}

struct Harness {
    sup: ObdSupervisor,
    t0: f64,
    t: u64,
    timers: Vec<(ObdTimer, u64, u64, u64)>,
    seq: u64,
    runs: Vec<Run>,
    events: Vec<Value>,
    logs: Vec<Value>,
    reads: u64,
    settings: Option<ObdSettings>,
    last_stamp: Option<u64>,
}

impl Harness {
    fn now(&self) -> ObdNow {
        ObdNow {
            wall_ms: self.t0 + self.t as f64,
            mono_ms: self.t,
        }
    }

    fn apply(&mut self, actions: Vec<ObdAction>) {
        for a in actions {
            match a {
                ObdAction::Spawn { handle, cfg } => self.runs.push(Run {
                    handle,
                    cfg,
                    stopped: None,
                }),
                ObdAction::Sigint { handle } => {
                    let r = self.runs.iter_mut().find(|r| r.handle == handle).unwrap();
                    r.stopped = Some("SIGINT");
                }
                ObdAction::Arm {
                    timer,
                    gen,
                    delay_ms,
                } => {
                    self.seq += 1;
                    self.timers.push((timer, gen, self.t + delay_ms, self.seq));
                }
                ObdAction::Disarm { timer } => self.timers.retain(|x| x.0 != timer),
                ObdAction::Log { level, line } => self
                    .logs
                    .push(json!({"level": level.as_str(), "line": line})),
                ObdAction::Event { level, msg } => self
                    .events
                    .push(json!({"level": level.as_str(), "msg": msg})),
            }
        }
    }

    fn read_settings(&mut self) -> Vec<ObdAction> {
        let s = self.settings.clone();
        let reads = &mut self.reads;
        let mut f = || {
            *reads += 1;
            Ok(s.clone())
        };
        let now = ObdNow {
            wall_ms: self.t0 + self.t as f64,
            mono_ms: self.t,
        };
        self.sup.tick(now, &mut f)
    }

    fn advance(&mut self, ms: u64) {
        let target = self.t + ms;
        loop {
            let next = self
                .timers
                .iter()
                .enumerate()
                .filter(|(_, x)| x.2 <= target)
                .min_by_key(|(_, x)| (x.2, x.3))
                .map(|(i, _)| i);
            let Some(i) = next else { break };
            let (timer, gen, due, _) = self.timers.remove(i);
            self.t = due;
            let a = self.sup.timer_fired(self.now(), timer, gen);
            self.apply(a);
        }
        self.t = target;
    }
}

fn settings_of(v: &Value) -> Option<ObdSettings> {
    (!v.is_null()).then(|| serde_json::from_value(v.clone()).unwrap())
}

#[test]
fn supervisor_traces_match_ts() {
    let f = load("supervisor-traces.json");
    let t0 = f["t0"].as_f64().unwrap();
    let cases = f["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 34); // 27 its (obd.test.ts:914 split in two) + 6 extra paths
    let mut steps_checked = 0;
    for c in cases {
        let meta = &c["case"];
        let name = meta["name"].as_str().unwrap();
        let mut h = Harness {
            sup: ObdSupervisor::new(
                env_of(&meta["env"]),
                ObdNow {
                    wall_ms: t0,
                    mono_ms: 0,
                },
            ),
            t0,
            t: 0,
            timers: vec![],
            seq: 0,
            runs: vec![],
            events: vec![],
            logs: vec![],
            reads: 0,
            settings: settings_of(&meta["settings"]),
            last_stamp: None,
        };
        for (si, obs) in c["steps"].as_array().unwrap().iter().enumerate() {
            let step = &obs["step"];
            let ev0 = h.events.len();
            let log0 = h.logs.len();
            let mut out: Option<Value> = None;
            match step["op"].as_str().unwrap() {
                "read" => {
                    let a = h.read_settings();
                    h.apply(a);
                    out = Some(serde_json::to_value(h.sup.map_latest(h.now())).unwrap());
                }
                "sample" => {
                    let a = h.read_settings();
                    h.apply(a);
                    let stamp = h.sup.stamp();
                    out = Some(json!({
                        "fields": serde_json::to_value(h.sup.map_latest(h.now())).unwrap(),
                        "fresh": h.last_stamp != Some(stamp),
                    }));
                    h.last_stamp = Some(stamp);
                }
                "feed" => {
                    let msg = parse_obd_line(step["line"].as_str().unwrap());
                    out = Some(Value::Bool(msg.is_some()));
                    if let Some(m) = msg {
                        let a = h.sup.on_message(h.now(), m);
                        h.apply(a);
                    }
                }
                "stderr" => {
                    let a = h.sup.on_stderr(step["line"].as_str().unwrap());
                    h.apply(a);
                }
                "exit" => {
                    let handle = h.runs[step["run"].as_u64().unwrap() as usize].handle;
                    let a = h.sup.on_exit(h.now(), handle, step["code"].as_i64());
                    h.apply(a);
                }
                "advance" => h.advance(step["ms"].as_u64().unwrap()),
                "set" => h.settings = settings_of(&step["settings"]),
                "reapply" => {
                    let s = h.settings.clone();
                    let reads = &mut h.reads;
                    let mut f = || {
                        *reads += 1;
                        Ok(s.clone())
                    };
                    let now = ObdNow {
                        wall_ms: h.t0 + h.t as f64,
                        mono_ms: h.t,
                    };
                    let a = h.sup.reapply(now, &mut f);
                    h.apply(a);
                }
                op @ ("scan" | "scanJoin") => {
                    let a = h.sup.scan_begin().expect("a fresh scan");
                    h.apply(a);
                    let joined = op == "scanJoin" && h.sup.scan_begin().is_none();
                    let sc = &meta["scanner"];
                    let res = match sc["error"].as_str() {
                        Some(e) => Err(e.to_string()),
                        None => Ok(serde_json::from_value(sc["candidates"].clone()).unwrap_or_default()),
                    };
                    let (r, a) = h.sup.scan_end(h.now(), res);
                    h.apply(a);
                    let r = serde_json::to_value(r).unwrap();
                    out = Some(if op == "scanJoin" {
                        json!({"joined": joined, "result": r})
                    } else {
                        r
                    });
                }
                "forget" => {
                    let (mac, a) = h.sup.forget(h.now());
                    h.apply(a);
                    if let (Some(m), Some(e)) = (&mac, meta["unbond"]["error"].as_str()) {
                        let a = h.sup.forget_unbond_failed(m, e);
                        h.apply(a);
                    }
                    out = Some(json!({ "mac": mac }));
                }
                "stop" => {
                    let a = h.sup.stop(h.now());
                    h.apply(a);
                }
                op => panic!("unknown op {op}"),
            }
            let ctx = format!("case {name:?} step {si} {step}");
            assert_eq!(h.t, obs["t"].as_u64().unwrap(), "{ctx}: t");
            if let Some(exp) = obs.get("out") {
                assert_eq!(j(out.as_ref().unwrap()), j(exp), "{ctx}: out");
            } else {
                assert!(out.is_none(), "{ctx}: unexpected out");
            }
            let runs: Vec<Value> = h
                .runs
                .iter()
                .map(|r| json!({"cfg": serde_json::to_value(&r.cfg).unwrap(), "stopped": r.stopped}))
                .collect();
            assert_eq!(j(&Value::Array(runs)), j(&obs["runs"]), "{ctx}: runs");
            assert_eq!(h.reads, obs["reads"].as_u64().unwrap(), "{ctx}: reads");
            assert_eq!(
                j(&Value::Array(h.events[ev0..].to_vec())),
                j(&obs["events"]),
                "{ctx}: events"
            );
            assert_eq!(
                j(&Value::Array(h.logs[log0..].to_vec())),
                j(&obs["logs"]),
                "{ctx}: logs"
            );
            assert_eq!(jv(&h.sup.status()), j(&obs["status"]), "{ctx}: status");
            assert_eq!(j(&h.sup.report()), j(&obs["report"]), "{ctx}: report");
            steps_checked += 1;
        }
    }
    assert!(steps_checked > 300, "{steps_checked} steps");
}

/// D37 candidate pin: the supervisor's intervals run on the monotonic clock.
/// A wall-clock step (NTP correcting a stale RTC by +1 h) changes neither
/// sample freshness, nor the settings poll, nor `ranMs` — the TS, measuring
/// them on `Date.now()`, would drop the sample, re-read settings and count a
/// quick exit as stable. `since` and `scan.ts` stay wall.
#[test]
fn d37_intervals_ignore_a_wall_clock_step() {
    let env = ObdEnv {
        adapter: Some("AA:BB:CC:DD:EE:FF".into()),
        ..Default::default()
    };
    let at = |wall: f64, mono: u64| ObdNow {
        wall_ms: wall,
        mono_ms: mono,
    };
    let mut sup = ObdSupervisor::new(env, at(1_000.0, 0));
    let mut reads = 0;
    let mut read = || {
        reads += 1;
        Ok(None)
    };
    let a = sup.tick(at(1_000.0, 0), &mut read);
    let handle = match &a[..] {
        [ObdAction::Log { .. }, ObdAction::Spawn { handle, .. }, ObdAction::Arm { .. }] => *handle,
        other => panic!("{other:?}"),
    };
    sup.on_message(
        at(1_000.0, 0),
        parse_obd_line(r#"{"type":"sample","fields":{"rpm":900,"link":2}}"#).unwrap(),
    );
    // The wall clock jumps +1 h, 100 ms of real time later.
    let stepped = at(3_601_000.0, 100);
    assert!(sup.map_latest(stepped).contains_key("rpm"), "sample still fresh");
    sup.tick(stepped, &mut read);
    assert_eq!(reads, 1, "no settings re-read: 100 ms on mono");
    // A quick exit (100 ms on mono) is a quick failure, not "after 3600 s".
    let a = sup.on_exit(stepped, handle, Some(2));
    assert!(
        a.iter().any(|x| matches!(x, ObdAction::Log { line, .. } if line.contains("retry 1/3 in 2000 ms"))),
        "{a:?}"
    );
    assert_eq!(sup.status().since, 3_601_000.0, "since is wall");
}

/// `NATIVE_CADENCE_HZ.obd = 1` (`telemetry-recorder.ts:30-31` @ 09da38d):
/// the recorder writes the `obd` group at most once a second.
#[test]
fn recorder_cadence_has_obd_at_1_hz() {
    let c = xorgate_core::recorder_gate::native_cadence_hz();
    assert_eq!(c.get("obd"), Some(&1.0));
    assert_eq!(
        c.keys().map(String::as_str).collect::<Vec<_>>(),
        ["imu", "gps", "lte", "system", "obd"]
    );
}
