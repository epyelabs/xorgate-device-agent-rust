//! Phase 4 WP2 — the AT port end to end over a REAL pty pair (E5's rig
//! half): `nix::pty::openpty`, the TS FakeModem (`support/fake_modem.rs`)
//! serving the master, and the Rust reader path on the slave via the
//! `XORGATE_SMS_PORT`-style override: `resolve_sms_port` → `stty -F <slave>
//! raw -echo min 0 time 1` (the real `stty`) → `open(O_RDWR|O_NOCTTY)` →
//! `sweep_transport` → close. The inbox is the fixture's `ptySpec` (bench
//! stores + a stored SUBMIT + a two-part UCS2 concat; text mode on entry).
//!
//! Expected bytes: `fixtures/sms_reader/pty-e2e.json` = what the REAL TS
//! `SmsReader` printed (`tools/replay/js/sms-pty-read.mts`, fixed clock;
//! re-dumped @ 09da38d in Phase 5b WP3 — the reply gains `action:"read"`)
//! against this same fake modem on a pty, plus the AT command log the modem
//! received and (since Phase 5b) EVERY raw byte written to it (`rawHex`). Regenerate (needs node + the 89b5c64 scratch build, e.g.
//! inside `xorgate-replay-rig:node20-bookworm`):
//!
//! ```text
//! XORGATE_SMS_PTY_JS='cd /js && npx --no-install tsx .rig-drivers/sms-pty-read.mts 50 1760000000000' \
//! XORGATE_SMS_PTY_OUT=/src/fixtures/sms_reader/pty-e2e.json  <this test binary>
//! ```
//!
//! The Rust side stamps the sweep exactly as `SmsReader.runSweep` does with
//! a fixed `now` (`finishedAt = now`, `durationMs = 0`) and prints
//! `JSON.stringify(buildSmsPayload(…, {deviceId:"local", requestId:"cli",
//! limit}), null, 2)`.

#[path = "support/fake_modem.rs"]
mod fake_modem;

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fake_modem::{FakeModem, FakeSpec, Reply, TestClock};
use serde_json::Value;
use xorgate_core::sms::{build_sms_payload, SmsPayloadRequest};
use xorgate_core::sms_cli::render_json;
use xorgate_hal::at_port::{
    open_serial_transport, resolve_sms_port, sweep_transport, ResolvePortOptions, SweepOptions,
};

const FIXED_NOW: i64 = 1_760_000_000_000;
const LIMIT: u32 = 50;

fn fixtures() -> PathBuf {
    PathBuf::from(format!(
        "{}/../../fixtures/sms_reader",
        env!("CARGO_MANIFEST_DIR")
    ))
}

fn pty_spec() -> FakeSpec {
    let v: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("sms-modem-io.json")).unwrap(),
    )
    .unwrap();
    serde_json::from_value(v["ptySpec"].clone()).unwrap()
}

/// A FakeModem on the master side of a fresh pty; returns the slave path,
/// the slave fd (kept open so the pair lives) and the modem.
type Raw = Arc<Mutex<Vec<u8>>>;

fn pty_modem(spec: FakeSpec) -> (PathBuf, OwnedFd, Arc<Mutex<FakeModem>>, Raw) {
    let pty = nix::pty::openpty(None, None).expect("openpty");
    let slave_path = std::fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
        .expect("pty slave name");
    let modem = Arc::new(Mutex::new(FakeModem::new(spec)));
    let raw: Raw = Default::default();
    let (m, r) = (modem.clone(), raw.clone());
    let master = File::from(pty.master);
    std::thread::spawn(move || serve(master, &m, &r));
    (slave_path, pty.slave, modem, raw)
}

/// Commands end in `\r` (`cmd\r`); each gets the TS FakeModem's reply lines,
/// `\r\n`-terminated, in one write. Ends when the pty is gone (EIO).
fn serve(master: File, modem: &Mutex<FakeModem>, raw: &Mutex<Vec<u8>>) {
    let mut r = &master;
    let mut w = &master;
    let mut pending = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        raw.lock().unwrap().extend_from_slice(&buf[..n]);
        pending.extend_from_slice(&buf[..n]);
        while let Some(i) = pending.iter().position(|&b| b == b'\r') {
            let cmd: Vec<u8> = pending.drain(..=i).collect();
            let cmd = String::from_utf8_lossy(&cmd[..cmd.len() - 1])
                .trim_start_matches('\n')
                .to_string();
            let reply = modem.lock().unwrap().command(&cmd);
            if let Reply::Lines(lines) = reply {
                let mut out = Vec::new();
                for l in lines {
                    out.extend_from_slice(l.as_bytes());
                    out.extend_from_slice(b"\r\n");
                }
                if w.write_all(&out).is_err() {
                    return;
                }
            }
        }
    }
}

