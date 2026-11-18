//! D86 (F-DA21-KVSLOOP, Rust-only): `MediaLadder::set_source_absent`, the
//! KVS master gate over the capture supervisor's start-failure back-off.
//! The TS has no such input (accepted-divergences D86), so these are plain
//! unit tests; with no gate ever set the ladder is the TS's
//! (`media_ladder_fixtures.rs` still passes byte for byte).

use std::cell::RefCell;
use std::collections::HashMap;

use xorgate_core::kvs_manifest::{KvsChannel, KvsManifest};
use xorgate_core::media_ladder::*;
use xorgate_core::stream_guard::StreamGuard;

struct World {
    guard: RefCell<HashMap<String, StreamGuard>>,
}

impl World {
    fn new() -> Self {
        Self {
            guard: RefCell::new(HashMap::new()),
        }
    }
}

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

/// The capture parks cam1: its live master is SIGINTed and HELD (no
/// restart timer; its SIGINT exit restarts nothing); cam0 is untouched.
#[test]
fn source_down_signals_and_holds_only_that_master() {
    let w = World::new();
    let (mut l, fx) = started(&w);
    assert_eq!(spawns(&fx), ["dev-cam0", "dev-cam1"]);

    let fx = l.set_source_absent("cam1", true, 6_000, &w);
    assert_eq!(signals(&fx), ["dev-cam1"]);
    assert!(spawns(&fx).is_empty() && arms(&fx).is_empty());
    assert_eq!(
        lines(&fx),
        ["[media dev-cam1] capture source down (camera absent or failing); master held until it is up"]
    );
    assert_eq!(state(&l, "dev-cam1"), "failed");
    assert_eq!(state(&l, "dev-cam0"), "starting");

    // The SIGINTed master exits: no restart ladder, no line, still failed.
    let fx = l.on_exit("dev-cam1", Some(0), 6_050);
    assert!(fx.is_empty(), "{fx:?}");
    assert_eq!(state(&l, "dev-cam1"), "failed");
    // A crash of the held channel's master after a long run: same.
    assert!(l.on_exit("dev-cam1", Some(1), 60_000).is_empty());

    // The snapshot reports it truthfully with the existing states.
    let snap = l.snapshot();
    let st: Vec<(&str, &str)> = snap
        .streams
        .iter()
        .map(|s| (s.stream_key.as_str(), s.state.as_str()))
        .collect();
    assert_eq!(st, [("cam0", "starting"), ("cam1", "failed")]);
    assert!(snap.kvs.value, "cam0 is still live");

    // A repeated `down` is a no-op (one line per transition).
    assert!(l.set_source_absent("cam1", true, 7_000, &w).is_empty());
}

/// While held, nothing respawns it: the capture's probe spawns bounce
/// nothing (no handle), the stale stable/restart timers are dead, and
/// the power-policy release / settings re-enable re-check the gate.
#[test]
fn a_held_master_never_restart_loops() {
    let w = World::new();
    let (mut l, fx) = started(&w);
    let stable_cam1 = arms(&fx)
        .into_iter()
        .find(|a| a.0 == "dev-cam1")
        .expect("stable timer");
    let _ = l.set_source_absent("cam1", true, 6_000, &w);
    let _ = l.on_exit("dev-cam1", Some(0), 6_010);

    assert!(l.restart_channel("cam1").is_empty(), "no handle: no bounce");
    let fx = l.on_timer(&stable_cam1.0, stable_cam1.1, stable_cam1.2, 15_000, &w);
    assert!(fx.is_empty(), "the cleared stable timer is dead");
    assert_eq!(state(&l, "dev-cam1"), "failed");

    // Shed + release: cam0 respawns (after its own exit), cam1 stays held.
    let _ = l.set_shed(true, 20_000, &w);
    let _ = l.on_exit("dev-cam0", Some(0), 20_010);
    assert_eq!(state(&l, "dev-cam1"), "stopped");
    let fx = l.set_shed(false, 30_000, &w);
    assert_eq!(spawns(&fx), ["dev-cam0"]);
    assert_eq!(state(&l, "dev-cam1"), "failed");

    // Disabled + re-enabled in settings: still held.
    let _ = l.set_disabled_streams(&["cam1".to_string()], 40_000, &w);
    assert_eq!(state(&l, "dev-cam1"), "stopped");
    let fx = l.set_disabled_streams(&[], 50_000, &w);
    assert!(spawns(&fx).is_empty(), "{fx:?}");
    assert_eq!(state(&l, "dev-cam1"), "failed");
}

