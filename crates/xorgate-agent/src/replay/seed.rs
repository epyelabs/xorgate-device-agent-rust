//! First-boot seeding — Phase 7e SPIKE port of `src/agent/replay-seed.ts`
//! @ 09da38d. On a replay container's first start, lay the catalog's seed set
//! into `recordings/` as ready-to-upload sessions: the ORIGINAL fragments
//! (symlinked, byte-for-byte), FRESH UUIDv7 ids, and every timestamp moved by
//! ONE delta so the newest scenario ends 1 h before now. The ordinary
//! uploader ships them. Idempotent via `<home>/replay-seeded.json`.

use std::io::Write;
use std::path::Path;

use serde_json::{json, Map, Value};

use super::{LoadedScenario, ReplayCatalog};

pub const SEED_MARKER: &str = "replay-seeded.json";
/// The newest seeded session ends this long before the seed.
pub const SEED_END_BEFORE_MS: i64 = 3_600_000;
/// Samples per seeded telemetry segment.
const SEED_SEGMENT_SAMPLES: usize = 60;

#[derive(Debug, Clone)]
pub struct SeedSession {
    pub scenario_id: String,
    pub media_session_id: String,
    pub telemetry_session_id: String,
    pub stream_key: String,
    pub start_ts: i64,
    pub end_ts: i64,
    pub fragments: u64,
    pub samples: usize,
}

#[derive(Debug, Clone)]
pub struct SeedPlan {
    pub delta_ms: i64,
    pub sessions: Vec<SeedSession>,
}

fn session_json(s: &SeedSession) -> Value {
    json!({
        "scenarioId": s.scenario_id,
        "mediaSessionId": s.media_session_id,
        "telemetrySessionId": s.telemetry_session_id,
        "streamKey": s.stream_key,
        "startTs": s.start_ts,
        "endTs": s.end_ts,
        "fragments": s.fragments,
        "samples": s.samples,
    })
}

fn mint(ms: i64) -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    xorgate_core::session::mint_session_id(ms, b)
}

