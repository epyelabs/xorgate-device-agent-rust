//! Phase 7a WP3 (Rig form): the supervisor tapes the rig-only
//! `capture_timing` record for every host timing line it parses in
//! `XORGATE_SIDECAR_TIMING=exact` — start lines with lagMs + startTs, END
//! lines with endTs, each with the run's table verdict (`accepted`), the
//! child pid and the session id — and nothing in `legacy`. A line that is
//! not a timing line, or a timing prefix with an unusable body, tapes no
//! record (its id is unknown).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use xorgate_agent::ctx::AgentClock;
use xorgate_agent::recording::capture::*;
use xorgate_core::capture_timing::{FragmentEnd, FragmentStart, VERSION};
use xorgate_core::types::TimeSource;
use xorgate_core::video_presets::resolve_video_preset;
use xorgate_hal::child::{ChildDone, ChildEvent};
use xorgate_hal::io::{CaptureTimingRec, TapeSink};

const T0: u64 = 1_760_000_000_000;

struct Entry {
    tx: mpsc::UnboundedSender<ChildEvent>,
    exited: AtomicBool,
}

struct FakeChild(Arc<Entry>);

impl CaptureChild for FakeChild {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn signal(&self, _sig: i32) {
        if !self.0.exited.swap(true, Ordering::SeqCst) {
            let _ = self.0.tx.send(ChildEvent::Done(ChildDone {
                exit_code: Some(0),
                ..ChildDone::default()
            }));
        }
    }
    fn renice(&self, _nice: i32) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct FakeHost {
    spawns: Mutex<Vec<Arc<Entry>>>,
}

impl CaptureBackend for FakeHost {
    fn program(&self) -> &str {
        "xorgate-capture"
    }
    fn spawn(
        &self,
        _args: &[String],
    ) -> (Box<dyn CaptureChild>, mpsc::UnboundedReceiver<ChildEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let e = Arc::new(Entry {
            tx,
            exited: AtomicBool::new(false),
        });
        self.spawns.lock().unwrap().push(e.clone());
        (Box::new(FakeChild(e)), rx)
    }
    fn emits_timing(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct RecTape(Mutex<Vec<CaptureTimingRec>>);

impl TapeSink for RecTape {
    fn capture_timing(&self, r: &CaptureTimingRec) {
        self.0.lock().unwrap().push(r.clone());
    }
}

fn start_line(k: u32, lag: u64) -> String {
    let frame = T0 + 300 + u64::from(k) * 60_000;
    FragmentStart {
        v: VERSION,
        id: k,
        frame_mono_ns: 1_000_000_000_000,
        cb_mono_ns: 1_000_000_000_000 + lag * 1_000_000,
        cb_wall_ms: frame + lag,
        lag_ms: lag,
        start_ts: frame,
    }
    .to_line()
}

async fn run(timing: SidecarTiming, tag: &str) -> (Vec<CaptureTimingRec>, String) {
    let home = std::env::temp_dir().join(format!("xg-tt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let be = Arc::new(FakeHost::default());
    let tape = Arc::new(RecTape::default());
    let mut o = VideoCaptureOpts::new(
        "dev-1",
        vec![CaptureCamera {
            stream_key: "cam0".into(),
            camera_name: "/base/cam0@1a".into(),
        }],
        true,
        resolve_video_preset("720p15-1mbps").clone(),
        60,
        home.join("video"),
        home.join("shm"),
        "boot-1",
        Arc::new(|| TimeSource::Ntp),
        Arc::new(AgentClock::following_tokio(T0 as i64)),
    );
    o.backend = be.clone();
    o.fs_watch = false;
    o.stable_ms = 50;
    o.shm_segment_dir = home.join("devshm");
    o.sidecar_timing = timing;
    o.tape = tape.clone();
    let s = VideoCaptureSupervisor::new(o);
    s.start();
    let sid = s.status()[0].session_id.clone().expect("session");
    let c = be.spawns.lock().unwrap().last().unwrap().clone();
    for l in [
        "Setting pipeline to PLAYING ...".to_string(),
        start_line(0, 12),
        start_line(1, 9_000), // out of the guard: refused by the table
        "XG-FRAGMENT {\"v\":1,\"id\":".to_string(), // unusable body: no record
        FragmentEnd {
            v: VERSION,
            id: 0,
            end_mono_ns: 1,
            cb_mono_ns: 2,
            cb_wall_ms: T0 + 9_000,
            end_ts: T0 + 9_000,
        }
        .to_line(),
    ] {
        let _ = c.tx.send(ChildEvent::Stdout(l));
    }
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(1)).await;
    s.stop();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let _ = std::fs::remove_dir_all(&home);
    let recs = tape.0.lock().unwrap().clone();
    (recs, sid)
}

#[tokio::test(start_paused = true)]
async fn exact_tapes_every_parsed_timing_line_with_the_tables_verdict() {
    let (recs, sid) = run(SidecarTiming::Exact, "exact").await;
    let rec = |id: u32, lag: Option<u64>, start: Option<u64>, end: Option<u64>, accepted: bool| {
        CaptureTimingRec {
            key: "cam0".into(),
            pid: 0,
            sid: sid.clone(),
            id,
            lag_ms: lag,
            start_ts: start,
            end_ts: end,
            accepted,
        }
    };
    assert_eq!(
        recs,
        vec![
            rec(0, Some(12), Some(T0 + 300), None, true),
            rec(1, Some(9_000), Some(T0 + 60_300), None, false),
            rec(0, None, None, Some(T0 + 9_000), true),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn legacy_tapes_no_capture_timing_record() {
    let (recs, _) = run(SidecarTiming::Legacy, "legacy").await;
    assert!(recs.is_empty(), "{recs:?}");
}
