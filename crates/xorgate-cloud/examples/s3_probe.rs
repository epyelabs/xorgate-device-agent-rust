//! C9 bench probe (phase-2 WP5): fetch IoT role-alias credentials over mTLS
//! and PUT ONE sidecar-shaped JSON object (no mp4 → no ingest) through the
//! exact production path — `build_upload_options` → `S3Put` →
//! `IotCredentialsProvider::get` → `s3_put`.
//!
//! Env (the dev identity's, e.g. `set -a; . /etc/xorgate/agent-rs-dev.env`):
//!   DEVICE_ID              thing name (== deviceId)
//!   CERT_PATH / KEY_PATH   device cert + key (PEM)
//!   KVS_MANIFEST           kvs.json with credentialEndpoint/roleAlias/region/uploadBucket
//!   XORGATE_UPLOAD_BUCKET  optional override (as in the agent)
//!
//! Prints `key=<s3 key>` and `bucket=<bucket>` on success (exit 0); exits 1
//! with the TS error text otherwise.
//!
//!   cargo build --release --example s3_probe
//!   target/release/examples/s3_probe

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use xorgate_cloud::s3::{build_upload_options, UploadEnv};
use xorgate_core::kvs_manifest::upload_bucket_decision;
use xorgate_core::sidecar::{SidecarAnchor, SidecarCodec, VideoSidecar};
use xorgate_core::types::TimeSource;
use xorgate_hal::kvs_manifest_io::load_manifest;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// RFC 9562 UUIDv7 (48-bit ms timestamp, version 7, variant 10, random rest).
fn uuid_v7(now_ms: u64) -> String {
    let mut r = [0u8; 10];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut r);
    }
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&now_ms.to_be_bytes()[2..]);
    b[6..].copy_from_slice(&r);
    b[6] = (b[6] & 0x0f) | 0x70;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber_init();
    let device_id = env("DEVICE_ID").expect("DEVICE_ID");
    let cert = PathBuf::from(env("CERT_PATH").expect("CERT_PATH"));
    let key = PathBuf::from(env("KEY_PATH").expect("KEY_PATH"));
    let manifest_path = PathBuf::from(env("KVS_MANIFEST").expect("KVS_MANIFEST"));
    let env_bucket = env("XORGATE_UPLOAD_BUCKET");

    let manifest = load_manifest(&manifest_path);
    let bucket = upload_bucket_decision(manifest.as_ref(), env_bucket.as_deref());
    let Some(upload) = build_upload_options(
        &UploadEnv {
            offline: false,
            env_upload_bucket: env_bucket.as_deref(),
            thing_name: &device_id,
            cert_path: &cert,
            key_path: &key,
        },
        || manifest.clone(),
        Arc::new(|| true),
    ) else {
        eprintln!("upload disabled: {bucket:?}");
        std::process::exit(1);
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let session = uuid_v7(now_ms);
    let s3_key = format!("media-sessions/v1/{device_id}/cam0/{session}/00000.json");
    let sidecar = VideoSidecar {
        v: 1,
        device_id: device_id.clone(),
        stream_key: "cam0".into(),
        session_id: session.clone(),
        seq: 0.0,
        start_ts: now_ms as f64,
        start_mono: 0.0,
        duration_ms: 2000.0,
        size_bytes: 0.0,
        time_source: TimeSource::Ntp,
        codec: SidecarCodec::H264,
        width: 1280.0,
        height: 720.0,
        fps: 15.0,
        bitrate_kbps: 2000.0,
        anchor: SidecarAnchor::Computed,
        finalized: true,
    };
    let body = Bytes::from(serde_json::to_vec(&sidecar).expect("sidecar json"));
    let t0 = std::time::Instant::now();
    match upload
        .put
        .put(&s3_key, body, "application/json", &CancellationToken::new())
        .await
    {
        Ok(()) => {
            println!("bucket={}", bucket.unwrap_or_default());
            println!("key={s3_key}");
            println!("ok in {} ms", t0.elapsed().as_millis());
        }
        Err(e) => {
            eprintln!("put failed: {e}");
            std::process::exit(1);
        }
    }
}

/// INFO+ (`[agent] segment upload disabled (…)` and friends) to stderr;
/// hyper/reqwest debug chatter is dropped.
fn tracing_subscriber_init() {
    struct Stderr;
    impl tracing::Subscriber for Stderr {
        fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
            *m.level() <= tracing::Level::INFO
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, e: &tracing::Event<'_>) {
            struct V(String);
            impl tracing::field::Visit for V {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    if f.name() == "message" {
                        self.0 = format!("{v:?}");
                    }
                }
            }
            let mut v = V(String::new());
            e.record(&mut v);
            eprintln!("{}", v.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    let _ = tracing::subscriber::set_global_default(Stderr);
}
