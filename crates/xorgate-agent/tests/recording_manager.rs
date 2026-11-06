//! `RecordingManager` lifecycle tests (`tokio::time::pause`) — the six
//! `it()`s of `test/recording-manager.test.ts`, the orphan-recovery `it()` of
//! "RecordingManager video wiring", the timers (`SETTINGS_POLL_MS` 5 s,
//! `SHED_SAMPLE_MS` 1 s, `DISK_ENFORCE_MS` 60 s, `TIME_PROBE_TIMEOUT_MS` 2 s),
//! connectivity / `setUpload` / `stop`, the hook wiring, the no-deadlock rule
//! (P9) and the single live-parking call site (invariant 6). Components are
//! the fakes of `recording_support` (the I/O halves are WP2–WP4's).

#[path = "recording_support/mod.rs"]
mod recording_support;

use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use recording_support::*;
use serde_json::json;
use xorgate_agent::ctx::PhaseHooks;
use xorgate_agent::recording::manager::{CaptureStatus, VideoRecordingStatus};
use xorgate_agent::recording::RecordingHooks;
use xorgate_core::telemetry::{BoolLeaf, MediaStream, TelemetryMedia};
use xorgate_core::types::{GnssView, PowerSampleReading, SkyState, TimeSource};

async fn yields() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn hooks_for(m: xorgate_agent::recording::RecordingManager, rig: &Rig) -> RecordingHooks {
    let h = RecordingHooks::new(None, None, Arc::new(|| None), rig.clock.clone());
    h.install(m);
    h
}

// --- test/recording-manager.test.ts -----------------------------------------

/// "creates nothing when recording is disabled (disabled-by-default regression)".
#[tokio::test(start_paused = true)]
async fn creates_nothing_when_recording_is_disabled() {
    let mut rig = Rig::new("off");
    rig.video = false;
    let m = rig.build();
    m.start().await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!rig.root().exists());
    assert_eq!(rig.log.finalize_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.log.disks.load(Ordering::SeqCst), 0);
    let st = m.status();
    assert!(!st.enabled);
    assert_eq!(st.session_id, None);
    assert_eq!(
        serde_json::to_value(&st).unwrap(),
        serde_json::to_value(xorgate_core::telemetry::RecordingStatus::all_off()).unwrap()
    );
    m.stop();
}

/// "starts and stops the recorder live from settings changes".
#[tokio::test(start_paused = true)]
async fn starts_and_stops_the_recorder_live() {
    let mut rig = Rig::new("live");
    rig.video = false;
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 20}}));
    let m = rig.build();
    m.start().await;
    let st = m.status();
    assert!(st.enabled);
    let sid = st.session_id.clone().expect("session");
    let rec = rig.log.recorders.lock().unwrap()[0].clone();
    assert_eq!(rec.rate_hz, 20);
    assert_eq!(rec.dir, rig.root().join("telemetry").join(&sid));
    assert_eq!(rec.session_id, sid);
    assert_eq!(
        rig.events(),
        vec![format!(
            "telemetry recording started (session {}…, 20 Hz)",
            &sid[..8]
        )]
    );
    rig.save_recording(&json!({"telemetry": {"enabled": false}}));
    m.reapply().unwrap();
    assert!(!m.status().enabled);
    assert_eq!(m.status().session_id, None);
    assert_eq!(rig.log.recorder_stops.load(Ordering::SeqCst), 1);
    // TS quirk kept: `{enabled:false}` resolves rateHz to the default 1 ≠ 20,
    // so `rateChanged` wins the reason (`manager.ts:437-440`).
    assert_eq!(
        rig.events().last().unwrap(),
        "telemetry recording stopped (rateHz changed)"
    );
    rig.save_recording(&json!({"telemetry": {"enabled": true}}));
    m.reapply().unwrap();
    rig.save_recording(&json!({"telemetry": {"enabled": false}}));
    m.reapply().unwrap();
    assert_eq!(
        rig.events().last().unwrap(),
        "telemetry recording stopped (recording disabled)"
    );
    m.stop();
}

/// "holds the recorder while the clock is untrusted and starts it on the
/// next poll once trusted (clock-trust gate)" (`test/recording-manager.test.ts`
/// @ 09da38d).
#[tokio::test(start_paused = true)]
async fn holds_the_recorder_while_the_clock_is_untrusted() {
    let mut rig = Rig::new("clockhold");
    rig.video = false;
    let trusted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let t = trusted.clone();
    rig.clock_trusted = Some(Arc::new(move || t.load(Ordering::SeqCst)));
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 20}}));
    let m = rig.build();
    m.start().await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let st = m.status();
    assert!(!st.enabled);
    assert_eq!(st.session_id, None);
    assert!(st.clock_hold);
    // `clockHold` sits after powerShed, before liveBlock; absent when false.
    let v = serde_json::to_value(&st).unwrap();
    assert_eq!(v["clockHold"], json!(true));
    assert!(
        !rig.root().join("telemetry").exists(),
        "no session minted on the bad clock"
    );
    let held = |rig: &Rig| {
        rig.events()
            .iter()
            .filter(|m| m.contains("recording held"))
            .count()
    };
    assert_eq!(held(&rig), 1);
    assert!(rig.events().contains(
        &"telemetry recording held: wall clock not trusted yet (waiting for NTP/GPS/NITZ, 5 min cap)"
            .to_string()
    ));
    m.reapply().unwrap(); // unchanged settings, still untrusted: no start, no 2nd event
    assert!(m.status().clock_hold);
    assert_eq!(held(&rig), 1);

    trusted.store(true, Ordering::SeqCst);
    m.reapply().unwrap(); // the worker's settings poll
    tokio::time::sleep(Duration::from_millis(150)).await;
    let st = m.status();
    assert!(st.enabled);
    assert!(st.session_id.is_some());
    assert!(!st.clock_hold);
    assert!(
        serde_json::to_value(&st)
            .unwrap()
            .get("clockHold")
            .is_none(),
        "absent, never false"
    );
    assert!(rig
        .events()
        .iter()
        .any(|m| m.starts_with("telemetry recording started")));
    m.stop();
}

/// The gate holds a recorder that is NOT wanted: no `clockHold`, no event.
#[tokio::test(start_paused = true)]
async fn clock_hold_is_absent_when_telemetry_is_off() {
    let mut rig = Rig::new("clockhold-off");
    rig.video = false;
    rig.clock_trusted = Some(Arc::new(|| false));
    let m = rig.build();
    m.start().await;
    assert!(!m.status().clock_hold);
    assert!(rig.events().iter().all(|m| !m.contains("recording held")));
    m.stop();
}

/// "mints a new session when rateHz changes".
#[tokio::test(start_paused = true)]
async fn mints_a_new_session_when_rate_changes() {
    let mut rig = Rig::new("rate");
    rig.video = false;
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 20}}));
    let m = rig.build();
    m.start().await;
    let first = m.status().session_id.unwrap();
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 5}}));
    m.reapply().unwrap();
    let second = m.status().session_id.unwrap();
    assert_ne!(first, second);
    let ev = rig.events();
    assert_eq!(ev[1], "telemetry recording stopped (rateHz changed)");
    assert!(ev[2].ends_with("…, 5 Hz)"), "{ev:?}");
    m.stop();
}

