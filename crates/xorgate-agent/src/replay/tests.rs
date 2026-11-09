//! `test/replay.test.ts` @ 09da38d (8 cases, step by step) plus the 7e WP1
//! pins: P1 (racing callers), P3 (JS numbers in the seed), P5 (a lap restart
//! on a recording toggle), D70 (non-wire leaves), D71/D72 notes, error texts,
//! the uncaught-error routes, and the fixture pack in `fixtures/sim-replay/`.

use super::seed::*;
use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use xorgate_core::js::to_json_string;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Home(PathBuf);
impl Home {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "xg-replay-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn assets(&self) -> PathBuf {
        self.0.join("assets")
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn gps(lat: f64, speed: f64) -> Value {
    json!({"gps": {"lat": {"value": lat}, "lon": {"value": -79}, "speed": {"value": speed, "unit": "km/h"}, "fix_quality": {"value": 1}}, "system": {"cpu_usage": {"value": 10}}})
}

/// `trace(n, t0)`: `{t: t0 + i*1000, seq: i, metrics: gps(43 + i/1000, i)}`.
fn trace(n: usize, t0: f64) -> Vec<Value> {
    (0..n)
        .map(|i| json!({"t": t0 + i as f64 * 1000.0, "seq": i, "metrics": gps(43.0 + i as f64 / 1000.0, i as f64)}))
        .collect()
}

struct Sc {
    id: &'static str,
    lap_ms: f64,
    samples: Vec<Value>,
    start_ts: f64,
    fragments: usize,
}

/// `writePack`: a tiny asset pack in the real on-disk shape.
fn write_pack(assets: &Path, scs: &[Sc]) {
    std::fs::create_dir_all(assets).unwrap();
    let catalog = json!({
        "v": 1, "home": {"lat": 10.0, "lon": 20.0},
        "seedSet": scs.iter().map(|s| s.id).collect::<Vec<_>>(),
        "scenarios": scs.iter().map(|s| json!({"id": s.id, "title": s.id, "durationMs": s.lap_ms, "originalStartTs": s.start_ts})).collect::<Vec<_>>(),
    });
    std::fs::write(assets.join("catalog.json"), catalog.to_string()).unwrap();
    for s in scs {
        let dir = assets.join(s.id);
        std::fs::create_dir_all(dir.join("cam0/seg")).unwrap();
        std::fs::create_dir_all(dir.join("telemetry")).unwrap();
        std::fs::write(dir.join("cam0/lap.h264"), "h264").unwrap();
        for i in 0..s.fragments {
            let name = format!("{i:05}");
            std::fs::write(
                dir.join(format!("cam0/seg/{name}.mp4")),
                format!("mp4-{}-{i}", s.id),
            )
            .unwrap();
            let sc = json!({
                "v": 1, "deviceId": "src-device", "streamKey": "cam0", "sessionId": "src-session", "seq": i,
                "startTs": s.start_ts + i as f64 * 60_000.0, "startMono": 1000 + i, "durationMs": 60_000, "sizeBytes": 10,
                "timeSource": "ntp", "codec": "h264", "width": 1280, "height": 720, "fps": 15, "bitrateKbps": 1000,
                "anchor": if i == 0 { "boundary" } else { "fstat" }, "finalized": true,
            });
            std::fs::write(
                dir.join(format!("cam0/seg/{name}.json")),
                to_json_string(&sc),
            )
            .unwrap();
        }
        let body: String = s
            .samples
            .iter()
            .map(|x| format!("{}\n", to_json_string(x)))
            .collect();
        std::fs::write(
            dir.join("telemetry/trace.jsonl.gz"),
            xorgate_hal::gzip_finalize::gzip_bytes(body.as_bytes()),
        )
        .unwrap();
        let manifest = json!({
            "v": 1, "id": s.id, "title": s.id, "description": "", "durationMs": s.lap_ms,
            "originalStartTs": s.start_ts, "originalEndTs": s.start_ts + s.fragments as f64 * 60_000.0,
            "streams": [{"streamKey": "cam0", "lap": "cam0/lap.h264", "width": 1280, "height": 720, "fps": 15, "fragments": "cam0/seg/", "fragmentCount": s.fragments, "mediaDurationMs": s.lap_ms}],
            "telemetry": {"trace": "telemetry/trace.jsonl.gz", "rateHz": 1, "samples": s.samples.len(), "tMin": s.samples[0]["t"], "tMax": s.samples.last().unwrap()["t"]},
        });
        std::fs::write(dir.join("scenario.json"), to_json_string(&manifest)).unwrap();
    }
}

fn samples_of(v: &[Value]) -> Vec<TraceSample> {
    v.iter()
        .map(|o| {
            let mut d = BTreeSet::new();
            TraceSample {
                t: o["t"].as_f64().unwrap(),
                seq: o.get("seq").cloned(),
                raw_metrics: o["metrics"].clone(),
                metrics: metrics_of(&o["metrics"], &mut d),
            }
        })
        .collect()
}

fn leaf_json(m: &Metrics, g: &str, f: &str) -> Value {
    serde_json::to_value(&m[g][f]).unwrap()
}

// ------------------------------------------------------- replay sampler ----

/// "returns the nearest sample at or before the offset, and null before the first"
#[test]
fn ts_sampler_nearest_at_or_before_and_null_before_the_first() {
    let s = samples_of(&trace(5, 1700.0));
    let seq = |o: f64| sample_at(&s, o).map(|i| s[i].seq.clone().unwrap());
    assert_eq!(seq(0.0), None);
    assert_eq!(seq(1699.0), None);
    assert_eq!(seq(1700.0), Some(json!(0)));
    assert_eq!(seq(2699.0), Some(json!(0)));
    assert_eq!(seq(2700.0), Some(json!(1)));
    assert_eq!(seq(999_999.0), Some(json!(4)));
}

/// "parked metrics keep the position and zero the speed with its unit"
#[test]
fn ts_parked_metrics_keep_position_and_zero_speed_with_unit() {
    let m = samples_of(&[json!({"t": 0, "metrics": gps(43.5, 88.0)})])
        .remove(0)
        .metrics;
    let p = parked_metrics(&m);
    assert_eq!(leaf_json(&p, "gps", "lat"), json!({"value": 43.5}));
    assert_eq!(
        leaf_json(&p, "gps", "speed"),
        json!({"value": 0, "unit": "km/h"})
    );
    assert_eq!(
        serde_json::to_value(&p["system"]).unwrap(),
        json!({"cpu_usage": {"value": 10}})
    );
    // `speed?.unit ? {unit} : {}`: an empty unit is dropped, a missing speed is added.
    let m2 = samples_of(&[json!({"t": 0, "metrics": {"gps": {"lat": {"value": 1}, "speed": {"value": 3, "unit": ""}}}})]).remove(0).metrics;
    assert_eq!(
        leaf_json(&parked_metrics(&m2), "gps", "speed"),
        json!({"value": 0})
    );
    let m3 = samples_of(&[json!({"t": 0, "metrics": {"gps": {"lat": {"value": 1}}, "imu": {}}})])
        .remove(0)
        .metrics;
    let p3 = parked_metrics(&m3);
    assert_eq!(p3["gps"].keys().collect::<Vec<_>>(), ["lat", "speed"]);
    assert!(p3["imu"].is_empty());
}

/// "the capture head is the Phase 0 recipe with the scenario's framerate"
#[test]
fn ts_capture_head_is_the_phase0_recipe() {
    let h = replay_capture_head(Path::new("/a/lap.h264"), 15.0);
    assert_eq!(
        h.join(" "),
        "filesrc location=/a/lap.h264 ! h264parse ! video/x-h264,framerate=15/1 ! avdec_h264 ! identity sync=true ! videoconvert ! videorate"
    );
}

// --------------------------------------------------- ReplayPlayer lap clock ----

fn fake_scenario(samples: Vec<TraceSample>, lap_ms: f64) -> Arc<LoadedScenario> {
    Arc::new(LoadedScenario {
        manifest: serde_json::from_value(json!({"v": 1, "id": "x", "durationMs": lap_ms, "originalStartTs": 0, "originalEndTs": lap_ms,
            "streams": [{"streamKey": "cam0", "lap": "cam0/lap.h264", "fps": 15, "fragments": "cam0/seg/", "fragmentCount": 1, "mediaDurationMs": lap_ms}],
            "telemetry": {"trace": "t", "rateHz": 1}})).unwrap(),
        dir: PathBuf::from("/x"),
        samples,
        lap_ms,
        dropped_leaves: BTreeSet::new(),
    })
}

/// "reads nothing before the lap, the trace during it, parked after it; updatedMono is the sample's own time"
#[tokio::test(start_paused = true)]
async fn ts_player_lap_clock_and_updated_mono() {
    let adv = |ms: u64| tokio::time::advance(Duration::from_millis(ms));
    let mut p = ReplayPlayer::new(fake_scenario(samples_of(&trace(4, 0.0)), 4000.0));
    assert!(p.read_metrics().is_empty());
    assert!(p.sample_groups().is_empty());
    p.start_lap(1_700_000_000_000);
    let lap = Instant::now();
    assert_eq!(
        leaf_json(&p.read_metrics(), "gps", "speed"),
        json!({"value": 0, "unit": "km/h"})
    );
    adv(2500).await;
    assert_eq!(
        leaf_json(&p.read_metrics(), "gps", "speed"),
        json!({"value": 2, "unit": "km/h"})
    );
    let g = p.sample_groups();
    assert_eq!(g["gps"].updated_mono, lap + Duration::from_millis(2000)); // t=2000, NOT the read time
    assert_eq!(g["system"].updated_mono, lap + Duration::from_millis(2000));
    adv(300).await;
    assert_eq!(
        p.sample_groups()["gps"].updated_mono,
        lap + Duration::from_millis(2000)
    );
    adv(1500).await;
    assert!(p.lap_elapsed());
    p.end_lap(true);
    assert_eq!(p.phase(), ReplayPhase::Gap);
    assert_eq!(
        leaf_json(&p.read_metrics(), "gps", "lat"),
        json!({"value": 43.003})
    );
    assert_eq!(
        leaf_json(&p.read_metrics(), "gps", "speed"),
        json!({"value": 0, "unit": "km/h"})
    );
    let parked1 = p.sample_groups()["gps"].updated_mono;
    adv(1000).await;
    assert!(p.sample_groups()["gps"].updated_mono > parked1);
    p.end_lap(false);
    assert_eq!(p.phase(), ReplayPhase::Parked);
    p.start_lap(1_700_000_010_000);
    assert_eq!(p.state().lap_index, 2);
}

// ---------------------------------------------------- createReplaySource ----

#[derive(Default)]
struct FakeHost {
    cycles: Mutex<usize>,
    rotations: Mutex<Vec<String>>,
    fatals: Mutex<Vec<String>>,
    fail_rotate: std::sync::atomic::AtomicBool,
}
impl ReplayHost for FakeHost {
    fn cycle_video(&self) {
        *self.cycles.lock().unwrap() += 1;
    }
    fn rotate_telemetry_session(&self, why: &str) -> Result<(), String> {
        self.rotations.lock().unwrap().push(why.to_string());
        if self.fail_rotate.load(Ordering::SeqCst) {
            return Err("EIO: i/o error, write".into());
        }
        Ok(())
    }
    fn fatal(&self, msg: String) {
        self.fatals.lock().unwrap().push(msg);
    }
}

type Settings = Arc<Mutex<Option<SimulationSettings>>>;

fn sim(id: Option<&str>, loop_: Option<bool>, gap: Option<u32>) -> Option<SimulationSettings> {
    Some(SimulationSettings {
        scenario_id: id.map(str::to_string),
        loop_,
        gap_seconds: gap,
    })
}

struct Made {
    src: Arc<ReplaySource>,
    settings: Settings,
    events: Arc<Mutex<Vec<(ActivityLevel, String)>>>,
}

fn make(
    assets: &Path,
    default: Option<&str>,
    settings: Option<SimulationSettings>,
) -> Result<Made, String> {
    let s: Settings = Arc::new(Mutex::new(settings));
    let events: Arc<Mutex<Vec<(ActivityLevel, String)>>> = Arc::default();
    let (s2, e2) = (s.clone(), events.clone());
    let t0 = Instant::now();
    let src = ReplaySource::create(ReplayOptions {
        assets_dir: assets.to_path_buf(),
        default_scenario_id: default.map(str::to_string),
        settings: Arc::new(move || s2.lock().unwrap().clone()),
        media: None,
        on_event: Some(Arc::new(move |l, m: &str| {
            e2.lock().unwrap().push((l, m.to_string()))
        })),
        now: Arc::new(move || 1_800_000_000_000 + t0.elapsed().as_millis() as i64),
        tick_ms: 0,
    })?;
    Ok(Made {
        src,
        settings: s,
        events,
    })
}

fn two_pack(assets: &Path) {
    write_pack(
        assets,
        &[
            Sc {
                id: "city-a",
                lap_ms: 3000.0,
                samples: trace(3, 0.0),
                start_ts: 1_700_000_000_000.0,
                fragments: 2,
            },
            Sc {
                id: "hwy-b",
                lap_ms: 2000.0,
                samples: trace(2, -1000.0),
                start_ts: 1_700_500_000_000.0,
                fragments: 2,
            },
        ],
    );
}

fn cam(key: &str) -> CaptureCamera {
    CaptureCamera {
        stream_key: key.into(),
        camera_name: "replay".into(),
    }
}

/// "loads the settings scenario (falling back to the catalog's first) and drives laps from capture callbacks"
#[tokio::test(start_paused = true)]
async fn ts_source_drives_laps_from_capture_callbacks() {
    let home = Home::new("src");
    two_pack(&home.assets());
    let m = make(
        &home.assets(),
        Some("nope"),
        sim(Some("hwy-b"), None, Some(7)),
    )
    .unwrap();
    let (src, set) = (&m.src, &m.settings);
    let host = Arc::new(FakeHost::default());
    src.attach_host(host.clone());
    assert_eq!(src.scenario().manifest.id, "hwy-b");
    assert_eq!(
        src.source_head(&cam("cam0"))[1],
        format!(
            "location={}",
            home.assets().join("hwy-b/cam0/lap.h264").display()
        )
    );
    assert_eq!(src.stream_guard("cam0"), StreamGuard::Ok);
    assert_eq!(src.stream_guard("cam1"), StreamGuard::NoPipeline);
    assert!(src.read().await.unwrap().metrics.is_empty());
    assert!(src.read().await.unwrap().media.streams.is_empty());

    src.on_playing("cam1", 1); // not the clock stream
    assert_eq!(src.status().phase, ReplayPhase::Idle);
    src.on_playing("cam0", 1_800_000_000_000);
    let st = src.status();
    assert_eq!(
        (st.phase, st.lap_index, st.video_driven),
        (ReplayPhase::Lap, 1, true)
    );
    assert_eq!(*host.rotations.lock().unwrap(), ["replay lap 1"]);
    assert_eq!(
        leaf_json(&src.read().await.unwrap().metrics, "gps", "lat"),
        json!({"value": 43.001})
    );
    tokio::time::advance(Duration::from_millis(1500)).await;
    let want = samples_of(&[json!({"t": 0, "metrics": gps(43.001, 1.0)})])
        .remove(0)
        .metrics;
    assert_eq!(src.sample_groups().await["gps"].fields, want["gps"]);
    src.on_ended("cam0", true);
    assert_eq!(src.status().phase, ReplayPhase::Gap);
    assert_eq!(src.respawn_hold_ms("cam0"), Some(7000));
    assert_eq!(src.respawn_hold_ms("cam1"), Some(0));
    assert_eq!(
        leaf_json(&src.read().await.unwrap().metrics, "gps", "speed"),
        json!({"value": 0, "unit": "km/h"})
    );
    *set.lock().unwrap() = sim(Some("hwy-b"), Some(false), None);
    assert_eq!(src.respawn_hold_ms("cam0"), None);

    // Scenario switch via settings: ends the lap through the host, hold 0, next spawn gets the new lap.
    *set.lock().unwrap() = sim(Some("city-a"), None, None);
    src.reapply().unwrap();
    assert_eq!(*host.cycles.lock().unwrap(), 1);
    assert_eq!(src.respawn_hold_ms("cam0"), Some(0));
    assert!(src.source_head(&cam("cam0"))[1].contains("city-a"));
    src.on_playing("cam0", 1_800_000_010_000);
    let st = src.status();
    assert_eq!(
        (st.phase, st.lap_index, st.scenario_id.as_str()),
        (ReplayPhase::Lap, 2, "city-a")
    );
    assert_eq!(src.respawn_hold_ms("cam0"), Some(30_000)); // default gap, switch no longer pending
    src.reapply().unwrap(); // unchanged → no-op
    assert_eq!(*host.cycles.lock().unwrap(), 1);
    *set.lock().unwrap() = sim(Some("missing"), None, None);
    src.reapply().unwrap();
    assert_eq!(src.status().scenario_id, "city-a"); // unknown → warned, catalog first… which IS city-a
    src.stop();
    let ev = m.events.lock().unwrap();
    assert!(ev
        .iter()
        .any(|(_, e)| e.contains("scenario switched to city-a")));
    assert!(ev.iter().any(|(l, e)| *l == ActivityLevel::Warn
        && e == "replay: scenario \"missing\" is not in the catalog — using city-a"));
    // The first line, as the TS logs it (settings win over the bad default).
    assert_eq!(
        ev[0].1,
        "replay: scenario hwy-b loaded (2 samples, lap 2 s)"
    );
}

/// "runs the lap clock on its own timer when no capture child ever reports PLAYING"
#[tokio::test(start_paused = true)]
async fn ts_source_runs_the_lap_clock_on_its_own_timer() {
    let home = Home::new("timer");
    two_pack(&home.assets());
    let m = make(&home.assets(), Some("city-a"), sim(None, None, Some(2))).unwrap();
    let src = &m.src;
    let adv = |ms: u64| tokio::time::advance(Duration::from_millis(ms));
    src.tick();
    assert_eq!(src.status().phase, ReplayPhase::Idle);
    adv(46_000).await;
    src.tick();
    let st = src.status();
    assert_eq!(
        (st.phase, st.lap_index, st.video_driven),
        (ReplayPhase::Lap, 1, false)
    );
    adv(3000).await; // lapMs
    src.tick();
    assert_eq!(src.status().phase, ReplayPhase::Gap);
    adv(2000).await;
    src.tick();
    let st = src.status();
    assert_eq!((st.phase, st.lap_index), (ReplayPhase::Lap, 2));
    src.stop();
}

// -------------------------------------------------------- first-boot seeding ----

/// "plans one delta so the newest scenario ends 1 h before now and spacing is kept; ids are fresh"
#[test]
fn ts_seed_plans_one_delta() {
    let home = Home::new("plan");
    write_pack(
        &home.assets(),
        &[
            Sc {
                id: "old",
                lap_ms: 120_000.0,
                samples: trace(3, -2000.0),
                start_ts: 1_700_000_000_000.0,
                fragments: 2,
            },
            Sc {
                id: "new",
                lap_ms: 120_000.0,
                samples: trace(3, 0.0),
                start_ts: 1_700_900_000_000.0,
                fragments: 2,
            },
        ],
    );
    let cat = load_catalog(&home.assets()).unwrap();
    let scs: Vec<_> = cat
        .seed_set
        .iter()
        .map(|id| load_scenario(&home.assets(), id).unwrap())
        .collect();
    let now = 1_800_000_000_000.0;
    let mint = |ms: f64| format!("id-{}", xorgate_core::js::number_to_string(ms));
    let plan = plan_seed(&scs, now, 3_600_000.0, &mint);
    let newest = scs[1].manifest.original_end_ts;
    assert_eq!(plan.delta_ms, now - 3_600_000.0 - newest);
    assert_eq!(plan.sessions[1].end_ts, now - 3_600_000.0);
    assert_eq!(
        plan.sessions[0].start_ts - plan.sessions[1].start_ts,
        1_700_000_000_000.0 - 1_700_900_000_000.0
    );
    assert_eq!(
        plan.sessions[0].media_session_id,
        mint(1_700_000_000_000.0 + plan.delta_ms)
    );
    assert_eq!(
        plan.sessions[0].telemetry_session_id,
        mint(1_700_000_000_000.0 + plan.delta_ms - 2000.0)
    );
}

fn seed_opts<'a>(
    home: &Home,
    cat: &'a ReplayCatalog,
    load: &'a dyn Fn(&str) -> Result<LoadedScenario, String>,
    now: &'a dyn Fn() -> f64,
    mint: &'a dyn Fn(f64) -> String,
) -> SeedOptions<'a> {
    SeedOptions {
        catalog: cat,
        load_scenario: load,
        recordings_root: home.0.join("recordings"),
        home: home.0.clone(),
        device_id: "sim-device",
        boot_id: "boot-1",
        end_before_ms: None,
        now: Some(now),
        mint: Some(mint),
        on_event: None,
    }
}

