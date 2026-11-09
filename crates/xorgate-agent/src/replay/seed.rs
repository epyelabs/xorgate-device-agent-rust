//! First-boot seeding (Phase 7e): the port of `src/agent/replay-seed.ts`
//! @ 09da38d. On a replay container's first start, lay the catalog's seed set
//! into `recordings/` as ready-to-upload sessions: the ORIGINAL fragments
//! (symlinked, byte for byte), FRESH UUIDv7 ids, and every timestamp moved by
//! ONE delta so the newest scenario ends 1 h before now. The ordinary
//! uploader ships them. Idempotent via `<home>/replay-seeded.json`.
//!
//! Numbers stay JS numbers (`f64`) end to end and are printed the way
//! `JSON.stringify` / template strings print them (P3), so a fractional or
//! absent pack timestamp gives the TS's bytes, not a truncation.
//! Errors carry Node's `err.message` for the fs calls (`ENOENT: …, open '…'`).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use xorgate_core::js::{number_to_string, to_iso_string, to_json_string};
use xorgate_core::telemetry::ActivityLevel;
use xorgate_hal::io::node_fs_message;

use super::{js_math_round, LoadedScenario, ReplayCatalog};

pub const SEED_MARKER: &str = "replay-seeded.json";
/// The newest seeded session ends this long before the seed (README §3.4: 1 h).
pub const SEED_END_BEFORE_MS: f64 = 3_600_000.0;
/// Samples per seeded telemetry segment (the real writer rotates at 60 s / 1 Hz).
const SEED_SEGMENT_SAMPLES: usize = 60;

#[derive(Debug, Clone, PartialEq)]
pub struct SeedSession {
    pub scenario_id: String,
    pub media_session_id: String,
    pub telemetry_session_id: String,
    pub stream_key: String,
    /// Shifted wall window of the video.
    pub start_ts: f64,
    pub end_ts: f64,
    pub fragments: f64,
    pub samples: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeedPlan {
    /// Added to every original timestamp.
    pub delta_ms: f64,
    pub sessions: Vec<SeedSession>,
}

/// `SeedResult` (`replay-seed.ts:80-85`).
#[derive(Debug, Clone, PartialEq)]
pub enum SeedResult {
    Seeded(SeedPlan),
    AlreadySeeded,
    EmptySeedSet,
}

/// A number as `JSON.stringify` writes it (`NaN`/`±Infinity` → `null`).
fn num(v: f64) -> Value {
    serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
}

fn session_json(s: &SeedSession) -> Value {
    let mut m = Map::new();
    m.insert("scenarioId".into(), Value::from(s.scenario_id.clone()));
    m.insert(
        "mediaSessionId".into(),
        Value::from(s.media_session_id.clone()),
    );
    m.insert(
        "telemetrySessionId".into(),
        Value::from(s.telemetry_session_id.clone()),
    );
    m.insert("streamKey".into(), Value::from(s.stream_key.clone()));
    m.insert("startTs".into(), num(s.start_ts));
    m.insert("endTs".into(), num(s.end_ts));
    m.insert("fragments".into(), num(s.fragments));
    m.insert("samples".into(), Value::from(s.samples));
    Value::Object(m)
}

/// `mintSessionId(ms)`: UUIDv7 of `Math.max(0, Math.floor(ms))`.
pub fn mint_session_id(ms: f64) -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    let floored = ms.floor();
    let ms = if floored.is_finite() && floored > 0.0 {
        floored as i64
    } else {
        0
    };
    xorgate_core::session::mint_session_id(ms, b)
}

/// `Math.max(...xs)`: NaN if any is NaN.
fn js_max(xs: impl Iterator<Item = f64>) -> f64 {
    let mut m = f64::NEG_INFINITY;
    for x in xs {
        if x.is_nan() {
            return f64::NAN;
        }
        if x > m {
            m = x;
        }
    }
    m
}

/// The first 8 chars (`.slice(0, 8)`).
fn head8(s: &str) -> String {
    s.chars().take(8).collect()
}

/// `planSeed` (`replay-seed.ts:88-111`): pick the delta and mint the ids.
pub fn plan_seed(
    scenarios: &[LoadedScenario],
    now: f64,
    end_before_ms: f64,
    mint: &dyn Fn(f64) -> String,
) -> SeedPlan {
    let newest_end = js_max(scenarios.iter().map(|s| s.manifest.original_end_ts));
    let delta_ms = js_math_round(now - end_before_ms - newest_end);
    let sessions = scenarios
        .iter()
        .map(|sc| {
            let start_ts = sc.manifest.original_start_ts + delta_ms;
            let end_ts = sc.manifest.original_end_ts + delta_ms;
            let stream = &sc.manifest.streams[0];
            SeedSession {
                scenario_id: sc.manifest.id.clone(),
                media_session_id: mint(start_ts),
                // The telemetry session starts at its first (possibly pre-video) sample.
                telemetry_session_id: mint(start_ts + sc.samples[0].t),
                stream_key: stream.stream_key.clone(),
                start_ts,
                end_ts,
                fragments: stream.fragment_count,
                samples: sc.samples.len(),
            }
        })
        .collect();
    SeedPlan { delta_ms, sessions }
}

