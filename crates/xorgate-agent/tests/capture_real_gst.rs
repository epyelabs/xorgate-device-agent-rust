//! WP4 integration probe against a REAL `gst-launch-1.0` (ignored by
//! default: needs gstreamer1.0-tools + plugins good/bad/ugly). The supervisor
//! runs its real record argv with the head swapped to
//! `videotestsrc is-live=true pattern=ball` through the TS's own seam
//! (`XORGATE_CAPTURE_SOURCE_HEAD`), writes real splitmuxsink fragments, and
//! the probe prints, per sidecar, `startTs − (gst "New clock:" wall time)`
//! and the inter-fragment spacing — the ~2.1 s GOP lag (memory
//! `video-sidecar-gop-latency`) must be REPRODUCED (phase-3 D8).
//!
//!   XORGATE_CAPTURE_SOURCE_HEAD='videotestsrc is-live=true pattern=ball' \
//!     cargo test -p xorgate-agent --test capture_real_gst -- --ignored --nocapture

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use xorgate_agent::ctx::AgentClock;
use xorgate_agent::recording::capture::*;
use xorgate_core::types::TimeSource;
use xorgate_core::video_presets::resolve_video_preset;
use xorgate_hal::child::ChildEvent;

/// GstLaunchBackend with a tap that stamps the wall time of "New clock:".
struct Tap {
    inner: GstLaunchBackend,
    playing: Arc<Mutex<Vec<i64>>>,
    argv: Arc<Mutex<Vec<Vec<String>>>>,
}

impl CaptureBackend for Tap {
    fn program(&self) -> &str {
        self.inner.program()
    }
    fn spawn(
        &self,
        args: &[String],
    ) -> (Box<dyn CaptureChild>, mpsc::UnboundedReceiver<ChildEvent>) {
        self.argv.lock().unwrap().push(args.to_vec());
        let (child, mut rx) = self.inner.spawn(args);
        let (tx, out) = mpsc::unbounded_channel();
        let playing = self.playing.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let ChildEvent::Stdout(l) = &ev {
                    if l.contains("New clock:") {
                        playing.lock().unwrap().push(xorgate_agent::now_ms());
                    }
                }
                let _ = tx.send(ev);
            }
        });
        (child, out)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real gst-launch-1.0 with x264enc/splitmuxsink/shmsink"]
async fn real_gst_sidecar_timing() {
    let home = std::env::temp_dir().join(format!("xg-realgst-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let playing = Arc::new(Mutex::new(Vec::new()));
    let argv = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let seg: u64 = std::env::var("WP4_SEGMENT_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let run_secs: u64 = std::env::var("WP4_RUN_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(35);
    let mut o = VideoCaptureOpts::new(
        "dev-1",
        vec![CaptureCamera {
            stream_key: "cam0".into(),
            camera_name: "/base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a".into(),
        }],
        true,
        resolve_video_preset("720p15-1mbps").clone(),
        seg,
        home.join("recordings/video"),
        home.join("shm"),
        "boot-1",
        Arc::new(|| TimeSource::Ntp),
        Arc::new(AgentClock::system()),
    );
    o.backend = Arc::new(Tap {
        inner: GstLaunchBackend::default(),
        playing: playing.clone(),
        argv: argv.clone(),
    });
    let ev = events.clone();
    o.on_event = Some(Arc::new(move |_l, m| {
        ev.lock().unwrap().push(m.to_string())
    }));
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let sid = sup.status()[0].session_id.clone().unwrap();
    tokio::time::sleep(Duration::from_secs(run_secs)).await;
    sup.stop_and_wait(Duration::from_secs(10)).await;

    println!("ARGV gst-launch-1.0 {}", argv.lock().unwrap()[0].join(" "));
    println!("EVENTS {:?}", events.lock().unwrap());
    let playing_at = *playing.lock().unwrap().first().expect("New clock: line");
    println!("PLAYING wall {playing_at}");
    let dir: PathBuf = home.join("recordings/video/cam0").join(&sid);
    let mut prev: Option<f64> = None;
    let mut n = 0;
    for i in 0.. {
        let p = dir.join(format!("{i:05}.json"));
        if !p.exists() {
            break;
        }
        let sc: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let start = sc["startTs"].as_f64().unwrap();
        let birth = std::fs::metadata(dir.join(format!("{i:05}.mp4")))
            .ok()
            .and_then(|m| m.created().ok())
            .map(xorgate_hal::fragment_stat::ms_from_system_time)
            .unwrap_or(0.0);
        println!(
            "FRAG {i} anchor={} finalized={} startTs-playing={:+.0}ms spacing={} birth-playing={:+.0}ms dur={} size={}",
            sc["anchor"],
            sc["finalized"],
            start - playing_at as f64,
            prev.map_or("-".into(), |p| format!("{:.0}ms", start - p)),
            birth - playing_at as f64,
            sc["durationMs"],
            sc["sizeBytes"]
        );
        prev = Some(start);
        n += 1;
    }
    println!(
        "SIDECAR {}",
        std::fs::read_to_string(dir.join("00000.json")).unwrap_or_default()
    );
    println!(
        "SESSION {}",
        std::fs::read_to_string(dir.join("session.json")).unwrap_or_default()
    );
    assert!(n >= 2, "expected ≥2 fragments, got {n}");
    let _ = std::fs::remove_dir_all(&home);
}