/// The Rust reader path on `port`, stamped like `runSweep` with a fixed now.
async fn rust_read(port: &str) -> String {
    let resolved = resolve_sms_port(&ResolvePortOptions {
        sys_class_tty: PathBuf::from("/nonexistent-sys-class-tty"),
        dev_dir: PathBuf::from("/dev"),
        port_override: Some(port.to_string()),
    })
    .expect("override wins");
    let mut t = open_serial_transport(&resolved).await.expect("open pty");
    let clock = TestClock::new(FIXED_NOW);
    let mut sweep = sweep_transport(&mut *t, SweepOptions::new(clock))
        .await
        .expect("sweep ok");
    sweep.finished_at = FIXED_NOW;
    sweep.duration_ms = 0;
    t.close().await;
    render_json(&build_sms_payload(
        &sweep,
        &SmsPayloadRequest {
            device_id: "local".into(),
            request_id: "cli".into(),
            limit: LIMIT,
            action: xorgate_core::sms::SmsAction::Read,
        },
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pty_end_to_end_matches_the_ts_reader() {
    let spec = pty_spec();

    // Optional: regenerate the expectation from the REAL TS on its own pty.
    if let Ok(js) = std::env::var("XORGATE_SMS_PTY_JS") {
        let (slave, _keep, modem, raw) = pty_modem(spec.clone());
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&js)
            .env("XORGATE_SMS_PORT", &slave)
            .output()
            .await
            .expect("run the TS driver");
        assert!(
            out.status.success(),
            "TS driver failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let log = modem.lock().unwrap().log.clone();
        let rec = serde_json::json!({
            "jsSourceFile": "src/hardware/sms-modem.ts SmsReader + openSerialTransport (real stty + O_RDWR|O_NOCTTY on a pty slave)",
            "jsCommit": std::env::var("XORGATE_SMS_PTY_JS_COMMIT")
                .unwrap_or_else(|_| "09da38dcc8b75c248625a3a0bee43b70e7def55d".into()),
            "generatedBy": "crates/xorgate-hal/tests/at_port_pty.rs with XORGATE_SMS_PTY_JS running tools/replay/js/sms-pty-read.mts (phase-4 WP2)",
            "limit": LIMIT,
            "fixedNow": FIXED_NOW,
            "stdout": stdout,
            "log": log,
            "rawHex": hex::encode(&*raw.lock().unwrap()),
        });
        if let Ok(path) = std::env::var("XORGATE_SMS_PTY_OUT") {
            std::fs::write(&path, serde_json::to_string_pretty(&rec).unwrap() + "\n").unwrap();
        }
    }

    let want: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("pty-e2e.json")).expect("pty-e2e.json"),
    )
    .unwrap();

    let (slave, _keep, modem, raw) = pty_modem(spec);
    let json = rust_read(&slave.to_string_lossy()).await;
    // `console.log(text)` adds the newline.
    assert_eq!(
        format!("{json}\n"),
        want["stdout"].as_str().unwrap(),
        "payload bytes"
    );
    let log = modem.lock().unwrap().log.clone();
    let want_log: Vec<String> = serde_json::from_value(want["log"].clone()).unwrap();
    assert_eq!(log, want_log, "AT command sequence");
    // The full transcript, byte for byte (`\r` terminators included).
    assert_eq!(
        hex::encode(&*raw.lock().unwrap()),
        want["rawHex"].as_str().expect("rawHex"),
        "AT transcript bytes"
    );
    // Text mode on entry: PDU mode for the reads, text mode restored last;
    // mem1 restored before it.
    assert_eq!(log.last().map(String::as_str), Some("AT+CMGF=1"));
    assert_eq!(log[log.len() - 2], "AT+CPMS=\"SR\"");
    let m = modem.lock().unwrap();
    assert_eq!((m.cmgf, m.mem1.as_str()), (1, "SR"));
}

/// `stty` on something that is not a tty fails → `stty failed: …` (error),
/// and a missing node is `no_modem "sms port not present"`; a regular file
/// is `no_modem "sms port is not a character device"`.
#[tokio::test]
async fn open_failures_map_to_the_ts_statuses() {
    use xorgate_core::sms::SmsReadStatus;
    let e = open_serial_transport(std::path::Path::new("/nonexistent/ttyX"))
        .await
        .err()
        .unwrap();
    assert_eq!(
        (e.status, e.message.as_str()),
        (SmsReadStatus::NoModem, "sms port not present")
    );
    let f = std::env::temp_dir().join(format!("xg-not-a-tty-{}", std::process::id()));
    std::fs::write(&f, b"").unwrap();
    let e = open_serial_transport(&f).await.err().unwrap();
    assert_eq!(
        (e.status, e.message.as_str()),
        (SmsReadStatus::NoModem, "sms port is not a character device")
    );
    let _ = std::fs::remove_file(&f);
    // /dev/null is a character device but not a tty: stty refuses.
    let e = open_serial_transport(std::path::Path::new("/dev/null"))
        .await
        .err()
        .unwrap();
    assert_eq!(e.status, SmsReadStatus::Error);
    assert!(e.message.starts_with("stty failed: "), "{}", e.message);
}
