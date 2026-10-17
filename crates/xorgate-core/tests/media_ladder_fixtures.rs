//! Fixture-driven test for `xorgate_core::media_ladder`, from
//! `fixtures/media_ladder/ladder.json`: script traces of the REAL
//! `createMediaCollector` (`src/agent/collectors/media.ts:83-380`) with a
//! fake runtime and virtual timers (`phase-3/evidence/wp5-tools/
//! dump-media-ladder.mts`).
//!
//! This test replays each script through the pure ladder with the same
//! virtual scheduler the driver used (one queue ordered by deadline, then
//! creation order; a SIGINT'd fake master exits with code 0 `sigintExitMs`
//! later) and compares the whole trace — every log line (level + text),
//! spawn (channel + pipeline), signal and `read()` snapshot — exactly.

mod common;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Deserialize;
use serde_json::{json, Value};
use xorgate_core::kvs_manifest::{parse_manifest_text, parse_manifest_value};
use xorgate_core::media_ladder::*;
use xorgate_core::stream_guard::StreamGuard;

#[derive(Deserialize)]
struct Case {
    name: String,
    input: Value,
    expected: Vec<Value>,
}

/// The driver's world: guard table, pipeline files, global fallback.
struct World {
    guard: RefCell<Option<HashMap<String, String>>>,
    present: RefCell<BTreeSet<String>>,
    pipeline_dir: String,
    global: Option<String>,
}

impl SpawnEnv for World {
    fn guard(&self, stream_key: &str) -> StreamGuard {
        match &*self.guard.borrow() {
            None => StreamGuard::Ok,
            Some(m) => StreamGuard::parse(m.get(stream_key).map_or("ok", String::as_str)).unwrap(),
        }
    }
    fn resolve_pipeline(&self, stream_key: &str) -> Option<String> {
        if self.present.borrow().contains(stream_key) {
            Some(format!("{}/{stream_key}.gst", self.pipeline_dir))
        } else {
            self.global.clone()
        }
    }
}

enum Pending {
    Timer {
        channel: String,
        kind: TimerKind,
        id: u64,
    },
    /// A SIGINT'd fake master exits with code 0.
    SigintExit { channel: String },
}

struct Sim<'a> {
    ladder: MediaLadder,
    world: &'a World,
    now: u64,
    seq: u64,
    queue: BTreeMap<(u64, u64), Pending>,
    /// channel → fake state: (exiting, done)
    fakes: HashMap<String, (bool, bool)>,
    sigint_exit_ms: u64,
    trace: Vec<Value>,
}

impl Sim<'_> {
    fn push_at(&mut self, at: u64, p: Pending) {
        self.seq += 1;
        self.queue.insert((at, self.seq), p);
    }

    fn apply(&mut self, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::Log { level, line } => self.trace.push(json!({
                    "t": self.now, "k": "log", "level": level.as_str(), "line": line
                })),
                Effect::Spawn {
                    channel, pipeline, ..
                } => {
                    if let Some((_, done)) = self.fakes.get(&channel) {
                        assert!(*done, "double spawn {channel}");
                    }
                    self.fakes.insert(channel.clone(), (false, false));
                    self.trace.push(json!({
                        "t": self.now, "k": "spawn", "channel": channel, "pipeline": pipeline
                    }));
                }
                Effect::Signal { channel } => {
                    self.trace.push(json!({
                        "t": self.now, "k": "signal", "channel": channel, "sig": "SIGINT"
                    }));
                    let f = self
                        .fakes
                        .get_mut(&channel)
                        .expect("signal to a spawned master");
                    if !f.0 && !f.1 {
                        f.0 = true;
                        let at = self.now + self.sigint_exit_ms;
                        self.push_at(at, Pending::SigintExit { channel });
                    }
                }
                Effect::Arm {
                    channel,
                    kind,
                    id,
                    delay_ms,
                } => {
                    let at = self.now + delay_ms;
                    self.push_at(at, Pending::Timer { channel, kind, id });
                }
            }
        }
    }

    fn exit(&mut self, channel: &str, code: Option<i32>) {
        let f = self.fakes.get_mut(channel).unwrap();
        assert!(!f.1, "{channel} exited twice");
        f.1 = true;
        let fx = self.ladder.on_exit(channel, code, self.now);
        self.apply(fx);
    }

    fn advance_to(&mut self, t: u64) {
        while let Some((&(at, seq), _)) = self.queue.iter().next() {
            if at > t {
                break;
            }
            let p = self.queue.remove(&(at, seq)).unwrap();
            self.now = at;
            match p {
                Pending::Timer { channel, kind, id } => {
                    let fx = self
                        .ladder
                        .on_timer(&channel, kind, id, self.now, self.world);
                    self.apply(fx);
                }
                Pending::SigintExit { channel } => self.exit(&channel, Some(0)),
            }
        }
        self.now = t;
    }
}