/// "stamps the first session with the PROBED time source, not the placeholder".
#[tokio::test(start_paused = true)]
async fn first_session_sees_the_probed_time_source() {
    let mut rig = Rig::new("probe");
    rig.video = false;
    rig.time_source = Arc::new(FakeTimeSource {
        value: Mutex::new(TimeSource::Unsynced),
        probed: TimeSource::Ntp,
        hang: false,
        delay_ms: 20,
    });
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 20}}));
    let m = rig.build();
    m.start().await;
    assert_eq!(rig.log.time_sources.lock().unwrap()[0], TimeSource::Ntp);
    m.stop();
}

/// "first video session.json carries the probed time source (badge-bug fix)"
/// (video wiring #4): the capture supervisor is built after the probe, so its
/// `timeSource()` — what `session.json` is stamped from — is the probed value.
#[tokio::test(start_paused = true)]
async fn first_video_session_sees_the_probed_time_source() {
    let mut rig = Rig::new("vprobe");
    rig.time_source = Arc::new(FakeTimeSource {
        value: Mutex::new(TimeSource::Unsynced),
        probed: TimeSource::Ntp,
        hang: false,
        delay_ms: 20,
    });
    rig.save_recording(&json!({"video": {"enabled": true, "bootDefer": "off"}}));
    let m = rig.build();
    m.start().await;
    m.settled().await;
    assert_eq!(
        rig.log.captures.lock().unwrap()[0],
        (true, vec!["cam0".to_string()])
    );
    assert_eq!(rig.log.time_sources.lock().unwrap()[0], TimeSource::Ntp);
    m.stop();
}

/// "a hung probe cannot delay start beyond the bound (proceeds pessimistic)".
#[tokio::test(start_paused = true)]
async fn a_hung_probe_is_bounded() {
    let mut rig = Rig::new("hung");
    rig.video = false;
    rig.time_source = Arc::new(FakeTimeSource {
        value: Mutex::new(TimeSource::Unsynced),
        probed: TimeSource::Ntp,
        hang: true,
        delay_ms: 0,
    });
    rig.time_probe_timeout_ms = Some(50);
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 20}}));
    let m = rig.build();
    let t0 = tokio::time::Instant::now();
    m.start().await;
    assert_eq!(t0.elapsed(), Duration::from_millis(50));
    assert!(m.status().enabled);
    assert_eq!(
        rig.log.time_sources.lock().unwrap()[0],
        TimeSource::Unsynced
    );
    m.stop();
}

/// The default bound is `TIME_PROBE_TIMEOUT_MS` = 2 s.
#[tokio::test(start_paused = true)]
async fn the_default_probe_bound_is_two_seconds() {
    let mut rig = Rig::new("hung2");
    rig.video = false;
    rig.time_source = Arc::new(FakeTimeSource {
        value: Mutex::new(TimeSource::Unsynced),
        probed: TimeSource::Ntp,
        hang: true,
        delay_ms: 0,
    });
    let m = rig.build();
    let t0 = tokio::time::Instant::now();
    m.start().await;
    assert_eq!(t0.elapsed(), Duration::from_millis(2000));
    m.stop();
}

/// D41 (Rust only): `start()` sweeps dead owners' `/dev/shm` segments ONCE,
/// before the crash recovery and before any capture can spawn, even with
/// recording off; the removal is journal-only (no activity event).
#[tokio::test(start_paused = true)]
async fn start_sweeps_dead_shm_segments_once_before_recovery() {
    let mut rig = Rig::new("shm");
    rig.video = false;
    std::fs::create_dir_all(rig.root().join("telemetry").join("prev-session")).unwrap();
    let m = rig.build();
    m.start().await;
    yields().await;
    assert_eq!(
        *rig.log.sweeps.lock().unwrap(),
        vec![0],
        "one sweep, before finalize"
    );
    assert_eq!(rig.log.finalize_calls.load(Ordering::SeqCst), 1);
    assert!(
        rig.events().iter().all(|e| !e.contains("shm")),
        "no activity event: {:?}",
        rig.events()
    );
    m.stop();
}

/// "finalizes orphans from a previous run on start, even with recording off".
#[tokio::test(start_paused = true)]
async fn finalizes_orphans_on_start_with_recording_off() {
    let mut rig = Rig::new("orph");
    rig.video = false;
    rig.orphans = 1;
    std::fs::create_dir_all(rig.root().join("telemetry").join("prev-session")).unwrap();
    let m = rig.build();
    m.start().await;
    yields().await;
    assert_eq!(rig.log.finalize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.log.recover_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.log.disks.load(Ordering::SeqCst), 1, "ensureDisk");
    assert!(
        rig.log.enforces.load(Ordering::SeqCst) >= 1,
        "initial enforce"
    );
    assert_eq!(
        rig.events(),
        vec!["recording: finalized 1 orphan segment(s) from previous run"]
    );
    assert!(!m.status().enabled);
    m.stop();
}

/// "orphan video sessions get sidecars on start" (video wiring #5): the
/// recovery pass runs before any capture spawns and reports its count.
#[tokio::test(start_paused = true)]
async fn orphan_video_sessions_are_backfilled_on_start() {
    let mut rig = Rig::new("vorph");
    rig.backfilled = 1;
    std::fs::create_dir_all(rig.root().join("video").join("cam0").join("sess-dead")).unwrap();
    let m = rig.build();
    m.start().await;
    let ev = rig.events();
    assert_eq!(
        ev[0],
        "recording: backfilled 1 video sidecar(s) from previous run"
    );
    assert_eq!(rig.log.recover_calls.load(Ordering::SeqCst), 1);
    m.stop();
}

// --- timers -------------------------------------------------------------------

/// `SETTINGS_POLL_MS`: a settings.json change applies within one 5 s poll.
#[tokio::test(start_paused = true)]
async fn the_settings_poll_applies_changes_every_five_seconds() {
    let mut rig = Rig::new("poll");
    rig.video = false;
    rig.poll_ms = 5_000;
    let m = rig.build();
    m.start().await;
    rig.save_recording(&json!({"telemetry": {"enabled": true}}));
    tokio::time::sleep(Duration::from_millis(4_900)).await;
    assert!(!m.status().enabled);
    tokio::time::sleep(Duration::from_millis(150)).await;
    yields().await;
    assert!(m.status().enabled, "applied by the 5 s poll");
    m.stop();
}