/// `atomicWriteJson(path, value)` with Node's error text.
fn write_json_atomic(path: &Path, v: &Value) -> Result<(), String> {
    xorgate_hal::atomic_write::atomic_write_bytes_traced(path, to_json_string(v).as_bytes())
        .map_err(|e| e.node_message())
}

/// `readSidecars(dir)`: `^\d{5}\.json$`, sorted, parsed.
fn read_sidecars(dir: &Path) -> Result<Vec<(String, Value)>, String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| node_fs_message(&e, "scandir", dir))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| {
            n.len() == 10
                && n.ends_with(".json")
                && n.as_bytes()[..5].iter().all(u8::is_ascii_digit)
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let p = dir.join(&name);
            let text = std::fs::read(&p).map_err(|e| node_fs_message(&e, "open", &p))?;
            let v = serde_json::from_slice::<Value>(&text)
                .map(xorgate_core::js::parsed_key_order)
                .map_err(|e| e.to_string())?;
            Ok((name, v))
        })
        .collect()
}

/// What `writeSeedSession` needs (`Pick<SeedOptions, …>`).
pub struct SeedTarget<'a> {
    pub recordings_root: &'a Path,
    pub device_id: &'a str,
    pub boot_id: &'a str,
}

/// `writeSeedSession` (`replay-seed.ts:121-185`): one scenario's media +
/// telemetry session. Writes only under `recordings_root`.
pub fn write_seed_session(
    sc: &LoadedScenario,
    session: &SeedSession,
    plan: &SeedPlan,
    t: &SeedTarget<'_>,
) -> Result<(), String> {
    let stream = &sc.manifest.streams[0];
    let seg_dir = sc.dir.join(&stream.fragments);
    let video_dir = t
        .recordings_root
        .join("video")
        .join(&session.stream_key)
        .join(&session.media_session_id);
    mkdir_p(&video_dir)?;
    // One wall/mono pair for the whole session, as buildSidecar derives it.
    let wall_ms = crate::now_ms() as f64;
    let mono_ms = xorgate_hal::fragment_stat::monotonic_ms() as f64;
    for (name, orig) in read_sidecars(&seg_dir)? {
        let base = &name[..5];
        let src = seg_dir.join(format!("{base}.mp4"));
        let dst = video_dir.join(format!("{base}.mp4"));
        std::os::unix::fs::symlink(&src, &dst).map_err(|e| {
            format!(
                "{} -> '{}'",
                node_fs_message(&e, "symlink", &src),
                dst.display()
            )
        })?;
        // `{...orig, deviceId, sessionId, startTs, startMono, timeSource}`.
        let mut obj = match orig {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        let start_ts = obj
            .get("startTs")
            .and_then(Value::as_f64)
            .unwrap_or(f64::NAN)
            + plan.delta_ms;
        obj.insert("deviceId".into(), Value::from(t.device_id));
        obj.insert(
            "sessionId".into(),
            Value::from(session.media_session_id.clone()),
        );
        obj.insert("startTs".into(), num(start_ts));
        obj.insert(
            "startMono".into(),
            num(js_math_round(mono_ms - (wall_ms - start_ts))),
        );
        obj.insert("timeSource".into(), Value::from("ntp"));
        write_json_atomic(&video_dir.join(&name), &Value::Object(obj))?;
    }

    let tel_dir = t
        .recordings_root
        .join("telemetry")
        .join(&session.telemetry_session_id);
    mkdir_p(&tel_dir)?;
    let started_at = session.start_ts + sc.samples[0].t;
    let mut header = Map::new();
    header.insert("type".into(), Value::from("header"));
    header.insert("v".into(), Value::from(1));
    header.insert("deviceId".into(), Value::from(t.device_id));
    header.insert(
        "sessionId".into(),
        Value::from(session.telemetry_session_id.clone()),
    );
    header.insert("bootId".into(), Value::from(t.boot_id));
    header.insert("timeSource".into(), Value::from("ntp"));
    header.insert("rateHz".into(), num(sc.manifest.telemetry.rate_hz));
    header.insert("startedAt".into(), num(started_at));
    let header = to_json_string(&Value::Object(header));
    for (seg, chunk) in sc.samples.chunks(SEED_SEGMENT_SAMPLES).enumerate() {
        let first_ts = session.start_ts + chunk[0].t;
        let mut text = header.clone();
        for s in chunk {
            let mut line = Map::new();
            line.insert("ts".into(), num(session.start_ts + s.t));
            // The live writer stamps hrtime ns; keep the field coherent.
            line.insert("mono".into(), num(js_math_round((mono_ms + s.t) * 1e6)));
            if let Some(seq) = &s.seq {
                line.insert("seq".into(), seq.clone());
            }
            line.insert("metrics".into(), s.raw_metrics.clone());
            text.push('\n');
            text.push_str(&to_json_string(&Value::Object(line)));
        }
        text.push('\n');
        let name = format!("seg-{}-{seg:05}.jsonl.gz", number_to_string(first_ts));
        let p = tel_dir.join(name);
        std::fs::write(&p, xorgate_hal::gzip_finalize::gzip_bytes(text.as_bytes()))
            .map_err(|e| node_fs_message(&e, "open", &p))?;
    }
    Ok(())
}

/// `mkdirSync(dir, { recursive: true })`.
fn mkdir_p(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| node_fs_message(&e, "mkdir", dir))
}

