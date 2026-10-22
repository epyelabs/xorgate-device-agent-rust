//! The REAL recording stack behind the spine (WP2 recorder + disk, WP3
//! uploader) with fake inputs only: sample_groups, the PUT seam, the
//! MQTT-connected gate, a fixed time-source probe, paused tokio time.
//!
//! - CF34: the recorder samples whether or not MQTT is connected — the
//!   `sample_groups()` calls (which keep the gps collector, hence the
//!   `gnss-sky.json` cadence, alive) continue with MQTT down;
//! - a clean `stop()` finalizes the active segment to `.jsonl.gz`
//!   (`manager.ts:351-365`) and its `onSegmentClosed` → `runEnforce` runs
//!   the disk prefix inside the finalize without deadlocking the manager;
//! - offline nothing is PUT; on reconnect (`notifyConnectivity(true)`) the
//!   finalized segment goes up under `telemetry/v1/<dev>/<sid>/<ms>-<seq>.jsonl.gz`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::json;
use xorgate_agent::ctx::{AgentClock, GroupSample};
use xorgate_agent::recording::time_source::{ProbeFn, TimeSourceHandle};
use xorgate_agent::recording::{Backend, RecordingManager, RecordingManagerOpts};
use xorgate_core::session::ProbedTimeSource;
use xorgate_core::telemetry::{MetricGroup, MetricLeaf};
use xorgate_hal::boot_sources::FileBootSources;
use xorgate_hal::io::null_tape;

struct LoggingPut(Arc<Mutex<Vec<String>>>);

impl xorgate_cloud::s3::ObjectPut for LoggingPut {
    fn put<'a>(
        &'a self,
        key: &'a str,
        _body: bytes::Bytes,
        _content_type: &'a str,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> xorgate_cloud::s3::PutFuture<'a> {
        self.0.lock().unwrap().push(key.to_string());
        Box::pin(async { Ok(()) })
    }
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[tokio::test(start_paused = true)]
async fn real_stack_records_offline_finalizes_and_uploads_on_reconnect() {
    let home = std::env::temp_dir().join(format!("xg-wp1-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let root = home.join("recordings");
    let settings = home.join("settings.json");
    std::fs::write(
        &settings,
        serde_json::to_vec(&json!({"recording": {"telemetry": {"enabled": true, "rateHz": 1}}}))
            .unwrap(),
    )
    .unwrap();

    let clock = Arc::new(AgentClock::following_tokio(1_760_000_000_000));
    let probe: ProbeFn = Arc::new(|| Box::pin(async { ProbedTimeSource::Ntp }));
    let ts = TimeSourceHandle::create(60_000, probe, clock.clone());
    let samples = Arc::new(AtomicUsize::new(0));
    let s2 = samples.clone();
    let puts = Arc::new(Mutex::new(Vec::new()));
    let online = Arc::new(AtomicBool::new(false));
    let o2 = online.clone();

    let m = RecordingManager::new(RecordingManagerOpts {
        device_id: "dev-1".into(),
        root: root.clone(),
        shm_dir: home.join("shm"),
        settings_path: settings,
        time_source: Arc::new(ts.clone()),
        upload: Some(xorgate_cloud::s3::UploadOptions {
            put: Arc::new(LoggingPut(puts.clone())),
            is_online: Arc::new(move || o2.load(Ordering::SeqCst)),
        }),
        video: None,
        sample_groups: Arc::new(move || {
            let n = s2.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let mut fields = MetricGroup::new();
                fields.insert("lat".into(), MetricLeaf::new(45.0 + n as f64 * 1e-5));
                let mut out = IndexMap::new();
                out.insert(
                    "gps".to_string(),
                    GroupSample {
                        fields,
                        updated_mono: tokio::time::Instant::now(),
                    },
                );
                out
            })
        }),
        power_sample: None,
        boot_sources: Box::new(FileBootSources {
            budget_path: home.join("no-max-current"),
            telemetry_file: home.join("no-telemetry.json"),
            tape: null_tape(),
            gnss: None,
        }),
        on_status: Arc::new(|_| {}),
        on_event: Arc::new(|_, _| {}),
        clock: clock.clone(),
        backend: Arc::new(Backend {
            clock: clock.clone(),
            tape: null_tape(),
            home: Some(home.clone()),
            time_source: ts,
        }),
        poll_ms: None,
        shed_sample_ms: None,
        time_probe_timeout_ms: None,
        clock_trusted: None,
    });
    m.start().await;
    let sid = m.status().session_id.expect("recording");

    // MQTT down the whole time: the recorder still samples (CF34).
    let before = samples.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(10)).await;
    let during = samples.load(Ordering::SeqCst) - before;
    assert!(during >= 9, "sampled {during}× in 10 s offline");
    assert!(puts.lock().unwrap().is_empty());

    // Past the 60 s rotation age (mono follows tokio): one closed .jsonl.gz
    // plus the new active plain segment; still offline → nothing PUT.
    tokio::time::sleep(Duration::from_secs(55)).await;
    let dir = root.join("telemetry").join(&sid);
    let gz: Vec<PathBuf> = files_under(&dir)
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".jsonl.gz"))
        .collect();
    assert_eq!(gz.len(), 1, "{:?}", files_under(&dir));
    assert!(puts.lock().unwrap().is_empty(), "offline: no PUT");

    // Reconnect: the uploader drains the closed segment, then deletes it.
    online.store(true, Ordering::SeqCst);
    m.notify_connectivity(true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let name = gz[0].file_name().unwrap().to_string_lossy().into_owned();
    let stem = name.trim_start_matches("seg-");
    assert_eq!(
        *puts.lock().unwrap(),
        vec![format!("telemetry/v1/dev-1/{sid}/{stem}")]
    );
    assert!(!gz[0].exists());

    // Clean stop: the active segment is finalized to .jsonl.gz (its
    // onSegmentClosed → runEnforce prefix runs inside the finalize).
    online.store(false, Ordering::SeqCst);
    m.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let left = files_under(&dir);
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].to_string_lossy().ends_with(".jsonl.gz"), "{left:?}");
    assert!(!m.status().enabled);
    let _ = std::fs::remove_dir_all(&home);
}