/// `SHED_SAMPLE_MS`: the 1 s monitor arms with a camera + sampler, walks to
/// rung 1 on its own ticks, and disarms (no more samples) once no camera is left.
#[tokio::test(start_paused = true)]
async fn the_shed_monitor_ticks_every_second_and_disarms() {
    let mut rig = Rig::new("shed");
    rig.shed_sample_ms = 1_000;
    *rig.reading.lock().unwrap() = Some(PowerSampleReading {
        uv: true,
        rail_v: Some(4.7),
        temp_c: None,
        soft_temp: None,
    });
    rig.save_recording(&json!({
        "video": {"enabled": true, "bootDefer": "off"},
        "powerShedding": {"uvWindowSecs": 5, "escalateDwellSecs": 5, "restoreSecs": 10}
    }));
    let m = rig.build();
    m.start().await;
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        yields().await;
    }
    assert_eq!(rig.log.sample_calls.load(Ordering::SeqCst), 3);
    assert_eq!(*rig.log.live_shed.lock().unwrap(), vec![true]);
    assert_eq!(m.status().power_shed.as_deref(), Some("no-live"));
    // Every camera disabled → monitor disarmed, ladder reset, live released.
    rig.save_recording(&json!({
        "video": {"enabled": true, "bootDefer": "off", "cameras": {"cam0": false}},
        "powerShedding": {"uvWindowSecs": 5, "escalateDwellSecs": 5, "restoreSecs": 10}
    }));
    m.reapply().unwrap();
    let calls = rig.log.sample_calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(5_000)).await;
    yields().await;
    assert_eq!(
        rig.log.sample_calls.load(Ordering::SeqCst),
        calls,
        "disarmed"
    );
    assert_eq!(m.status().power_shed, None);
    assert!(rig
        .events()
        .contains(&"power shedding disarmed — restoring full operation".to_string()));
    assert_eq!(*rig.log.live_shed.lock().unwrap(), vec![true, false]);
    m.stop();
}

/// `DISK_ENFORCE_MS`: enforcement on creation, then every 60 s.
#[tokio::test(start_paused = true)]
async fn the_disk_sweep_runs_every_minute() {
    let mut rig = Rig::new("disk");
    rig.video = false;
    rig.save_recording(&json!({"telemetry": {"enabled": true}}));
    let m = rig.build();
    m.start().await;
    yields().await;
    let n0 = rig.log.enforces.load(Ordering::SeqCst);
    assert!(n0 >= 1);
    tokio::time::sleep(Duration::from_millis(59_000)).await;
    yields().await;
    assert_eq!(rig.log.enforces.load(Ordering::SeqCst), n0);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    yields().await;
    assert_eq!(rig.log.enforces.load(Ordering::SeqCst), n0 + 1);
    m.stop();
}

// --- connectivity / setUpload / stop -------------------------------------------

/// `notifyConnectivity` kicks / pauses; `setUpload` replaces the uploader.
#[tokio::test(start_paused = true)]
async fn connectivity_and_upload_rearm() {
    let mut rig = Rig::new("up");
    rig.video = false;
    rig.upload = true;
    let m = rig.build();
    assert_eq!(
        rig.log.uploaders.load(Ordering::SeqCst),
        1,
        "built in the constructor"
    );
    m.start().await;
    let k0 = rig.log.kicks.load(Ordering::SeqCst);
    assert!(k0 >= 2, "apply() + start() kick");
    m.notify_connectivity(true);
    assert_eq!(rig.log.kicks.load(Ordering::SeqCst), k0 + 1);
    m.notify_connectivity(false);
    assert_eq!(rig.log.pauses.load(Ordering::SeqCst), 1);
    m.set_upload(None);
    assert_eq!(
        rig.log.uploaders.load(Ordering::SeqCst),
        1,
        "None is a no-op"
    );
    m.set_upload(Some(xorgate_cloud::s3::UploadOptions {
        put: Arc::new(NoopPut),
        is_online: Arc::new(|| true),
    }));
    assert_eq!(rig.log.uploader_stops.load(Ordering::SeqCst), 1);
    assert_eq!(rig.log.uploaders.load(Ordering::SeqCst), 2);
    assert_eq!(rig.log.kicks.load(Ordering::SeqCst), k0 + 2);
    m.stop();
    assert_eq!(rig.log.uploader_stops.load(Ordering::SeqCst), 2);
}