/// `SeedOptions` (`replay-seed.ts:61-78`).
pub struct SeedOptions<'a> {
    pub catalog: &'a ReplayCatalog,
    /// Loader for each seed-set scenario (missing ones are skipped with a warning).
    pub load_scenario: &'a dyn Fn(&str) -> Result<LoadedScenario, String>,
    /// XORGATE_HOME/recordings.
    pub recordings_root: PathBuf,
    /// Where the marker lives (XORGATE_HOME).
    pub home: PathBuf,
    pub device_id: &'a str,
    pub boot_id: &'a str,
    pub end_before_ms: Option<f64>,
    /// `Date.now()` by default.
    pub now: Option<&'a dyn Fn() -> f64>,
    /// Session-id minter (UUIDv7 of the shifted start by default).
    pub mint: Option<&'a dyn Fn(f64) -> String>,
    pub on_event: Option<SeedEventFn<'a>>,
}

/// `onEvent(level, msg)`.
pub type SeedEventFn<'a> = &'a dyn Fn(ActivityLevel, &str);

/// `seedOnce` (`replay-seed.ts:192-224`). `Err` = a filesystem failure (the
/// TS throw), with Node's message.
pub fn seed_once(opts: &SeedOptions<'_>) -> Result<SeedResult, String> {
    let marker = opts.home.join(SEED_MARKER);
    if marker.exists() {
        return Ok(SeedResult::AlreadySeeded);
    }
    let now = || match opts.now {
        Some(f) => f(),
        None => crate::now_ms() as f64,
    };
    let event = |l: ActivityLevel, m: &str| {
        if let Some(f) = opts.on_event {
            f(l, m);
        }
    };
    let mut scenarios = Vec::new();
    for id in &opts.catalog.seed_set {
        match (opts.load_scenario)(id) {
            Ok(s) => scenarios.push(s),
            Err(e) => event(
                ActivityLevel::Warn,
                &format!("replay seed: skipping {id}: {e}"),
            ),
        }
    }
    if scenarios.is_empty() {
        let mut m = Map::new();
        m.insert("v".into(), Value::from(1));
        m.insert("at".into(), num(now()));
        m.insert("sessions".into(), Value::Array(vec![]));
        write_json_atomic(&marker, &Value::Object(m))?;
        return Ok(SeedResult::EmptySeedSet);
    }
    let default_mint = |ms: f64| mint_session_id(ms);
    let plan = plan_seed(
        &scenarios,
        now(),
        opts.end_before_ms.unwrap_or(SEED_END_BEFORE_MS),
        opts.mint.unwrap_or(&default_mint),
    );
    let target = SeedTarget {
        recordings_root: &opts.recordings_root,
        device_id: opts.device_id,
        boot_id: opts.boot_id,
    };
    for session in &plan.sessions {
        let sc = scenarios
            .iter()
            .find(|s| s.manifest.id == session.scenario_id)
            .expect("planned from this list");
        write_seed_session(sc, session, &plan, &target)?;
        event(
            ActivityLevel::Info,
            &format!(
                "replay seed: {} → media {}… ({} fragments) + telemetry {}… ({} samples) at {}",
                session.scenario_id,
                head8(&session.media_session_id),
                number_to_string(session.fragments),
                head8(&session.telemetry_session_id),
                session.samples,
                to_iso_string(session.start_ts),
            ),
        );
    }
    let mut m = Map::new();
    m.insert("v".into(), Value::from(1));
    m.insert("at".into(), num(now()));
    m.insert("deltaMs".into(), num(plan.delta_ms));
    m.insert(
        "sessions".into(),
        Value::Array(plan.sessions.iter().map(session_json).collect()),
    );
    write_json_atomic(&marker, &Value::Object(m))?;
    Ok(SeedResult::Seeded(plan))
}
