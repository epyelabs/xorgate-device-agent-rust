//! D86 (F-DA21-KVSLOOP, Rust-only): `MediaLadder::set_source_absent`, the
//! KVS master gate over the capture supervisor's start-failure back-off.
//! The TS has no such input (accepted-divergences D86), so these are plain
//! unit tests; with no gate ever set the ladder is the TS's
//! (`media_ladder_fixtures.rs` still passes byte for byte).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use xorgate_core::kvs_manifest::{KvsChannel, KvsManifest};
use xorgate_core::media_ladder::*;
use xorgate_core::stream_guard::StreamGuard;

struct World {
    guard: RefCell<HashMap<String, StreamGuard>>,
    seq: Cell<u64>,
}

impl World {
    fn new() -> Self {
        Self {
            guard: RefCell::new(HashMap::new()),
            seq: Cell::new(0),
        }
    }
    /// The next edge sequence number (the agent's `next_source_seq`).
    fn seq(&self) -> u64 {
        self.seq.set(self.seq.get() + 1);
        self.seq.get()
    }
}

/// `set_source_absent` with a fresh (newest) sequence number.
fn sa(l: &mut MediaLadder, w: &World, key: &str, absent: bool, now: u64) -> Vec<Effect> {
    let seq = w.seq();
    l.set_source_absent(key, absent, seq, now, w)
}

const DOWN: &str =
    "[media dev-cam1] capture source down (camera absent or failing); master held until it is up";

impl SpawnEnv for World {
    fn guard(&self, stream_key: &str) -> StreamGuard {
        *self
            .guard
            .borrow()
            .get(stream_key)
            .unwrap_or(&StreamGuard::Ok)
    }
    fn resolve_pipeline(&self, stream_key: &str) -> Option<String> {
        Some(format!("/p/{stream_key}.gst"))
    }
}

fn manifest() -> KvsManifest {
    KvsManifest {
        device_id: "dev".into(),
        region: "us-east-1".into(),
        credential_endpoint: "c.example".into(),
        role_alias: "r".into(),
        channels: ["cam0", "cam1"]
            .iter()
            .map(|k| KvsChannel {
                stream_key: (*k).into(),
                channel_name: format!("dev-{k}"),
            })
            .collect(),
        upload_bucket: None,
    }
}

fn started(w: &World) -> (MediaLadder, Vec<Effect>) {
    let mut l = MediaLadder::new(LadderOpts::default());
    assert!(l.read().kick_start);
    let fx = l.start(
        &RuntimeDetect {
            available: true,
            gstreamer: true,
            reason: None,
        },
        || Some(manifest()),
        0,
        w,
    );
    (l, fx)
}

fn spawns(fx: &[Effect]) -> Vec<&str> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::Spawn { channel, .. } => Some(channel.as_str()),
            _ => None,
        })
        .collect()
}

fn signals(fx: &[Effect]) -> Vec<&str> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::Signal { channel } => Some(channel.as_str()),
            _ => None,
        })
        .collect()
}

fn arms(fx: &[Effect]) -> Vec<(String, TimerKind, u64, u64)> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::Arm {
                channel,
                kind,
                id,
                delay_ms,
            } => Some((channel.clone(), *kind, *id, *delay_ms)),
            _ => None,
        })
        .collect()
}

fn lines(fx: &[Effect]) -> Vec<String> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::Log { line, .. } => Some(line.clone()),
            _ => None,
        })
        .collect()
}

fn state(l: &MediaLadder, ch: &str) -> &'static str {
    l.state_of(ch).expect("channel").as_str()
}

/// The capture parks cam1: its live master is SIGINTed and HELD (its exit
/// restarts nothing; only a 60 s guard re-check timer); cam0 is untouched.
#[test]
fn source_down_signals_and_holds_only_that_master() {
    let w = World::new();
    let (mut l, fx) = started(&w);
    assert_eq!(spawns(&fx), ["dev-cam0", "dev-cam1"]);

    let fx = sa(&mut l, &w, "cam1", true, 6_000);
    assert_eq!(signals(&fx), ["dev-cam1"]);
    assert!(spawns(&fx).is_empty() && arms(&fx).is_empty());
    assert_eq!(lines(&fx), [DOWN]);
    assert_eq!(state(&l, "dev-cam1"), "failed");
    assert_eq!(state(&l, "dev-cam0"), "starting");

    // The SIGINTed master exits: no restart ladder, no line, a 60 s re-check.
    let fx = l.on_exit("dev-cam1", Some(0), 6_050);
    assert!(lines(&fx).is_empty() && spawns(&fx).is_empty(), "{fx:?}");
    let a = arms(&fx);
    assert_eq!(a.len(), 1);
    assert_eq!((a[0].1, a[0].3), (TimerKind::Restart, PARKED_RETRY_MS));
    assert_eq!(state(&l, "dev-cam1"), "failed");

    let snap = l.snapshot();
    let st: Vec<(&str, &str)> = snap
        .streams
        .iter()
        .map(|s| (s.stream_key.as_str(), s.state.as_str()))
        .collect();
    assert_eq!(st, [("cam0", "starting"), ("cam1", "failed")]);
    assert!(snap.kvs.value, "cam0 is still live");

    // A repeated `down` is a no-op (one line per transition).
    assert!(sa(&mut l, &w, "cam1", true, 7_000).is_empty());
}