struct NoopPut;
impl xorgate_cloud::s3::ObjectPut for NoopPut {
    fn put<'a>(
        &'a self,
        _key: &'a str,
        _body: bytes::Bytes,
        _content_type: &'a str,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> xorgate_cloud::s3::PutFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

/// `stop()`: recorder finalized ("agent shutdown"), capture SIGINTed, timers
/// off; a late config message (`reapply`) cannot restart anything.
#[tokio::test(start_paused = true)]
async fn stop_is_final() {
    let mut rig = Rig::new("stop");
    rig.poll_ms = 5_000;
    rig.save_recording(
        &json!({"telemetry": {"enabled": true}, "video": {"enabled": true, "bootDefer": "off"}}),
    );
    let m = rig.build();
    m.start().await;
    m.settled().await;
    assert_eq!(rig.live_cams(), vec!["cam0"]);
    m.stop();
    assert_eq!(rig.live_cams(), Vec::<String>::new());
    assert_eq!(rig.log.recorder_stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.events().last().unwrap(),
        "telemetry recording stopped (agent shutdown)"
    );
    assert!(!m.status().enabled);
    assert_eq!(m.video_key(), "");
    let spawns = rig.spawns().len();
    rig.save_recording(
        &json!({"telemetry": {"enabled": true, "rateHz": 5}, "video": {"enabled": false}}),
    );
    m.reapply().unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    yields().await;
    assert_eq!(rig.spawns().len(), spawns);
    assert_eq!(rig.log.recorders.lock().unwrap().len(), 1);
}

/// The worker's `onCaptureSpawned` reaches the media supervisor's bounce for
/// every capture spawn (`[media camN] restarting master …`, D6).
#[tokio::test(start_paused = true)]
async fn every_capture_spawn_bounces_its_master() {
    let mut rig = Rig::new("bounce");
    rig.cameras = vec!["cam0".into(), "cam1".into()];
    rig.save_recording(
        &json!({"video": {"enabled": true, "bootDefer": "off", "spawnStaggerSecs": 0}}),
    );
    let m = rig.build();
    m.start().await;
    m.settled().await;
    assert_eq!(
        *rig.log.spawned_callbacks.lock().unwrap(),
        vec!["cam0", "cam1"]
    );
    m.stop();
}

// --- hooks ---------------------------------------------------------------------

/// The hook wiring: connect/close → `notify_connectivity`; the tick overlay;
/// `final_recording_status` after `recording_stop`; `report_hints` from the
/// published snapshot.
#[tokio::test(start_paused = true)]
async fn hooks_wire_every_call_site() {
    let mut rig = Rig::new("hooks");
    rig.upload = true;
    rig.budget_ma = Some(3000);
    rig.save_recording(&json!({"telemetry": {"enabled": true}, "video": {"enabled": true}}));
    let m = rig.build();
    let hooks = hooks_for(m.clone(), &rig);
    hooks.recording_start().await;
    m.settled().await;
    // Boot defer armed on a 3 A budget: the hint comes from the snapshot.
    let hints = hooks.report_hints();
    assert_eq!(hints.boot_defer.as_deref(), Some("deferring"));
    assert_eq!(hints.power_shed, None);
    assert_eq!(hints.clock_stepper_armed, None);
    let k0 = rig.log.kicks.load(Ordering::SeqCst);
    hooks.notify_connectivity(true);
    hooks.notify_connectivity(false);
    assert_eq!(rig.log.kicks.load(Ordering::SeqCst), k0 + 1);
    assert_eq!(rig.log.pauses.load(Ordering::SeqCst), 1);
    let st = hooks.recording_status();
    assert!(st.enabled);
    assert_eq!(st.boot_defer.as_deref(), Some("deferring"));
    hooks.note_recording_status(&st);
    assert_eq!(hooks.recording_status_for_led(), st);
    // Bridge-only capture: media.recording false, the stream annotated false.
    let media = TelemetryMedia {
        kvs: BoolLeaf { value: true },
        recording: BoolLeaf { value: false },
        streams: vec![
            MediaStream {
                stream_key: "cam0".into(),
                channel_name: "dev-1-cam0".into(),
                state: "running".into(),
                recording: None,
            },
            MediaStream {
                stream_key: "cam9".into(),
                channel_name: "dev-1-cam9".into(),
                state: "running".into(),
                recording: None,
            },
        ],
    };
    let out = hooks.overlay_media(media.clone());
    assert_eq!(
        serde_json::to_string(&out).unwrap(),
        r#"{"kvs":{"value":true},"recording":{"value":false},"streams":[{"streamKey":"cam0","channelName":"dev-1-cam0","state":"running","recording":false},{"streamKey":"cam9","channelName":"dev-1-cam9","state":"running"}]}"#
    );
    hooks.recording_stop().unwrap();
    let fin = hooks.final_recording_status().unwrap();
    assert!(!fin.enabled);
    assert_eq!(fin.session_id, None);
    assert_eq!(fin.boot_defer.as_deref(), Some("deferring"));
}

/// `worker.ts:1200-1221`: `media.recording` = any capture recording; only
/// streams under capture supervision get the flag.
#[test]
fn overlay_flags_only_supervised_streams() {
    let media = TelemetryMedia {
        kvs: BoolLeaf { value: true },
        recording: BoolLeaf { value: false },
        streams: ["cam0", "cam1", "cam2"]
            .iter()
            .map(|k| MediaStream {
                stream_key: k.to_string(),
                channel_name: format!("c-{k}"),
                state: "running".into(),
                recording: None,
            })
            .collect(),
    };
    let video = VideoRecordingStatus {
        recording: true,
        streams: vec![
            CaptureStatus {
                stream_key: "cam0".into(),
                state: "running".into(),
                session_id: Some("s".into()),
                recording: true,
            },
            CaptureStatus {
                stream_key: "cam1".into(),
                state: "backoff".into(),
                session_id: None,
                recording: false,
            },
        ],
    };
    let out = xorgate_agent::recording::hooks::overlay(media, &video);
    assert!(out.recording.value);
    let flags: Vec<Option<bool>> = out.streams.iter().map(|s| s.recording).collect();
    assert_eq!(flags, vec![Some(true), Some(false), None]);
}

/// P9 / review-wp3 #5: `report_hints` (called under ConfigSync's mutex) and
/// the tick's reads never take the manager's state lock — they return while
/// an `apply` is blocked inside its live-parking callback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_never_block_on_a_manager_mid_apply() {
    let mut rig = Rig::new("nodl");
    rig.budget_ma = Some(3000);
    *rig.gnss.lock().unwrap() = Some(GnssView {
        fix: false,
        sky: SkyState::Clear,
    });
    std::fs::write(
        rig.telemetry_path(),
        r#"{"timestamp":"2026-10-10T00:00:00.000Z","status":"connected","reg":1}"#,
    )
    .unwrap();
    rig.save_recording(&json!({"video": {"enabled": true, "gpsGate": "always"}}));
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let entered_tx = Mutex::new(entered_tx);
    rig.live_shed_hook = Some(Arc::new(move |_s: bool| {
        let _ = entered_tx.lock().unwrap().send(());
        let _ = release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10));
    }));
    let m = rig.build();
    let hooks = Arc::new(hooks_for(m.clone(), &rig));
    let m2 = m.clone();
    let start = tokio::spawn(async move { m2.start().await });
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("apply reached setLiveShed");
    // apply() now holds the manager's state lock, blocked in the callback.
    let (done_tx, done_rx) = mpsc::channel();
    let (h, mm) = (hooks.clone(), m.clone());
    std::thread::spawn(move || {
        let hints = h.report_hints();
        let st = h.recording_status();
        let _ = mm.video_status();
        let _ = h.overlay_media(TelemetryMedia::idle());
        let _ = done_tx.send((hints, st));
    });
    let (hints, st) = done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("hooks returned while the manager was mid-apply (no deadlock)");
    // The snapshot taken mid-apply: the gate verdict was already published.
    assert_eq!(hints.boot_defer.as_deref(), Some("gating"));
    assert_eq!(st.boot_defer.as_deref(), Some("gating"));
    release_tx.send(()).unwrap();
    start.await.unwrap();
    assert_eq!(
        m.status().live_block.map(|b| b.reason).as_deref(),
        Some("gps-gate")
    );
    m.stop();
}

/// Invariant 6: the live masters are parked from exactly ONE place — the
/// manager's choke point (`decide_live` → `set_live_shed`) — and the media
/// supervisor's `set_shed` is reached only through that callback.
#[test]
fn live_parking_has_a_single_call_site() {
    let src = format!("{}/src", env!("CARGO_MANIFEST_DIR"));
    let mut set_live_shed_calls = Vec::new();
    let mut set_shed_calls = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(&src)];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            for (n, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                let at = format!("{}:{}", p.strip_prefix(&src).unwrap().display(), n + 1);
                if code.contains(".set_live_shed {") || code.contains("&vo.set_live_shed") {
                    set_live_shed_calls.push(at.clone());
                }
                // `media/` is the actuator itself (WP5's supervisor handing
                // the call to its pure ladder), not a caller of it.
                if code.contains(".set_shed(") && !at.starts_with("media/") {
                    set_shed_calls.push(at);
                }
            }
        }
    }
    assert_eq!(
        set_live_shed_calls.len(),
        1,
        "set_live_shed invoked outside the choke point: {set_live_shed_calls:?}"
    );
    assert!(set_live_shed_calls[0].starts_with("recording/manager.rs"));
    assert_eq!(
        set_shed_calls.len(),
        1,
        "MediaControl::set_shed called outside the worker's setLiveShed closure: {set_shed_calls:?}"
    );
    assert!(
        set_shed_calls[0].starts_with("worker.rs"),
        "{set_shed_calls:?}"
    );
}