fn gunzip(p: &Path) -> String {
    let mut s = String::new();
    flate2::read::MultiGzDecoder::new(&std::fs::read(p).unwrap()[..])
        .read_to_string(&mut s)
        .unwrap();
    s
}

/// "writes symlinked fragments, rewritten sidecars and telemetry segments once, then never again"
#[test]
fn ts_seed_writes_once_then_never_again() {
    let home = Home::new("seed");
    write_pack(
        &home.assets(),
        &[Sc {
            id: "city-a",
            lap_ms: 120_000.0,
            samples: trace(70, 0.0),
            start_ts: 1_700_000_000_000.0,
            fragments: 2,
        }],
    );
    let cat = load_catalog(&home.assets()).unwrap();
    let assets = home.assets();
    let load = move |id: &str| load_scenario(&assets, id);
    let now = || 1_800_000_000_000.0;
    // `0000${ms}`.slice(-36).padStart(36, "a")
    let mint = |ms: f64| {
        let s = format!("0000{}", xorgate_core::js::number_to_string(ms));
        let tail: String = s
            .chars()
            .rev()
            .take(36)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{}{tail}", "a".repeat(36 - tail.len()))
    };
    let opts = seed_opts(&home, &cat, &load, &now, &mint);
    let first = seed_once(&opts).unwrap();
    let SeedResult::Seeded(plan) = first else {
        panic!("{first:?}")
    };
    assert!(home.0.join(SEED_MARKER).exists());
    let s = &plan.sessions[0];
    let video_dir = home
        .0
        .join("recordings/video/cam0")
        .join(&s.media_session_id);
    let mut names: Vec<String> = std::fs::read_dir(&video_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["00000.json", "00000.mp4", "00001.json", "00001.mp4"]
    );
    assert!(std::fs::symlink_metadata(video_dir.join("00000.mp4"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_to_string(video_dir.join("00001.mp4")).unwrap(),
        "mp4-city-a-1"
    );
    let sc: Value =
        serde_json::from_slice(&std::fs::read(video_dir.join("00001.json")).unwrap()).unwrap();
    assert_eq!(sc["deviceId"], "sim-device");
    assert_eq!(sc["sessionId"], s.media_session_id.as_str());
    assert_eq!(sc["seq"], 1);
    assert_eq!(
        sc["startTs"].as_f64(),
        Some(1_700_000_060_000.0 + plan.delta_ms)
    );
    assert_eq!(
        (
            &sc["timeSource"],
            &sc["finalized"],
            &sc["anchor"],
            &sc["sizeBytes"]
        ),
        (&json!("ntp"), &json!(true), &json!("fstat"), &json!(10))
    );
    // `{...orig, …}`: the original key order is kept.
    assert_eq!(
        sc.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "v",
            "deviceId",
            "streamKey",
            "sessionId",
            "seq",
            "startTs",
            "startMono",
            "durationMs",
            "sizeBytes",
            "timeSource",
            "codec",
            "width",
            "height",
            "fps",
            "bitrateKbps",
            "anchor",
            "finalized"
        ]
    );
    assert!(!video_dir.join("session.json").exists());
    let tel_dir = home
        .0
        .join("recordings/telemetry")
        .join(&s.telemetry_session_id);
    let mut segs: Vec<String> = std::fs::read_dir(&tel_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    segs.sort();
    assert_eq!(segs.len(), 2); // 70 samples → 60 + 10
    assert_eq!(
        segs[0],
        format!(
            "seg-{}-00000.jsonl.gz",
            xorgate_core::js::number_to_string(s.start_ts)
        )
    );
    let text = gunzip(&tel_dir.join(&segs[0]));
    let lines: Vec<Value> = text
        .trim()
        .split('\n')
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        text.split('\n').next().unwrap(),
        format!(
            r#"{{"type":"header","v":1,"deviceId":"sim-device","sessionId":"{}","bootId":"boot-1","timeSource":"ntp","rateHz":1,"startedAt":{}}}"#,
            s.telemetry_session_id,
            xorgate_core::js::number_to_string(s.start_ts)
        )
    );
    assert_eq!(lines.len(), 61);
    assert_eq!(
        (lines[1]["ts"].as_f64(), &lines[1]["seq"]),
        (Some(s.start_ts), &json!(0))
    );
    assert_eq!(lines[60]["ts"].as_f64(), Some(s.start_ts + 59_000.0));
    assert_eq!(lines[60]["metrics"]["gps"]["lat"], json!({"value": 43.059}));
    // Byte shape of a sample line: `{ts, mono, seq, metrics}` with the raw metrics.
    assert!(text.split('\n').nth(1).unwrap().ends_with(&format!(
        r#""seq":0,"metrics":{}}}"#,
        to_json_string(&gps(43.0, 0.0))
    )));

    let second = seed_once(&opts).unwrap();
    assert_eq!(second, SeedResult::AlreadySeeded);
    assert_eq!(
        std::fs::read_dir(home.0.join("recordings/video/cam0"))
            .unwrap()
            .count(),
        1
    );
    // The marker, byte for byte in shape.
    let marker: Value =
        serde_json::from_slice(&std::fs::read(home.0.join(SEED_MARKER)).unwrap()).unwrap();
    assert_eq!(
        marker.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["v", "at", "deltaMs", "sessions"]
    );
    assert_eq!(
        marker["sessions"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "scenarioId",
            "mediaSessionId",
            "telemetrySessionId",
            "streamKey",
            "startTs",
            "endTs",
            "fragments",
            "samples"
        ]
    );
}

// ------------------------------------------------------------ 7e WP1 pins ----

/// An empty seed set (or every scenario missing) writes the empty marker,
/// warns per missing scenario, and seeds nothing.
#[test]
fn seed_with_every_scenario_missing_writes_the_empty_marker() {
    let home = Home::new("seed-empty");
    std::fs::create_dir_all(home.assets()).unwrap();
    std::fs::write(
        home.assets().join("catalog.json"),
        r#"{"v":1,"seedSet":["gone"],"scenarios":[{"id":"gone"}]}"#,
    )
    .unwrap();
    let cat = load_catalog(&home.assets()).unwrap();
    let assets = home.assets();
    let load = move |id: &str| load_scenario(&assets, id);
    let warns: Mutex<Vec<String>> = Mutex::default();
    let on = |_: ActivityLevel, m: &str| warns.lock().unwrap().push(m.to_string());
    let now = || 5.0;
    let mint = |_: f64| "x".to_string();
    let mut opts = seed_opts(&home, &cat, &load, &now, &mint);
    opts.on_event = Some(&on);
    assert_eq!(seed_once(&opts).unwrap(), SeedResult::EmptySeedSet);
    assert_eq!(
        std::fs::read_to_string(home.0.join(SEED_MARKER)).unwrap(),
        r#"{"v":1,"at":5,"sessions":[]}"#
    );
    assert_eq!(
        *warns.lock().unwrap(),
        [format!(
            "replay seed: skipping gone: scenario gone: {} missing",
            home.assets().join("gone/scenario.json").display()
        )]
    );
    assert!(!home.0.join("recordings").exists());
}

/// P3: the seed keeps JS numbers. A fractional pack timestamp is shifted and
/// printed like JS (no truncation); an absent `seq` is omitted, not `null`;
/// a missing sidecar `startTs` writes `null` (`NaN`).
#[test]
fn p3_seed_numbers_are_js_numbers() {
    let home = Home::new("p3");
    let mut samples = trace(2, 0.5);
    samples[1].as_object_mut().unwrap().remove("seq");
    write_pack(
        &home.assets(),
        &[Sc {
            id: "frac",
            lap_ms: 60_000.0,
            samples,
            start_ts: 1_700_000_000_000.25,
            fragments: 1,
        }],
    );
    let side = home.assets().join("frac/cam0/seg/00000.json");
    let mut v: Value = serde_json::from_slice(&std::fs::read(&side).unwrap()).unwrap();
    v.as_object_mut().unwrap().remove("startTs");
    std::fs::write(&side, to_json_string(&v)).unwrap();
    let cat = load_catalog(&home.assets()).unwrap();
    let assets = home.assets();
    let load = move |id: &str| load_scenario(&assets, id);
    let now = || 1_800_000_000_000.0;
    let mint = |ms: f64| format!("m{}", xorgate_core::js::number_to_string(ms));
    let opts = seed_opts(&home, &cat, &load, &now, &mint);
    let SeedResult::Seeded(plan) = seed_once(&opts).unwrap() else {
        panic!()
    };
    let s = &plan.sessions[0];
    // delta = Math.round(now − 1 h − originalEndTs) is integral; start keeps the .25.
    assert_eq!(s.start_ts.fract(), 0.25);
    assert_eq!(
        s.media_session_id,
        format!("m{}", xorgate_core::js::number_to_string(s.start_ts))
    );
    let tel = home
        .0
        .join("recordings/telemetry")
        .join(&s.telemetry_session_id);
    let seg = std::fs::read_dir(&tel)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(seg
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with(&format!(
            "seg-{}-",
            xorgate_core::js::number_to_string(s.start_ts + 0.5)
        )));
    let text = gunzip(&seg);
    let l2 = text.split('\n').nth(2).unwrap();
    assert!(!l2.contains("\"seq\""), "{l2}");
    assert!(l2.starts_with(&format!(
        "{{\"ts\":{},",
        xorgate_core::js::number_to_string(s.start_ts + 1000.5)
    )));
    let sc: Value = serde_json::from_slice(
        &std::fs::read(
            home.0
                .join("recordings/video/cam0")
                .join(&s.media_session_id)
                .join("00000.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(sc["startTs"], Value::Null);
    assert_eq!(sc["startMono"], Value::Null);
}

/// The seed never writes outside `recordings/` (plus the marker in home) and
/// a second fragment symlink onto an existing one fails with Node's text.
#[test]
fn seed_fs_errors_carry_nodes_message() {
    let home = Home::new("seed-err");
    write_pack(
        &home.assets(),
        &[Sc {
            id: "city-a",
            lap_ms: 60_000.0,
            samples: trace(2, 0.0),
            start_ts: 1_700_000_000_000.0,
            fragments: 1,
        }],
    );
    let cat = load_catalog(&home.assets()).unwrap();
    let scs = vec![load_scenario(&home.assets(), "city-a").unwrap()];
    let plan = plan_seed(&scs, 1_800_000_000_000.0, SEED_END_BEFORE_MS, &|_| {
        "fixed".to_string()
    });
    let root = home.0.join("recordings");
    let t = SeedTarget {
        recordings_root: &root,
        device_id: "d",
        boot_id: "b",
    };
    write_seed_session(&scs[0], &plan.sessions[0], &plan, &t).unwrap();
    let err = write_seed_session(&scs[0], &plan.sessions[0], &plan, &t).unwrap_err();
    let src = home.assets().join("city-a/cam0/seg/00000.mp4");
    let dst = root.join("video/cam0/fixed/00000.mp4");
    assert_eq!(
        err,
        format!(
            "EEXIST: file already exists, symlink '{}' -> '{}'",
            src.display(),
            dst.display()
        )
    );
    let top: Vec<String> = std::fs::read_dir(&home.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        top.iter().all(|n| n == "assets" || n == "recordings"),
        "{top:?}"
    );
    drop(cat);
}

/// D70: a leaf that is not `{value:number, unit?:string}` (or a group that
/// is not an object) is left out of the live view and the recorder, ONE warn
/// names the paths per scenario load, and seeding keeps the raw JSON.
#[tokio::test(start_paused = true)]
async fn d70_non_wire_leaves_are_left_out_of_the_live_view_with_one_warn() {
    let home = Home::new("d70");
    let odd = vec![
        json!({"t": 0, "seq": 0, "metrics": {"gps": {"lat": {"value": 1}, "fix": {"value": "3d"}, "hdop": {"value": 1, "q": 2}, "sat": {"value": 7, "unit": null}}, "x": 5, "system": {"cpu_usage": {"value": 10}}}}),
        json!({"t": 1000, "seq": 1, "metrics": {"gps": {"lat": {"value": 2}, "fix": {"value": true}}}}),
    ];
    write_pack(
        &home.assets(),
        &[Sc {
            id: "odd",
            lap_ms: 2000.0,
            samples: odd,
            start_ts: 1_700_000_000_000.0,
            fragments: 1,
        }],
    );
    let m = make(&home.assets(), None, None).unwrap();
    let sc = m.src.scenario();
    assert_eq!(
        sc.dropped_leaves
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["gps.fix", "gps.hdop", "gps.sat", "x"]
    );
    let warns: Vec<String> = m
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|(l, _)| *l == ActivityLevel::Warn)
        .map(|(_, e)| e.clone())
        .collect();
    assert_eq!(
        warns,
        ["replay: scenario odd: 4 trace path(s) are not {value:number, unit?:string} and are left out of the live view: gps.fix, gps.hdop, gps.sat, x"]
    );
    m.src.on_playing("cam0", 1);
    let live = m.src.read().await.unwrap().metrics;
    assert_eq!(
        serde_json::to_string(&live).unwrap(),
        r#"{"gps":{"lat":{"value":1}},"system":{"cpu_usage":{"value":10}}}"#
    );
    assert_eq!(
        m.src.sample_groups().await.keys().collect::<Vec<_>>(),
        ["gps", "system"]
    );
    // The raw view (seeding) is untouched.
    assert_eq!(
        sc.samples[0].raw_metrics["gps"]["fix"],
        json!({"value": "3d"})
    );
}

/// D70 pin on the shape classes the real pack uses (checked read-only over
/// all six scenarios, `phase-7e/evidence/wp1.md`): every leaf passes, and
/// the live payload is byte-identical to `JSON.stringify` of the raw trace
/// sample (the TS payload).
#[tokio::test(start_paused = true)]
async fn d70_fixture_pack_shapes_pass_through_byte_identical() {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sim-replay");
    let m = make(&assets, None, None).unwrap();
    let sc = m.src.scenario();
    assert!(sc.dropped_leaves.is_empty());
    assert!(m
        .events
        .lock()
        .unwrap()
        .iter()
        .all(|(l, _)| *l == ActivityLevel::Info));
    let groups: BTreeSet<&str> = sc
        .samples
        .iter()
        .flat_map(|s| s.metrics.keys().map(String::as_str))
        .collect();
    assert_eq!(
        groups,
        ["gps", "imu", "lte", "obd", "system"].into_iter().collect()
    );
    for s in &sc.samples {
        assert_eq!(
            serde_json::to_string(&s.metrics).unwrap(),
            to_json_string(&s.raw_metrics)
        );
    }
    m.src.on_playing(&sc.manifest.streams[0].stream_key, 1);
    let live = m.src.read().await.unwrap().metrics;
    let at0 = sample_at(&sc.samples, 0.0).unwrap(); // t=0 (t=-500 precedes the first frame)
    assert_eq!(
        serde_json::to_string(&live).unwrap(),
        to_json_string(&sc.samples[at0].raw_metrics)
    );
}

/// P1: the self-timer, the capture callbacks and the config path race on
/// many threads. No deadlock, no lost or doubled switch: every logged switch
/// cycled the video exactly once, and the final state is the settings'.
#[test]
fn p1_racing_reapply_tick_and_capture_callbacks_switch_once_each() {
    let home = Home::new("p1");
    two_pack(&home.assets());
    let m = make(&home.assets(), None, sim(Some("city-a"), None, Some(0))).unwrap();
    let host = Arc::new(FakeHost::default());
    m.src.attach_host(host.clone());
    m.src.on_playing("cam0", 1);
    std::thread::scope(|s| {
        for t in 0..4 {
            let (src, set) = (&m.src, &m.settings);
            s.spawn(move || {
                for i in 0..300 {
                    match (t + i) % 5 {
                        0 => {
                            *set.lock().unwrap() = sim(
                                Some(if i % 2 == 0 { "hwy-b" } else { "city-a" }),
                                None,
                                Some(0),
                            )
                        }
                        1 => src.reapply().unwrap(),
                        2 => src.tick(),
                        3 => src.on_ended("cam0", true),
                        _ => {
                            let _ = src.respawn_hold_ms("cam0");
                            src.on_playing("cam0", 2);
                            let _ = src.source_head(&cam("cam0"));
                        }
                    }
                }
            });
        }
    });
    m.src.reapply().unwrap();
    let want = m
        .settings
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .scenario_id
        .unwrap();
    let st = m.src.status();
    assert_eq!(st.scenario_id, want);
    assert_eq!(m.src.scenario().manifest.id, want);
    let switches = m
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| e.contains("scenario switched to"))
        .count();
    assert_eq!(*host.cycles.lock().unwrap(), switches);
    assert!(host.fatals.lock().unwrap().is_empty());
}

/// P5: recording toggled on mid-lap restarts the lap (the old child's EOS →
/// `on_ended`, the new child's PLAYING → the next lap and a telemetry rotation).
#[tokio::test(start_paused = true)]
async fn p5_a_capture_restart_mid_lap_starts_a_new_lap() {
    let home = Home::new("p5");
    two_pack(&home.assets());
    let m = make(&home.assets(), Some("city-a"), None).unwrap();
    let host = Arc::new(FakeHost::default());
    m.src.attach_host(host.clone());
    m.src.on_playing("cam0", 1);
    tokio::time::advance(Duration::from_millis(1200)).await;
    m.src.on_ended("cam0", true); // the bridge child, ended for the record child
    assert_eq!(m.src.status().phase, ReplayPhase::Gap);
    m.src.on_playing("cam0", 2);
    let st = m.src.status();
    assert_eq!(
        (st.phase, st.lap_index, st.offset_ms),
        (ReplayPhase::Lap, 2, 0.0)
    );
    assert_eq!(
        *host.rotations.lock().unwrap(),
        ["replay lap 1", "replay lap 2"]
    );
    // The next end closes lap 2; an end outside a lap is ignored.
    m.src.on_ended("cam0", true);
    m.src.on_ended("cam0", false);
    let ended = m
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| e.contains("lap ended"))
        .count();
    assert_eq!(ended, 2);
}

/// The TS throw out of `stopRecorder`: from the capture callback or the
/// timer it is uncaught (Node exits 1 → `fatal`); from `reapply` (the config
/// path, inside its try) it is returned.
#[tokio::test(start_paused = true)]
async fn rotation_errors_are_fatal_from_callbacks_and_returned_from_reapply() {
    let home = Home::new("fatal");
    two_pack(&home.assets());
    let m = make(&home.assets(), Some("city-a"), None).unwrap();
    let host = Arc::new(FakeHost::default());
    host.fail_rotate.store(true, Ordering::SeqCst);
    m.src.attach_host(host.clone());
    m.src.on_playing("cam0", 1);
    assert_eq!(*host.fatals.lock().unwrap(), ["EIO: i/o error, write"]);
    // Telemetry-only switch (no video-driven cycle): the lap restarts in
    // reapply, so the rotation error comes back to the caller.
    let m2 = make(&home.assets(), Some("city-a"), None).unwrap();
    let host2 = Arc::new(FakeHost::default());
    host2.fail_rotate.store(true, Ordering::SeqCst);
    m2.src.attach_host(host2.clone());
    *m2.settings.lock().unwrap() = sim(Some("hwy-b"), None, None);
    assert_eq!(m2.src.reapply(), Err("EIO: i/o error, write".to_string()));
    assert!(host2.fatals.lock().unwrap().is_empty());
    // The timer path: 46 s of no PLAYING → the timer lap → rotation throws → fatal.
    let m3 = make(&home.assets(), Some("city-a"), None).unwrap();
    let host3 = Arc::new(FakeHost::default());
    host3.fail_rotate.store(true, Ordering::SeqCst);
    m3.src.attach_host(host3.clone());
    tokio::time::advance(Duration::from_millis(46_000)).await;
    m3.src.tick();
    assert_eq!(host3.fatals.lock().unwrap().len(), 1);
}

/// `wanted && …`: an empty default id is falsy (no warn, catalog first).
#[test]
fn an_empty_scenario_id_falls_back_silently() {
    let home = Home::new("empty-id");
    two_pack(&home.assets());
    let m = make(&home.assets(), Some(""), None).unwrap();
    assert_eq!(m.src.status().scenario_id, "city-a");
    assert!(m
        .events
        .lock()
        .unwrap()
        .iter()
        .all(|(l, _)| *l == ActivityLevel::Info));
    assert_eq!(
        m.events.lock().unwrap()[0].1,
        "replay: scenario city-a loaded (3 samples, lap 3 s)"
    );
}

/// The TS throw texts for a broken pack (`replay.ts:84-117`), exactly.
#[test]
fn load_errors_carry_the_ts_text() {
    let home = Home::new("errs");
    let a = home.assets();
    let cat = a.join("catalog.json");
    assert_eq!(
        load_catalog(&a).unwrap_err(),
        format!(
            "ENOENT: no such file or directory, open '{}'",
            cat.display()
        )
    );
    std::fs::create_dir_all(&a).unwrap();
    std::fs::write(&cat, r#"{"v":2,"scenarios":[]}"#).unwrap();
    assert_eq!(
        load_catalog(&a).unwrap_err(),
        format!("{}: not a v1 catalog", cat.display())
    );
    std::fs::write(&cat, r#"{"v":1,"scenarios":{}}"#).unwrap();
    assert_eq!(
        load_catalog(&a).unwrap_err(),
        format!("{}: not a v1 catalog", cat.display())
    );
    std::fs::write(&cat, r#"{"v":1,"seedSet":"x","scenarios":[]}"#).unwrap();
    assert!(load_catalog(&a).unwrap().seed_set.is_empty());
    let r = ReplaySource::create(ReplayOptions {
        assets_dir: a.clone(),
        default_scenario_id: None,
        settings: Arc::new(|| None),
        media: None,
        on_event: None,
        now: Arc::new(|| 0),
        tick_ms: 0,
    });
    assert_eq!(r.err().unwrap(), "replay: the catalog has no scenarios");

    let mp = a.join("s/scenario.json");
    assert_eq!(
        load_scenario(&a, "s").unwrap_err(),
        format!("scenario s: {} missing", mp.display())
    );
    std::fs::create_dir_all(a.join("s/t")).unwrap();
    std::fs::write(&mp, r#"{"v":1,"id":"other","streams":[{}]}"#).unwrap();
    assert_eq!(
        load_scenario(&a, "s").unwrap_err(),
        "scenario s: scenario.json is not a v1 manifest for s"
    );
    std::fs::write(&mp, r#"{"v":1,"id":"s","streams":[]}"#).unwrap();
    assert_eq!(
        load_scenario(&a, "s").unwrap_err(),
        "scenario s: scenario.json is not a v1 manifest for s"
    );
    std::fs::write(
        &mp,
        r#"{"v":1,"id":"s","streams":[{"lap":"cam0/lap.h264"}],"telemetry":{"trace":"t/tr.gz"}}"#,
    )
    .unwrap();
    assert_eq!(
        load_scenario(&a, "s").unwrap_err(),
        "scenario s: cam0/lap.h264 missing"
    );
    std::fs::write(
        &mp,
        r#"{"v":1,"id":"s","streams":[{}],"telemetry":{"trace":"t/tr.gz"}}"#,
    )
    .unwrap();
    assert_eq!(
        load_scenario(&a, "s").unwrap_err(),
        "The \"path\" argument must be of type string. Received undefined"
    );
    std::fs::write(
        &mp,
        r#"{"v":1,"id":"s","streams":[{"lap":"t"}],"telemetry":{"trace":"t/tr.gz"}}"#,
    )
    .unwrap();
    let tr = a.join("s/t/tr.gz");
    assert_eq!(
        load_scenario(&a, "s").unwrap_err(),
        format!("ENOENT: no such file or directory, open '{}'", tr.display())
    );
    let gz = |body: &str| {
        std::fs::write(&tr, xorgate_hal::gzip_finalize::gzip_bytes(body.as_bytes())).unwrap()
    };
    gz("\n{\"t\":\"x\",\"metrics\":{}}\n{\"t\":1,\"metrics\":3}\n7\n");
    assert_eq!(
        load_scenario(&a, "s").unwrap_err(),
        "scenario s: empty trace"
    );
    gz("null\n");
    assert_eq!(
        load_scenario(&a, "s").unwrap_err(),
        "Cannot read properties of null (reading 't')"
    );
    gz("{\"t\":1,\"metrics\":{}}\n{not json\n");
    assert!(
        load_scenario(&a, "s").is_err(),
        "a bad line fails the load (the TS JSON.parse throw)"
    );
    // Stable sort by t; CRLF lines parse (JSON whitespace).
    gz("{\"t\":5,\"seq\":\"a\",\"metrics\":{}}\r\n{\"t\":1,\"seq\":\"b\",\"metrics\":{}}\n{\"t\":5,\"seq\":\"c\",\"metrics\":{}}\n");
    let l = load_scenario(&a, "s").unwrap();
    assert_eq!(
        l.samples
            .iter()
            .map(|s| s.seq.clone().unwrap())
            .collect::<Vec<_>>(),
        [json!("b"), json!("a"), json!("c")]
    );
    assert!(
        l.lap_ms.is_nan(),
        "no mediaDurationMs and no durationMs → NaN, as the TS"
    );
}