/// While held nothing respawns it: capture bounces, the dead stable timer,
/// the 60 s re-check (re-arms, never spawns), shed + release, and disable
/// + re-enable (the TS line gains `(held: …)`, review N1).
#[test]
fn a_held_master_never_restart_loops() {
    let w = World::new();
    let (mut l, fx) = started(&w);
    let stable_cam1 = arms(&fx)
        .into_iter()
        .find(|a| a.0 == "dev-cam1")
        .expect("stable timer");
    let _ = sa(&mut l, &w, "cam1", true, 6_000);
    let fx = l.on_exit("dev-cam1", Some(0), 6_010);
    let mut recheck = arms(&fx).remove(0);

    assert!(l.restart_channel("cam1").is_empty(), "no handle: no bounce");
    let fx = l.on_timer(&stable_cam1.0, stable_cam1.1, stable_cam1.2, 15_000, &w);
    assert!(fx.is_empty(), "the cleared stable timer is dead");
    for t in [66_010, 126_010] {
        let fx = l.on_timer(&recheck.0, recheck.1, recheck.2, t, &w);
        assert!(spawns(&fx).is_empty() && lines(&fx).is_empty(), "{fx:?}");
        recheck = arms(&fx).remove(0);
        assert_eq!(state(&l, "dev-cam1"), "failed");
    }

    // Shed + release: cam0 respawns (after its own exit), cam1 stays held.
    let _ = l.set_shed(true, 130_000, &w);
    let _ = l.on_exit("dev-cam0", Some(0), 130_010);
    assert_eq!(state(&l, "dev-cam1"), "stopped");
    let fx = l.set_shed(false, 140_000, &w);
    assert_eq!(spawns(&fx), ["dev-cam0"]);
    assert!(lines(&fx).contains(&DOWN.to_string()), "held again: {fx:?}");
    assert_eq!(state(&l, "dev-cam1"), "failed");

    // Disabled + re-enabled in settings: still held, and the line says so.
    let _ = l.set_disabled_streams(&["cam1".to_string()], 150_000, &w);
    assert_eq!(state(&l, "dev-cam1"), "stopped");
    let fx = l.set_disabled_streams(&[], 160_000, &w);
    assert!(spawns(&fx).is_empty(), "{fx:?}");
    assert_eq!(
        lines(&fx),
        [
            "[media dev-cam1] camera re-enabled in settings; starting master (held: capture source down)",
            DOWN
        ]
    );
    assert_eq!(state(&l, "dev-cam1"), "failed");
}

/// The source comes back: the held master starts at once with a fresh
/// ladder (its pending re-check timer is cleared).
#[test]
fn source_up_starts_the_master_with_a_fresh_ladder() {
    let w = World::new();
    let (mut l, _) = started(&w);
    let _ = sa(&mut l, &w, "cam1", true, 6_000);
    let fx = l.on_exit("dev-cam1", Some(0), 6_010);
    let recheck = arms(&fx).remove(0);

    let fx = sa(&mut l, &w, "cam1", false, 400_000);
    assert_eq!(spawns(&fx), ["dev-cam1"]);
    assert_eq!(
        lines(&fx),
        ["[media dev-cam1] capture source up; starting master"]
    );
    assert_eq!(state(&l, "dev-cam1"), "starting");
    assert!(
        l.on_timer(&recheck.0, recheck.1, recheck.2, 400_001, &w)
            .is_empty(),
        "the re-check timer was cleared"
    );
    let fx = l.on_exit("dev-cam1", Some(1), 401_000);
    assert_eq!(
        lines(&fx),
        ["[media dev-cam1] master exited (code 1); retry 1/3 in 2000ms"]
    );
    assert!(
        sa(&mut l, &w, "cam1", false, 402_000).is_empty(),
        "repeated up"
    );
    assert!(
        sa(&mut l, &w, "cam0", false, 402_000).is_empty(),
        "never down"
    );
}