/// CF39 / review-wp3 #4: every uploadBucket re-arm re-runs
/// `buildUploadOptions` (never a cached manifest) and hands the result to
/// `setUpload` — `None` (still no bucket) keeps the current uploader.
#[tokio::test(start_paused = true)]
async fn rearm_rebuilds_upload_options_every_time() {
    let mut rig = Rig::new("rearm");
    rig.video = false;
    let m = rig.build();
    let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let b2 = builds.clone();
    let hooks = RecordingHooks::new(
        None,
        None,
        Arc::new(move || {
            let n = b2.fetch_add(1, Ordering::SeqCst);
            (n > 0).then(|| xorgate_cloud::s3::UploadOptions {
                put: Arc::new(NoopPut),
                is_online: Arc::new(|| true),
            })
        }),
        rig.clock.clone(),
    );
    hooks.install(m.clone());
    hooks.recording_start().await;
    hooks.rearm_upload();
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert_eq!(rig.log.uploaders.load(Ordering::SeqCst), 0, "no bucket yet");
    hooks.rearm_upload();
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    assert_eq!(rig.log.uploaders.load(Ordering::SeqCst), 1, "armed live");
    hooks.rearm_upload();
    assert_eq!(rig.log.uploaders.load(Ordering::SeqCst), 2);
    assert_eq!(
        rig.log.uploader_stops.load(Ordering::SeqCst),
        1,
        "old one replaced"
    );
    m.stop();
}

/// A reconciler returning a scripted result (review-wp5 M1).
struct ScriptedReconciler(Mutex<Vec<Result<Vec<String>, String>>>);

impl xorgate_agent::recording::hooks::HardwareReconciler for ScriptedReconciler {
    fn reconcile(&self) -> xorgate_agent::ctx::BoxFut<'_, Result<Vec<String>, String>> {
        let r = self.0.lock().unwrap().remove(0);
        Box::pin(async move { r })
    }
}

/// Records `restart_channel` calls.
#[derive(Default)]
struct BounceLog(Mutex<Vec<String>>);

impl xorgate_agent::ctx::MediaControl for BounceLog {
    fn read(&self) -> TelemetryMedia {
        TelemetryMedia::idle()
    }
    fn stop(&self) {}
    fn restart_channel(&self, key: &str) {
        self.0.lock().unwrap().push(key.to_string());
    }
    fn set_shed(&self, _shed: bool) {}
    fn set_disabled_streams(&self, _keys: &[String]) {}
}

/// `worker.ts:427` + `:843-850` (review-wp5 M1): a throwing boot reconcile
/// is fatal (`reconcile_hardware_boot` → `Err`, the worker exits non-zero); a throwing
/// cloud-change reconcile is caught — no master bounce, no `reapply` — while
/// a good one bounces the changed streams, then reapplies.
#[tokio::test(start_paused = true)]
async fn hardware_reconcile_failure_paths() {
    let mut rig = Rig::new("hw-fail");
    rig.video = false;
    let m = rig.build();
    let media = Arc::new(BounceLog::default());
    let hw = Arc::new(ScriptedReconciler(Mutex::new(vec![
        Err("EACCES: permission denied, mkdir '/x'".into()),
        Err("ENOSPC: no space left on device, write".into()),
        Ok(vec!["cam0".into()]),
    ])));
    let hooks = RecordingHooks::new(
        Some(media.clone()),
        Some(hw),
        Arc::new(|| None),
        rig.clock.clone(),
    );
    // Boot: the error surfaces to the worker.
    assert_eq!(
        hooks.reconcile_hardware_boot().await,
        Err("EACCES: permission denied, mkdir '/x'".to_string())
    );
    hooks.install(m.clone());
    hooks.recording_start().await;
    assert!(!m.status().enabled);

    // Cloud change, failing pass: settings now enable telemetry, but the
    // caught failure skips `reapply` — nothing starts, nothing bounces.
    rig.save_recording(&json!({"telemetry": {"enabled": true}}));
    hooks.hardware_changed();
    yields().await;
    assert!(media.0.lock().unwrap().is_empty());
    assert!(
        !m.status().enabled,
        "reapply skipped after a failed reconcile"
    );

    // A good pass: bounce cam0, then reapply (the recorder starts).
    hooks.hardware_changed();
    yields().await;
    assert_eq!(*media.0.lock().unwrap(), vec!["cam0".to_string()]);
    assert!(m.status().enabled);
    m.stop();
}

/// One PUT the production uploader sent.
struct LoggingPut(Arc<Mutex<Vec<(String, String)>>>);

impl xorgate_cloud::s3::ObjectPut for LoggingPut {
    fn put<'a>(
        &'a self,
        key: &'a str,
        _body: bytes::Bytes,
        content_type: &'a str,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> xorgate_cloud::s3::PutFuture<'a> {
        self.0
            .lock()
            .unwrap()
            .push((key.to_string(), content_type.to_string()));
        Box::pin(async { Ok(()) })
    }
}

/// The production backend's uploader (WP3 `build_uploader`, `manager.ts:246-272`):
/// built at construction from the boot upload options, kicked by `start()`,
/// it drains a leftover telemetry segment under `<root>/telemetry` with the
/// TS key; `isOnline` is the MQTT gate only (review-wp3 S1) — offline, nothing
/// is PUT, and a reconnect (`notify_connectivity(true)`) kicks it again.
#[tokio::test(start_paused = true)]
async fn production_backend_uploads_through_wp3() {
    let mut rig = Rig::new("prod-upload");
    rig.video = false;
    let puts = Arc::new(Mutex::new(Vec::new()));
    let online = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let o2 = online.clone();
    rig.upload_override = Some(xorgate_cloud::s3::UploadOptions {
        put: Arc::new(LoggingPut(puts.clone())),
        is_online: Arc::new(move || o2.load(Ordering::SeqCst)),
    });
    let probe: xorgate_agent::recording::time_source::ProbeFn =
        Arc::new(|| Box::pin(async { xorgate_core::session::ProbedTimeSource::Ntp }));
    rig.backend_override = Some(Arc::new(xorgate_agent::recording::Backend {
        clock: rig.clock.clone(),
        tape: xorgate_hal::io::null_tape(),
        home: None,
        time_source: xorgate_agent::recording::time_source::TimeSourceHandle::create(
            60_000,
            probe,
            rig.clock.clone(),
        ),
    }));
    let sid = "0198aaaa-0000-7000-8000-000000000001";
    let dir = rig.root().join("telemetry").join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    let seg = dir.join("seg-1753000000000-00003.jsonl.gz");
    std::fs::write(&seg, b"x").unwrap();
    let m = rig.build();
    m.start().await;
    for _ in 0..5 {
        yields().await;
    }
    assert!(puts.lock().unwrap().is_empty(), "offline: no PUT");
    assert!(seg.exists());
    online.store(true, Ordering::SeqCst);
    m.notify_connectivity(true);
    for _ in 0..10 {
        yields().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        *puts.lock().unwrap(),
        vec![(
            format!("telemetry/v1/dev-1/{sid}/1753000000000-00003.jsonl.gz"),
            "application/gzip".to_string()
        )]
    );
    assert!(!seg.exists(), "deleted after the confirmed PUT");
    m.stop();
}