#[test]
fn media_ts_create_media_collector_script_traces() {
    let f = common::fixture::<Case>("media_ladder/ladder.json");
    assert!(f.cases.len() >= 14);
    for c in &f.cases {
        let inp = &c.input;
        let world = World {
            guard: RefCell::new(inp["guard"].as_object().map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                    .collect()
            })),
            present: RefCell::new(
                inp["pipelines"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect(),
            ),
            pipeline_dir: inp["pipelineDir"].as_str().unwrap().to_string(),
            global: inp["globalPipeline"].as_str().map(str::to_string),
        };
        let detect = RuntimeDetect {
            available: inp["detect"]["available"].as_bool().unwrap(),
            gstreamer: inp["detect"]["gstreamer"].as_bool().unwrap(),
            reason: inp["detect"]["reason"].as_str().map(str::to_string),
        };
        let manifest = match &inp["manifest"] {
            Value::Null => None,
            Value::String(text) => parse_manifest_text(text),
            v => parse_manifest_value(v),
        };
        let mut sim = Sim {
            ladder: MediaLadder::new(LadderOpts {
                lib_dir: inp["libDir"].as_str().unwrap().into(),
                manifest_path: inp["manifestPath"].as_str().unwrap().into(),
                source: inp["source"].as_str().unwrap().into(),
            }),
            world: &world,
            now: 0,
            seq: 0,
            queue: BTreeMap::new(),
            fakes: HashMap::new(),
            sigint_exit_ms: inp["sigintExitMs"].as_u64().unwrap(),
            trace: Vec::new(),
        };
        for step in inp["steps"].as_array().unwrap() {
            sim.advance_to(step["at"].as_u64().unwrap());
            let now = sim.now;
            match step["op"].as_str().unwrap() {
                "read" => {
                    let r = sim.ladder.read();
                    sim.trace
                        .push(json!({"t": now, "k": "read", "media": r.media}));
                    if r.kick_start {
                        let m = manifest.clone();
                        let fx = sim.ladder.start(&detect, move || m, now, &world);
                        sim.apply(fx);
                    }
                }
                "exit" => {
                    let code = step["code"].as_i64().map(|c| c as i32);
                    sim.exit(step["channel"].as_str().unwrap(), code);
                }
                "shed" => {
                    let fx = sim
                        .ladder
                        .set_shed(step["on"].as_bool().unwrap(), now, &world);
                    sim.apply(fx);
                }
                "disabled" => {
                    let keys: Vec<String> = step["keys"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect();
                    let fx = sim.ladder.set_disabled_streams(&keys, now, &world);
                    sim.apply(fx);
                }
                "restart" => {
                    let fx = sim.ladder.restart_channel(step["key"].as_str().unwrap());
                    sim.apply(fx);
                }
                "stop" => {
                    let fx = sim.ladder.stop();
                    sim.apply(fx);
                }
                "guard" => {
                    world.guard.borrow_mut().as_mut().unwrap().insert(
                        step["key"].as_str().unwrap().into(),
                        step["verdict"].as_str().unwrap().into(),
                    );
                }
                "pipeline" => {
                    let k = step["key"].as_str().unwrap().to_string();
                    if step["present"].as_bool().unwrap() {
                        world.present.borrow_mut().insert(k);
                    } else {
                        world.present.borrow_mut().remove(&k);
                    }
                }
                other => panic!("unknown op {other}"),
            }
        }
        sim.advance_to(inp["endAt"].as_u64().unwrap());
        assert_eq!(
            sim.trace.len(),
            c.expected.len(),
            "{}: trace length\n got: {:#?}",
            c.name,
            sim.trace
        );
        for (i, (got, want)) in sim.trace.iter().zip(&c.expected).enumerate() {
            assert_eq!(got, want, "{}: event #{i}", c.name);
        }
    }
}