/// The source comes back (a probe ran stable): the master starts at once
/// with a fresh ladder, and a later quick crash walks the TS ladder again.
#[test]
fn source_up_starts_the_master_with_a_fresh_ladder() {
    let w = World::new();
    let (mut l, _) = started(&w);
    let _ = l.set_source_absent("cam1", true, 6_000, &w);
    let _ = l.on_exit("dev-cam1", Some(0), 6_010);

    let fx = l.set_source_absent("cam1", false, 400_000, &w);
    assert_eq!(spawns(&fx), ["dev-cam1"]);
    assert_eq!(
        lines(&fx),
        ["[media dev-cam1] capture source up; starting master"]
    );
    assert_eq!(state(&l, "dev-cam1"), "starting");
    // Fresh ladder: the first quick crash retries at the 2 s base.
    let fx = l.on_exit("dev-cam1", Some(1), 401_000);
    assert_eq!(
        lines(&fx),
        ["[media dev-cam1] master exited (code 1); retry 1/3 in 2000ms"]
    );
    // A repeated `up` is a no-op.
    assert!(l.set_source_absent("cam1", false, 402_000, &w).is_empty());
    // `up` for a key that was never down: nothing.
    assert!(l.set_source_absent("cam0", false, 402_000, &w).is_empty());
}

/// `up` while the SIGINTed master has not exited yet: no double spawn;
/// its exit then takes the normal TS path.
#[test]
fn source_up_before_the_held_master_exited_does_not_double_spawn() {
    let w = World::new();
    let (mut l, _) = started(&w);
    let fx = l.set_source_absent("cam1", true, 20_000, &w);
    assert_eq!(signals(&fx), ["dev-cam1"]);
    let fx = l.set_source_absent("cam1", false, 20_001, &w);
    assert!(spawns(&fx).is_empty(), "the handle is still live");
    let fx = l.on_exit("dev-cam1", Some(0), 20_002);
    assert_eq!(
        lines(&fx),
        ["[media dev-cam1] master exited (code 0); restarting"],
        "ran ≥ 15 s → the TS prompt restart"
    );
    assert_eq!(arms(&fx).len(), 1);
}

/// Flags set before the lazy start are remembered (like disabled
/// streams); a guard park keeps its own state and slow retry; shed and
/// disabled channels stay `stopped`; after `stop()` the gate is inert.
#[test]
fn before_start_guard_parks_shed_disabled_and_stop() {
    let w = World::new();
    let mut l = MediaLadder::new(LadderOpts::default());
    assert!(
        l.set_source_absent("cam1", true, 0, &w).is_empty(),
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
    assert_eq!(state(&l, "dev-cam1"), "failed");

    // A guard park (no-camera) is not overwritten.
    let w2 = World::new();
    w2.guard
        .borrow_mut()
        .insert("cam1".into(), StreamGuard::NoCamera);
    let (mut l2, fx) = started(&w2);
    assert_eq!(spawns(&fx), ["dev-cam0"]);
    assert_eq!(state(&l2, "dev-cam1"), "no-camera");
    assert!(l2.set_source_absent("cam1", true, 1_000, &w2).is_empty());
    assert_eq!(state(&l2, "dev-cam1"), "no-camera");

    // Disabled: recorded only.
    let w3 = World::new();
    let (mut l3, _) = started(&w3);
    let _ = l3.set_disabled_streams(&["cam1".to_string()], 1_000, &w3);
    let _ = l3.on_exit("dev-cam1", Some(0), 1_010);
    assert!(l3.set_source_absent("cam1", true, 2_000, &w3).is_empty());
    assert_eq!(state(&l3, "dev-cam1"), "stopped");

    // Stopped: inert.
    let _ = l3.stop();
    assert!(l3.set_source_absent("cam0", true, 3_000, &w3).is_empty());
}