/// review-wp2 N5 (2): the probe outliving the 2 s bound is NOT cancelled —
/// the TS race leaves the `refresh()` promise running — so the tracker still
/// flips to the probed source after the bound.
#[tokio::test(start_paused = true)]
async fn a_slow_probe_keeps_running_past_the_bound() {
    let mut rig = Rig::new("slowprobe");
    rig.video = false;
    let ts = Arc::new(FakeTimeSource {
        value: Mutex::new(TimeSource::Unsynced),
        probed: TimeSource::Ntp,
        hang: false,
        delay_ms: 5_000,
    });
    rig.time_source = ts.clone();
    let m = rig.build();
    let t0 = tokio::time::Instant::now();
    m.start().await;
    assert_eq!(t0.elapsed(), Duration::from_millis(2_000));
    assert_eq!(*ts.value.lock().unwrap(), TimeSource::Unsynced);
    tokio::time::sleep(Duration::from_millis(3_001)).await;
    assert_eq!(
        *ts.value.lock().unwrap(),
        TimeSource::Ntp,
        "probe not cancelled"
    );
    m.stop();
}

const ENOSPC: &str = "ENOSPC: no space left on device, write";

/// The `[agent]`/`[recording]` log lines emitted on this thread while the
/// guard lives (the tests run on tokio's current-thread runtime).
fn capture_logs() -> (Arc<Mutex<Vec<String>>>, tracing::subscriber::DefaultGuard) {
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    struct Msg(String);
    impl Visit for Msg {
        fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
            if f.name() == "message" {
                self.0 = format!("{v:?}");
            }
        }
        fn record_str(&mut self, f: &Field, v: &str) {
            if f.name() == "message" {
                self.0 = v.to_string();
            }
        }
    }
    struct Capture(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, ev: &tracing::Event<'_>, _: Context<'_, S>) {
            let mut m = Msg(String::new());
            ev.record(&mut m);
            if m.0.starts_with("[agent]") || m.0.starts_with("[recording]") {
                self.0.lock().unwrap().push(m.0);
            }
        }
    }
    let logs = Arc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::registry().with(Capture(logs.clone()));
    (logs, tracing::subscriber::set_default(sub))
}

/// review-wp2 N5 (1) + review-wp1 M1/S1: a throwing finalize aborts
/// `stopRecorder` and the rest of `apply()`, and `reapply` returns the `Err`.
/// The recorder stays in the manager with no `stopped` event, and the restart
/// at the new rate does not run. The kept recorder is DEAD: WP2's `stop()`
/// set `stopped` and cleared its timer before the finalize threw
/// (`telemetry-recorder.ts:75-81`). It samples nothing, while `status()`
/// still reports the old session. Only a later settings change whose
/// finalize succeeds replaces it. This end state is the TS's on the caught
/// call sites (config message, hardware change). The 5 s poll exits instead
/// (`the_poll_path_hands_a_failed_finalize_to_the_worker`).
#[tokio::test(start_paused = true)]
async fn a_failed_finalize_aborts_stop_recorder() {
    let mut rig = Rig::new("finfail");
    rig.video = false;
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 20}}));
    let m = rig.build();
    m.start().await;
    let sid = m.status().session_id.unwrap();
    rig.log.recorder_stop_failures.store(1, Ordering::SeqCst);
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 10}}));
    assert_eq!(m.reapply(), Err(ENOSPC.to_string()));
    assert!(m.status().enabled);
    assert_eq!(m.status().session_id.as_deref(), Some(sid.as_str()));
    assert_eq!(rig.log.recorders.lock().unwrap().len(), 1, "no restart");
    assert_eq!(rig.log.recorder_stops.load(Ordering::SeqCst), 0);
    assert!(!rig.events().iter().any(|e| e.contains("stopped")));
    assert!(
        rig.log.recorder_stopped.lock().unwrap()[0].load(Ordering::SeqCst),
        "the kept recorder is stopped: it no longer samples"
    );
    // TS quirk kept: `lastSettingsKey` was taken BEFORE the throwing apply,
    // so unchanged settings never retry. The poll only re-runs the video diff,
    // and the dead recorder stays in place.
    m.reapply().unwrap();
    assert_eq!(rig.log.recorder_stops.load(Ordering::SeqCst), 0);
    assert_eq!(rig.log.recorders.lock().unwrap().len(), 1);
    // The next settings change stops (finalize OK) and restarts at its rate.
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 5}}));
    m.reapply().unwrap();
    assert_eq!(rig.log.recorder_stops.load(Ordering::SeqCst), 1);
    assert_eq!(rig.log.recorders.lock().unwrap().len(), 2);
    assert_eq!(rig.log.recorders.lock().unwrap()[1].rate_hz, 5);
    assert_eq!(
        rig.events()[1],
        "telemetry recording stopped (rateHz changed)"
    );
    m.stop();
}

/// review-wp1 M1, 5 s poll call site (a local `xorgate set`/TUI edit): the
/// TS `setInterval(() => this.reapply())` throw is uncaught, so Node exits 1
/// (`manager.ts:319`; only `unhandledRejection` is netted, `worker.ts:656`).
/// Here the poll stops and the error reaches the worker's fatal channel. The
/// worker logs `[agent] fatal: <msg>` and exits 1 through its normal shutdown
/// (`plane.rs recording_fatal_runs_the_normal_shutdown_with_exit_1`).
#[tokio::test(start_paused = true)]
async fn the_poll_path_hands_a_failed_finalize_to_the_worker() {
    let mut rig = Rig::new("finfail-poll");
    rig.video = false;
    rig.poll_ms = 5_000;
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 20}}));
    let m = rig.build();
    let mut fatal = m.take_fatal().expect("the worker takes the receiver");
    assert!(m.take_fatal().is_none(), "taken once");
    m.start().await;
    rig.log.recorder_stop_failures.store(1, Ordering::SeqCst);
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 10}}));
    assert!(fatal.try_recv().is_err());
    tokio::time::sleep(Duration::from_millis(5_001)).await;
    yields().await;
    assert_eq!(fatal.try_recv().unwrap(), ENOSPC);
    assert!(rig.log.recorder_stopped.lock().unwrap()[0].load(Ordering::SeqCst));
    // The poll is stopped (the TS process is gone): a later change is not
    // applied by the poll and nothing more is sent.
    rig.save_recording(&json!({"telemetry": {"enabled": false}}));
    tokio::time::sleep(Duration::from_millis(20_000)).await;
    yields().await;
    assert_eq!(rig.log.recorder_stops.load(Ordering::SeqCst), 0);
    assert!(fatal.try_recv().is_err());
    // The worker's shutdown still runs `stop()`, which retries the finalize.
    m.stop();
    assert_eq!(rig.log.recorder_stops.load(Ordering::SeqCst), 1);
}

/// review-wp1 M1, config-message call site, the manager half: the hook
/// returns the manager's `Err` to the config plane, which logs `cloud config
/// apply failed` and records the warn event
/// (`config_message.rs a_throwing_recording_reapply_lands_in_cloud_config_apply_failed`).
#[tokio::test(start_paused = true)]
async fn the_config_hook_returns_a_failed_finalize() {
    let mut rig = Rig::new("finfail-cfg");
    rig.video = false;
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 20}}));
    let m = rig.build();
    let hooks = hooks_for(m.clone(), &rig);
    hooks.recording_start().await;
    rig.log.recorder_stop_failures.store(1, Ordering::SeqCst);
    rig.save_recording(&json!({"telemetry": {"enabled": false}}));
    assert_eq!(hooks.reapply_recording(), Err(ENOSPC.to_string()));
    // The process keeps running (caught in the TS): no fatal message.
    let mut fatal = m.take_fatal().unwrap();
    assert!(fatal.try_recv().is_err());
    assert!(
        hooks.reapply_recording().is_ok(),
        "unchanged settings: no retry"
    );
    m.stop();
}