/// `planSeed`: pick the delta and mint the ids.
pub fn plan_seed(scenarios: &[LoadedScenario], now: i64, end_before_ms: i64) -> SeedPlan {
    let newest_end = scenarios
        .iter()
        .map(|s| s.manifest.original_end_ts)
        .fold(f64::NEG_INFINITY, f64::max);
    let delta_ms = (now as f64 - end_before_ms as f64 - newest_end).round() as i64;
    let sessions = scenarios
        .iter()
        .map(|sc| {
            let start_ts = (sc.manifest.original_start_ts + delta_ms as f64) as i64;
            let end_ts = (sc.manifest.original_end_ts + delta_ms as f64) as i64;
            let stream = &sc.manifest.streams[0];
            SeedSession {
                scenario_id: sc.manifest.id.clone(),
                media_session_id: mint(start_ts),
                telemetry_session_id: mint(start_ts + sc.samples[0].t as i64),
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

fn write_atomic(path: &Path, v: &Value) -> std::io::Result<()> {
    xorgate_hal::atomic_write::atomic_write_json(path, v)
}

/// `writeSeedSession`: one scenario's media + telemetry session.
pub fn write_seed_session(
    sc: &LoadedScenario,
    session: &SeedSession,
    plan: &SeedPlan,
    recordings_root: &Path,
    device_id: &str,
    boot_id: &str,
) -> std::io::Result<()> {
    let stream = &sc.manifest.streams[0];
    let seg_dir = sc.dir.join(&stream.fragments);
    let video_dir = recordings_root
        .join("video")
        .join(&session.stream_key)
        .join(&session.media_session_id);
    std::fs::create_dir_all(&video_dir)?;
    let wall_ms = crate::now_ms() as f64;
    let mono_ms = xorgate_hal::fragment_stat::monotonic_ms() as f64;
    let mut names: Vec<String> = std::fs::read_dir(&seg_dir)?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.len() == 10 && n.ends_with(".json") && n[..5].bytes().all(|b| b.is_ascii_digit()))
        .collect();
    names.sort();
    for name in names {
        let base = &name[..5];
        let src = seg_dir.join(format!("{base}.mp4"));
        let dst = video_dir.join(format!("{base}.mp4"));
        std::os::unix::fs::symlink(&src, &dst)?;
        let orig: Value = serde_json::from_slice(&std::fs::read(seg_dir.join(&name))?)
            .map_err(std::io::Error::other)?;
        let Value::Object(mut obj) = orig else {
            continue;
        };
        let start_ts = obj.get("startTs").and_then(Value::as_f64).unwrap_or(0.0) + plan.delta_ms as f64;
        obj.insert("deviceId".into(), json!(device_id));
        obj.insert("sessionId".into(), json!(session.media_session_id));
        obj.insert("startTs".into(), json!(start_ts as i64));
        obj.insert(
            "startMono".into(),
            json!((mono_ms - (wall_ms - start_ts)).round() as i64),
        );
        obj.insert("timeSource".into(), json!("ntp"));
        write_atomic(&video_dir.join(&name), &Value::Object(obj))?;
    }

    let tel_dir = recordings_root
        .join("telemetry")
        .join(&session.telemetry_session_id);
    std::fs::create_dir_all(&tel_dir)?;
    let started_at = session.start_ts + sc.samples[0].t as i64;
    let header = json!({
        "type": "header",
        "v": 1,
        "deviceId": device_id,
        "sessionId": session.telemetry_session_id,
        "bootId": boot_id,
        "timeSource": "ntp",
        "rateHz": sc.manifest.telemetry.rate_hz,
        "startedAt": started_at,
    });
    for (seg, chunk) in sc.samples.chunks(SEED_SEGMENT_SAMPLES).enumerate() {
        let first_ts = session.start_ts + chunk[0].t as i64;
        let mut text = serde_json::to_string(&header).map_err(std::io::Error::other)?;
        for s in chunk {
            let mut line = Map::new();
            line.insert("ts".into(), json!(session.start_ts + s.t as i64));
            line.insert("mono".into(), json!(((mono_ms + s.t) * 1e6).round() as i64));
            line.insert("seq".into(), s.seq.clone());
            line.insert("metrics".into(), s.raw_metrics.clone());
            text.push('\n');
            text.push_str(&serde_json::to_string(&Value::Object(line)).map_err(std::io::Error::other)?);
        }
        text.push('\n');
        let name = format!("seg-{first_ts}-{seg:05}.jsonl.gz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(text.as_bytes())?;
        std::fs::write(tel_dir.join(name), gz.finish()?)?;
    }
    Ok(())
}

/// `seedOnce`. Returns the plan when it seeded, `None` when already seeded
/// (or the seed set was empty). `Err` = a filesystem failure.
pub fn seed_once(
    catalog: &ReplayCatalog,
    assets_dir: &Path,
    recordings_root: &Path,
    home: &Path,
    device_id: &str,
    boot_id: &str,
    on_warn: &dyn Fn(&str),
    on_info: &dyn Fn(&str),
) -> std::io::Result<Option<SeedPlan>> {
    let marker = home.join(SEED_MARKER);
    if marker.exists() {
        return Ok(None);
    }
    let mut scenarios = Vec::new();
    for id in &catalog.seed_set {
        match super::load_scenario(assets_dir, id) {
            Ok(s) => scenarios.push(s),
            Err(e) => on_warn(&format!("replay seed: skipping {id}: {e}")),
        }
    }
    let now = crate::now_ms();
    if scenarios.is_empty() {
        write_atomic(&marker, &json!({"v": 1, "at": now, "sessions": []}))?;
        return Ok(None);
    }
    let plan = plan_seed(&scenarios, now, SEED_END_BEFORE_MS);
    for session in &plan.sessions {
        let sc = scenarios
            .iter()
            .find(|s| s.manifest.id == session.scenario_id)
            .expect("planned from this list");
        write_seed_session(sc, session, &plan, recordings_root, device_id, boot_id)?;
        on_info(&format!(
            "replay seed: {} → media {}… ({} fragments) + telemetry {}… ({} samples) at {}",
            session.scenario_id,
            &session.media_session_id[..8],
            session.fragments,
            &session.telemetry_session_id[..8],
            session.samples,
            xorgate_core::js::to_iso_string(session.start_ts as f64),
        ));
    }
    write_atomic(
        &marker,
        &json!({
            "v": 1,
            "at": crate::now_ms(),
            "deltaMs": plan.delta_ms,
            "sessions": plan.sessions.iter().map(session_json).collect::<Vec<_>>(),
        }),
    )?;
    Ok(Some(plan))
}