/// `up` while the SIGINTed master has not exited yet: no spawn, no line;
/// its exit then takes the normal TS path.
#[test]
fn source_up_before_the_held_master_exited_does_not_double_spawn() {
    let w = World::new();
    let (mut l, _) = started(&w);
    let fx = sa(&mut l, &w, "cam1", true, 20_000);
    assert_eq!(signals(&fx), ["dev-cam1"]);
    let fx = sa(&mut l, &w, "cam1", false, 20_001);
    assert!(fx.is_empty(), "the handle is still live: no spawn, no line");
    let fx = l.on_exit("dev-cam1", Some(0), 20_002);
    assert_eq!(
        lines(&fx),
        ["[media dev-cam1] master exited (code 0); restarting"],
        "ran ≥ 15 s → the TS prompt restart"
    );
    assert_eq!(arms(&fx).len(), 1);
}

/// Review S2: an older edge delivered after a newer one is ignored.
#[test]
fn a_stale_edge_cannot_re_hold_a_released_master() {
    let w = World::new();
    let (mut l, _) = started(&w);
    let park = w.seq(); // the park's `true`, numbered first …
    let retire = w.seq(); // … the retire's `false`, numbered after it
    assert!(l
        .set_source_absent("cam1", false, retire, 7_000, &w)
        .is_empty());
    assert!(
        l.set_source_absent("cam1", true, park, 7_001, &w)
            .is_empty(),
        "the overtaken park edge is dropped"
    );
    assert_eq!(state(&l, "dev-cam1"), "starting", "never held");
}

/// Flags set before the lazy start are remembered; guard parks keep their
/// own state, and a guard that opens while the source is down holds the
/// channel WITH the down line (review N2); `up` into a guard that turned
/// `no-camera` prints no `up` line (review N1); disabled; after `stop()`.
#[test]
fn before_start_guard_parks_shed_disabled_and_stop() {
    let w = World::new();
    let mut l = MediaLadder::new(LadderOpts::default());
    assert!(
        sa(&mut l, &w, "cam1", true, 0).is_empty(),
        "no channels yet"
    );
    assert!(l.read().kick_start);
    let fx = l.start(
        &RuntimeDetect {
            available: true,
            gstreamer: true,
            reason: None,
        },
        || Some(manifest()),
        0,
        &w,
    );
    assert_eq!(spawns(&fx), ["dev-cam0"], "cam1 held from the start");
    assert!(lines(&fx).contains(&DOWN.to_string()));
    assert_eq!(state(&l, "dev-cam1"), "failed");

    // A guard park (no-camera) is not overwritten, and its release is silent.
    let w2 = World::new();
    w2.guard
        .borrow_mut()
        .insert("cam1".into(), StreamGuard::NoCamera);
    let (mut l2, fx) = started(&w2);
    let guard_timer = arms(&fx)
        .into_iter()
        .find(|a| a.0 == "dev-cam1")
        .expect("guard park timer");
    assert_eq!(state(&l2, "dev-cam1"), "no-camera");
    assert!(sa(&mut l2, &w2, "cam1", true, 1_000).is_empty());
    assert_eq!(state(&l2, "dev-cam1"), "no-camera");
    // N2: the guard opens while the source is down → held, WITH the line.
    w2.guard.borrow_mut().clear();
    let fx = l2.on_timer(&guard_timer.0, guard_timer.1, guard_timer.2, 60_000, &w2);
    assert!(spawns(&fx).is_empty());
    assert_eq!(lines(&fx), [DOWN]);
    assert_eq!(state(&l2, "dev-cam1"), "failed");
    // N1: up while the guard has turned no-camera again → no `up` line.
    w2.guard
        .borrow_mut()
        .insert("cam1".into(), StreamGuard::NoCamera);
    let fx = sa(&mut l2, &w2, "cam1", false, 61_000);
    assert!(spawns(&fx).is_empty());
    assert_eq!(
        lines(&fx),
        ["[media dev-cam1] not starting a master: no-camera (model expects a camera that did not enumerate)"]
    );
    assert_eq!(state(&l2, "dev-cam1"), "no-camera");

    // Disabled: recorded only.
    let w3 = World::new();
    let (mut l3, _) = started(&w3);
    let _ = l3.set_disabled_streams(&["cam1".to_string()], 1_000, &w3);
    let _ = l3.on_exit("dev-cam1", Some(0), 1_010);
    assert!(sa(&mut l3, &w3, "cam1", true, 2_000).is_empty());
    assert_eq!(state(&l3, "dev-cam1"), "stopped");

    // Stopped: inert.
    let _ = l3.stop();
    assert!(sa(&mut l3, &w3, "cam0", true, 3_000).is_empty());
}