/// review-wp1 M1, hardware-change call site: `recording.reapply()` inside the
/// `.then` of `worker.ts:845-847` throws, the chain rejects, and the `.catch`
/// logs `[agent] hardware reconcile failed <msg>`. The bounce already
/// happened, and the process keeps running (no fatal message).
#[tokio::test(start_paused = true)]
async fn a_failed_finalize_after_a_hardware_change_is_caught() {
    let mut rig = Rig::new("finfail-hw");
    rig.video = false;
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 20}}));
    let m = rig.build();
    let mut fatal = m.take_fatal().unwrap();
    let media = Arc::new(BounceLog::default());
    let hw = Arc::new(ScriptedReconciler(Mutex::new(vec![Ok(
        vec!["cam0".into()],
    )])));
    let hooks = RecordingHooks::new(
        Some(media.clone()),
        Some(hw),
        Arc::new(|| None),
        rig.clock.clone(),
    );
    hooks.install(m.clone());
    hooks.recording_start().await;
    rig.log.recorder_stop_failures.store(1, Ordering::SeqCst);
    rig.save_recording(&json!({"telemetry": {"enabled": false}}));
    let (logs, guard) = capture_logs();
    hooks.hardware_changed();
    yields().await;
    drop(guard);
    let logs = logs.lock().unwrap().clone();
    assert_eq!(*media.0.lock().unwrap(), vec!["cam0".to_string()]);
    assert!(
        logs.iter()
            .any(|l| l == &format!("[agent] hardware reconcile failed {ENOSPC}")),
        "{logs:?}"
    );
    assert!(m.status().enabled, "the recorder is kept (dead)");
    assert!(fatal.try_recv().is_err());
    m.stop();
}

/// D29 (wp1.md), shutdown call site: in the TS a throwing finalize escapes
/// `recording.stop()` and the signal handler (`worker.ts:1306`), so Node
/// exits 1 before `videoCapture.stop()` and the offline publish. Rust logs
/// `[agent] fatal: <msg>` and still SIGINTs capture and finishes `stop()`.
/// The process is exiting anyway, and capture still stops cleanly.
/// `stop_checked` (the `recording_stop` hook) returns the error so the
/// shutdown ends the MQTT link the crash way (review-wp1 X1).
#[tokio::test(start_paused = true)]
async fn a_failed_finalize_at_shutdown_still_stops_capture() {
    let rig = Rig::new("finfail-stop");
    rig.save_recording(
        &json!({"telemetry": {"enabled": true}, "video": {"enabled": true, "bootDefer": "off"}}),
    );
    let m = rig.build();
    m.start().await;
    m.settled().await;
    assert_eq!(rig.live_cams(), vec!["cam0"]);
    rig.log.recorder_stop_failures.store(1, Ordering::SeqCst);
    let (logs, guard) = capture_logs();
    let res = m.stop_checked();
    drop(guard);
    assert_eq!(res, Err(ENOSPC.to_string()));
    let logs = logs.lock().unwrap().clone();
    assert_eq!(logs, vec![format!("[agent] fatal: {ENOSPC}")]);
    assert_eq!(rig.live_cams(), Vec::<String>::new(), "capture stopped");
    assert_eq!(rig.log.recorder_stops.load(Ordering::SeqCst), 0);
    assert!(!rig.events().iter().any(|e| e.contains("agent shutdown")));
    assert_eq!(m.video_key(), "");
    let mut fatal = m.take_fatal().unwrap();
    assert!(fatal.try_recv().is_err(), "no second exit path");
}

/// D41 agent-exit cleanup (ruling 2026-11-06): the capture the shutdown
/// `stop()` retired is exit-killed ONCE; before any stop there is nothing to
/// clean; a second call does nothing; the journal line reports the segments.
#[tokio::test(start_paused = true)]
async fn the_shutdown_capture_gets_the_exit_cleanup_once() {
    let rig = Rig::new("exit-reap");
    rig.save_recording(
        &json!({"telemetry": {"enabled": true}, "video": {"enabled": true, "bootDefer": "off"}}),
    );
    let m = rig.build();
    m.start().await;
    m.settled().await;
    assert_eq!(rig.live_cams(), vec!["cam0"]);
    m.reap_capture_at_exit(Duration::from_millis(500)).await;
    assert!(
        rig.log.exit_kills.lock().unwrap().is_empty(),
        "no stop yet: nothing retired"
    );
    m.stop();
    let (logs, guard) = capture_logs();
    m.reap_capture_at_exit(Duration::from_millis(500)).await;
    m.reap_capture_at_exit(Duration::from_millis(500)).await;
    drop(guard);
    assert_eq!(rig.log.exit_kills.lock().unwrap().len(), 1);
    let logs = logs.lock().unwrap().clone();
    assert!(
        logs.iter().any(|l| l.starts_with(
            "[recording] capture cam0 still running at exit: killed, 1 shm segment(s) unlinked"
        )),
        "{logs:?}"
    );
}

/// review-wp5-r2 R-WP5R-3: a config cut (preset change) hands the old
/// supervisor to a pending video transition; its child is still in its EOS
/// (`stop_and_wait` pending) when SIGTERM arrives. The exit cleanup must cover
/// that RETIRING supervisor too (and the transition must not start the new
/// one after shutdown).
#[tokio::test(start_paused = true)]
async fn a_supervisor_inside_a_pending_transition_gets_the_exit_cleanup() {
    let rig = Rig::new("exit-reap-retiring");
    rig.save_recording(
        &json!({"telemetry": {"enabled": true}, "video": {"enabled": true, "bootDefer": "off", "preset": "720p15-1mbps"}}),
    );
    let m = rig.build();
    m.start().await;
    m.settled().await;
    assert_eq!(rig.live_cams(), vec!["cam0"]);
    rig.log.hang_stop_and_wait.store(true, Ordering::SeqCst);
    // The config cut: preset change → Replace { prev } → prev.stop_and_wait hangs.
    rig.save_recording(
        &json!({"telemetry": {"enabled": true}, "video": {"enabled": true, "bootDefer": "off", "preset": "1080p15-2mbps"}}),
    );
    m.reapply().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let captures_before = rig.log.captures.lock().unwrap().len();
    // SIGTERM while the old child is still in its EOS.
    m.stop();
    let (logs, guard) = capture_logs();
    m.reap_capture_at_exit(Duration::from_millis(500)).await;
    drop(guard);
    assert_eq!(
        *rig.log.exit_kills.lock().unwrap(),
        vec!["720p15-1mbps".to_string()],
        "the retiring (old-preset) supervisor was exit-killed"
    );
    let logs = logs.lock().unwrap().clone();
    assert!(
        logs.iter().any(|l| l.starts_with(
            "[recording] capture cam0 still running at exit: killed, 1 shm segment(s) unlinked"
        )),
        "{logs:?}"
    );
    // The released transition does not start the new preset after shutdown.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.log.captures.lock().unwrap().len(), captures_before);
    // Once its stop_and_wait returned it left the retiring list: a second
    // cleanup has nothing to kill.
    m.reap_capture_at_exit(Duration::from_millis(500)).await;
    assert_eq!(rig.log.exit_kills.lock().unwrap().len(), 1);
}

