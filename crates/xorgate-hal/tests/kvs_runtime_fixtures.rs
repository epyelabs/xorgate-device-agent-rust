//! Byte test of the KVS master launch contract (`xorgate_hal::kvs_runtime`)
//! against `fixtures/media_ladder/kvs-runtime.json`: the REAL
//! `streamChannel` (`src/hardware/kvs-webrtc.ts:85-109`) launched a sh
//! "runtime" that printed its argv and the contract env keys
//! (`research/kvs-runtime-media-sim.md` §2), and `detectRuntime` (:56-77).
//!
//! Here the same script is spawned through `xorgate_hal::child` with the
//! spec `master_spec` builds, so the comparison is what the child SEES
//! (argv bytes, env values after the merge onto the inherited environment,
//! a drained stderr line, exit code) — not just the spec.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde_json::Value;
use xorgate_hal::child::{self, ChildEvent};
use xorgate_hal::kvs_runtime::{detect_runtime_with, master_spec, KvsRuntimeEnv};

const KEYS: &[&str] = &[
    "AWS_DEFAULT_REGION",
    "AWS_IOT_CORE_CREDENTIAL_ENDPOINT",
    "AWS_IOT_CORE_CERT",
    "AWS_IOT_CORE_PRIVATE_KEY",
    "AWS_IOT_CORE_ROLE_ALIAS",
    "AWS_IOT_CORE_THING_NAME",
    "AWS_KVS_CACERT_PATH",
    "AWS_KVS_LOG_LEVEL",
    "LD_LIBRARY_PATH",
    "KVS_GST_PIPELINE",
    "KVS_IDLE_TEARDOWN_SECS",
];

fn fixture() -> Value {
    let p = format!(
        "{}/../../fixtures/media_ladder/kvs-runtime.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-kvs-rt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The driver's fake master, byte for byte.
fn fake_master(dir: &std::path::Path) -> String {
    let mut s = String::from("#!/bin/sh\nfor a in \"$@\"; do printf 'ARG=%s\\n' \"$a\"; done\n");
    let lines: Vec<String> = KEYS
        .iter()
        .map(|k| format!("if [ -n \"${{{k}+x}}\" ]; then printf 'ENV {k}=%s\\n' \"${k}\"; fi"))
        .collect();
    s.push_str(&lines.join("\n"));
    s.push_str("\nprintf 'stderr line\\n' >&2\nexit 0\n");
    let p = dir.join("fake-master");
    std::fs::write(&p, s).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

#[tokio::test]
async fn stream_channel_argv_and_env_as_the_child_sees_them() {
    let f = fixture();
    let dir = scratch("sc");
    let bin = fake_master(&dir);
    let mut n = 0;
    for c in f["cases"].as_array().unwrap() {
        if c["op"] != "streamChannel" {
            continue;
        }
        n += 1;
        let i = &c["input"];
        let e = &i["env"];
        let env = KvsRuntimeEnv {
            region: e["region"].as_str().unwrap().into(),
            credential_endpoint: e["credentialEndpoint"].as_str().unwrap().into(),
            role_alias: e["roleAlias"].as_str().unwrap().into(),
            thing_name: e["thingName"].as_str().unwrap().into(),
            cert_path: e["certPath"].as_str().unwrap().into(),
            key_path: e["keyPath"].as_str().unwrap().into(),
            ca_cert_path: e["caCertPath"].as_str().unwrap().into(),
            lib_dir: e["libDir"].as_str().unwrap().into(),
            pipeline_path: opt_str(&e["pipelinePath"]),
            log_level: opt_str(&e["logLevel"]),
            idle_teardown_secs: opt_str(&e["idleTeardownSecs"]),
        };
        let spec = master_spec(
            i["channel"].as_str().unwrap(),
            i["mediaType"].as_str().unwrap(),
            i["source"].as_str().unwrap(),
            &bin,
            &env,
            i["inheritedLd"].as_str(),
        );
        // The spec sets ONLY contract keys.
        for (k, _) in &spec.env {
            assert!(KEYS.contains(&k.as_str()), "{}: stray env {k}", c["name"]);
        }
        let (_h, mut rx) = child::spawn(&spec);
        let (mut argv, mut seen, mut stderr) = (vec![], BTreeMap::new(), vec![]);
        let code = loop {
            match rx.recv().await.unwrap() {
                ChildEvent::Stdout(l) => {
                    if let Some(a) = l.strip_prefix("ARG=") {
                        argv.push(Value::from(a));
                    } else if let Some(kv) = l.strip_prefix("ENV ") {
                        let (k, v) = kv.split_once('=').unwrap();
                        // Inherited by the test process, not set by the spec: skip
                        // (the JS run had none of these in its environment).
                        if spec.env.iter().any(|(sk, _)| sk == k) {
                            seen.insert(k.to_string(), Value::from(v));
                        }
                    }
                }
                ChildEvent::Stderr(l) => stderr.push(Value::from(l)),
                ChildEvent::Done(d) => break d.exit_code,
            }
        };
        let want = &c["expected"];
        assert_eq!(Value::from(argv), want["argv"], "{}: argv", c["name"]);
        let want_env: BTreeMap<String, Value> = want["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(seen, want_env, "{}: env", c["name"]);
        assert_eq!(Value::from(stderr), want["stderr"], "{}: stderr", c["name"]);
        assert_eq!(Value::from(code), want["exitCode"], "{}: code", c["name"]);
    }
    assert_eq!(n, 6);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn detect_runtime_cases() {
    let f = fixture();
    let dir = scratch("dr");
    let bin = fake_master(&dir);
    std::fs::create_dir_all(dir.join("adir")).unwrap();
    let mut n = 0;
    for c in f["cases"].as_array().unwrap() {
        if c["op"] != "detectRuntime" {
            continue;
        }
        n += 1;
        let i = &c["input"];
        let js_dir = "/tmp/xg-kvs-runtime";
        let local = |p: &str| p.replace(js_dir, &dir.to_string_lossy());
        let path = local(i["binaryPath"].as_str().unwrap());
        let env_bin = i["kvsRuntimeBinEnv"].as_str().map(|_| bin.clone());
        // The JS container had no gst-launch-1.0 on PATH.
        let got = detect_runtime_with(
            &path,
            i["platform"].as_str().unwrap(),
            i["arch"].as_str().unwrap(),
            env_bin.as_deref(),
            || false,
        );
        let want = &c["expected"];
        assert_eq!(
            got.available,
            want["available"].as_bool().unwrap(),
            "{}",
            c["name"]
        );
        assert_eq!(
            got.gstreamer,
            want["gstreamer"].as_bool().unwrap(),
            "{}",
            c["name"]
        );
        assert_eq!(
            got.reason,
            want["reason"].as_str().map(local),
            "{}",
            c["name"]
        );
    }
    assert_eq!(n, 4);
    let _ = std::fs::remove_dir_all(&dir);
}