/// R-WP5R-3, the `StopAll` arm: every camera disabled hands the old supervisor
/// to a pending `StopAndWait`; its child is still in its EOS at SIGTERM — the
/// exit cleanup must cover it too.
#[tokio::test(start_paused = true)]
async fn a_supervisor_inside_a_pending_stop_all_gets_the_exit_cleanup() {
    let rig = Rig::new("exit-reap-stopall");
    rig.save_recording(
        &json!({"telemetry": {"enabled": true}, "video": {"enabled": true, "bootDefer": "off", "preset": "720p15-1mbps"}}),
    );
    let m = rig.build();
    m.start().await;
    m.settled().await;
    rig.log.hang_stop_and_wait.store(true, Ordering::SeqCst);
    // Every camera disabled → no camera left → StopAll (video:false alone
    // would keep a bridge-only Replace).
    rig.save_recording(
        &json!({"telemetry": {"enabled": true}, "video": {"enabled": true, "bootDefer": "off", "preset": "720p15-1mbps", "cameras": {"cam0": false}}}),
    );
    m.reapply().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    m.stop();
    m.reap_capture_at_exit(Duration::from_millis(500)).await;
    assert_eq!(
        *rig.log.exit_kills.lock().unwrap(),
        vec!["720p15-1mbps".to_string()],
        "the supervisor inside the pending StopAndWait was exit-killed"
    );
}

/// review-wp5-r2 R-WP5R-6: every SIGKILL first, then ONE wait with a TOTAL
/// bound — two killed children (two cameras) whose exits are never observed
/// cost 500 ms in all, not 500 ms each; both are reported.
#[tokio::test(start_paused = true)]
async fn the_exit_cleanup_waits_once_with_a_total_bound() {
    let mut rig = Rig::new("exit-reap-total");
    rig.cameras = vec!["cam0".into(), "cam1".into()];
    rig.save_recording(
        &json!({"telemetry": {"enabled": true}, "video": {"enabled": true, "bootDefer": "off", "spawnStaggerSecs": 0}}),
    );
    let m = rig.build();
    m.start().await;
    m.settled().await;
    assert_eq!(rig.live_cams(), vec!["cam0", "cam1"]);
    rig.log.exit_never.store(true, Ordering::SeqCst);
    m.stop();
    let (logs, guard) = capture_logs();
    let t0 = tokio::time::Instant::now();
    m.reap_capture_at_exit(Duration::from_millis(500)).await;
    let took = t0.elapsed();
    drop(guard);
    assert_eq!(
        took,
        Duration::from_millis(500),
        "one TOTAL bound for both children"
    );
    let logs = logs.lock().unwrap().clone();
    for cam in ["cam0", "cam1"] {
        assert!(
            logs.iter().any(|l| l.starts_with(&format!("[recording] capture {cam} still running at exit"))),
            "{logs:?}"
        );
    }
}

/// review-wp2 N1 (manager side): `onSegmentClosed: () => void this.runEnforce()`
/// runs `setActivePath` + the disk's synchronous scan prefix INSIDE the
/// callback (the rotation), not one task hop later — including when the
/// finalize of `stopRecorder` fires it while the manager's state is locked
/// (no deadlock). The recorder is never called back from its own callback
/// (WP2 fires it under the recorder's lock): the active path is `None`
/// there, as the TS `activePath()` is null at that instant. Other callers
/// (the 60 s timer, `onUploaded`, capture) read `this.recorder.activePath()`.
#[tokio::test(start_paused = true)]
async fn segment_closed_runs_the_enforce_prefix_synchronously() {
    let mut rig = Rig::new("segclosed");
    rig.video = false;
    rig.save_recording(&json!({"telemetry": {"enabled": true, "rateHz": 20}}));
    let m = rig.build();
    m.start().await;
    let sid = m.status().session_id.unwrap();
    let base = rig.log.enforces.load(Ordering::SeqCst);
    let closed = rig.log.segment_closed.lock().unwrap()[0].clone();
    closed();
    // No await in between: the prefix already ran.
    assert_eq!(rig.log.enforces.load(Ordering::SeqCst), base + 1);
    assert_eq!(rig.log.active_paths.lock().unwrap().last().unwrap(), &None);
    // The tail then notifies (and would kick an uploader).
    let statuses = rig.log.statuses.lock().unwrap().len();
    yields().await;
    assert!(rig.log.statuses.lock().unwrap().len() > statuses);

    // The 60 s timer's pass protects the recorder's active segment.
    tokio::time::sleep(Duration::from_millis(60_000)).await;
    yields().await;
    assert_eq!(rig.log.enforces.load(Ordering::SeqCst), base + 2);
    let active = Some(
        rig.root()
            .join("telemetry")
            .join(&sid)
            .join("seg-active.jsonl"),
    );
    assert_eq!(
        rig.log.active_paths.lock().unwrap().last().unwrap(),
        &active
    );

    // Fired from inside stop() (state lock held) — no deadlock.
    rig.log.fire_closed_on_stop.store(true, Ordering::SeqCst);
    rig.save_recording(&json!({"telemetry": {"enabled": false}}));
    m.reapply().unwrap();
    assert_eq!(rig.log.enforces.load(Ordering::SeqCst), base + 3);
    assert_eq!(rig.log.active_paths.lock().unwrap().last().unwrap(), &None);
    assert!(!m.status().enabled);
    m.stop();
}

/// The production backend's video pieces (WP4): orphan recovery over a
/// missing tree is 0 (no tree yet, `video-segments.ts:341-345`), and a
/// manager with video wired but no camera builds no capture supervisor.
#[tokio::test(start_paused = true)]
async fn production_backend_video_pieces_are_inert_without_a_tree_or_camera() {
    use xorgate_agent::recording::manager::RecordingBackend;
    let rig = Rig::new("prod-video");
    let probe: xorgate_agent::recording::time_source::ProbeFn =
        Arc::new(|| Box::pin(async { xorgate_core::session::ProbedTimeSource::Ntp }));
    let b = xorgate_agent::recording::Backend {
        clock: rig.clock.clone(),
        tape: xorgate_hal::io::null_tape(),
        home: Some(rig.home.clone()),
        time_source: xorgate_agent::recording::time_source::TimeSourceHandle::create(
            60_000,
            probe,
            rig.clock.clone(),
        ),
    };
    assert_eq!(
        b.recover_orphan_video_sessions(&rig.root().join("video")),
        0
    );
    let mut rig = Rig::new("prod-video-mgr");
    rig.cameras = vec![];
    rig.backend_override = Some(Arc::new(b));
    let m = rig.build();
    m.start().await;
    yields().await;
    assert!(m.video_status().streams.is_empty());
    assert!(!m.video_status().recording);
    m.stop();
}
