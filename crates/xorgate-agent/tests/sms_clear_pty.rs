//! Phase 5b WP3 (H11) — the SMS CLEAR end to end over a REAL pty pair: the
//! TS FakeModem (`xorgate-hal/tests/support/fake_modem.rs`, `allowClear`
//! set) serving the master, and the Rust `SmsReader::clear` on the slave
//! through the production transport (`stty -F <slave> raw -echo min 0 time
//! 1`, `open(O_RDWR|O_NOCTTY)`), then a read on the same reader.
//!
//! The proof is BYTE-level: every byte the agent wrote to the modem (the
//! full AT transcript, `\r` terminators included) must equal what the REAL
//! TS `SmsReader.clear()` @ 09da38d wrote to the same fake on its own pty —
//! as must the command log, the delete order, the modem's end state and the
//! printed payloads. Expected: `fixtures/sms_reader/pty-clear-e2e.json`.
//! Regenerate (node + the shared 09da38d build, e.g. inside
//! `xorgate-replay-rig:node20-bookworm` with this test binary):
//!
//! ```text
//! XORGATE_SMS_PTY_CLEAR_JS='cd /js && npx --no-install tsx .rig-drivers/sms-pty-clear.mts 10 1760000000000' \
//! XORGATE_SMS_PTY_CLEAR_OUT=/src/fixtures/sms_reader/pty-clear-e2e.json  <this test binary>
//! ```

#[path = "../../xorgate-hal/tests/support/fake_modem.rs"]
mod fake_modem;

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fake_modem::{FakeModem, FakeSpec, Reply, TestClock};
use serde_json::Value;
use xorgate_agent::ops::sms::{SmsReader, SmsReaderDeps};
use xorgate_core::sms::{build_sms_payload, SmsAction, SmsPayloadRequest};
use xorgate_core::sms_cli::render_json;
use xorgate_core::sms_cli::ClearGate;
use xorgate_hal::at_port::open_serial_transport;
use xorgate_hal::sms_clear_token::{ClearRequest, CliConfirmation};

const FIXED_NOW: i64 = 1_760_000_000_000;
const LIMIT: u32 = 10;

fn fixtures() -> PathBuf {
    PathBuf::from(format!(
        "{}/../../fixtures/sms_reader",
        env!("CARGO_MANIFEST_DIR")
    ))
}

/// The read test's pty inbox (bench stores + a SUBMIT + a two-part UCS2
/// concat, text mode on entry), with deletes allowed.
fn pty_spec() -> FakeSpec {
    let v: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("sms-modem-io.json")).unwrap(),
    )
    .unwrap();
    let mut spec: FakeSpec = serde_json::from_value(v["ptySpec"].clone()).unwrap();
    spec.allow_clear = true;
    spec
}

type Raw = Arc<Mutex<Vec<u8>>>;

/// A FakeModem on the master side of a fresh pty: the slave path, the slave
/// fd (kept open so the pair lives), the modem and every raw byte received.
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

/// Commands end in `\r`; each gets the FakeModem's reply lines,
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

/// The Rust path on `port`: the production `SmsReader` (port override, real
/// tty transport, no lock, fixed clock) → `clear()` → payload text; then a
/// read on the same reader.
async fn rust_clear(port: &str) -> String {
    let port = PathBuf::from(port);
    let mut deps = SmsReaderDeps::with(
        Box::new(move || Some(port.clone())),
        Box::new(|p: PathBuf| Box::pin(async move { open_serial_transport(&p).await })),
        TestClock::new(FIXED_NOW),
    );
    deps.lock_path = None;
    let reader = SmsReader::new(deps);
    let sweep = reader
        .clear(ClearRequest::confirmed_cli(
            CliConfirmation::from_gate(ClearGate::Proceed, None).unwrap(),
        ))
        .await;
    let clear = render_json(&build_sms_payload(
        &sweep,
        &SmsPayloadRequest {
            device_id: "local".into(),
            request_id: "cli".into(),
            limit: LIMIT,
            action: SmsAction::Clear,
        },
    ));
    let after = reader.read().await;
    let read = render_json(&build_sms_payload(
        &after,
        &SmsPayloadRequest::read("local", "cli", LIMIT),
    ));
    // `console.log(text)` adds the newline.
    format!("{clear}\n{read}\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pty_clear_end_to_end_is_byte_equal_to_the_ts_reader() {
    let spec = pty_spec();

    // Optional: regenerate the expectation from the REAL TS on its own pty.
    if let Ok(js) = std::env::var("XORGATE_SMS_PTY_CLEAR_JS") {
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
        let m = modem.lock().unwrap();
        let rec = serde_json::json!({
            "jsSourceFile": "src/hardware/sms-modem.ts SmsReader.clear + read + openSerialTransport (real stty + O_RDWR|O_NOCTTY on a pty slave)",
            "jsCommit": "09da38dcc8b75c248625a3a0bee43b70e7def55d",
            "generatedBy": "crates/xorgate-agent/tests/sms_clear_pty.rs with XORGATE_SMS_PTY_CLEAR_JS running tools/replay/js/sms-pty-clear.mts (phase-5b WP3)",
            "limit": LIMIT,
            "fixedNow": FIXED_NOW,
            "stdout": String::from_utf8(out.stdout).unwrap(),
            "log": m.log,
            "deletes": m.deletes,
            "end": {"cmgf": m.cmgf, "mem1": m.mem1},
            "rawHex": hex::encode(&*raw.lock().unwrap()),
        });
        if let Ok(path) = std::env::var("XORGATE_SMS_PTY_CLEAR_OUT") {
            std::fs::write(&path, serde_json::to_string_pretty(&rec).unwrap() + "\n").unwrap();
        }
    }

    let want: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("pty-clear-e2e.json"))
            .expect("pty-clear-e2e.json"),
    )
    .unwrap();

    let (slave, _keep, modem, raw) = pty_modem(spec);
    let stdout = rust_clear(&slave.to_string_lossy()).await;
    assert_eq!(stdout, want["stdout"].as_str().unwrap(), "payload bytes");
    // THE transcript: every byte written to the modem, in order.
    assert_eq!(
        hex::encode(&*raw.lock().unwrap()),
        want["rawHex"].as_str().unwrap(),
        "AT transcript bytes"
    );
    let m = modem.lock().unwrap();
    let want_log: Vec<String> = serde_json::from_value(want["log"].clone()).unwrap();
    assert_eq!(m.log, want_log, "AT command sequence");
    let want_deletes: Vec<String> = serde_json::from_value(want["deletes"].clone()).unwrap();
    assert_eq!(m.deletes, want_deletes, "delete order");
    assert_eq!(m.deletes, ["SM", "ME"]);
    assert_eq!(
        serde_json::json!({"cmgf": m.cmgf, "mem1": m.mem1}),
        want["end"],
        "modem end state"
    );
    // The clear never touches CMGF; the read after it does (and restores).
    let clear_part: Vec<&String> = m
        .log
        .iter()
        .take_while(|c| !c.starts_with("AT+CMGF"))
        .collect();
    assert_eq!(
        clear_part
            .iter()
            .filter(|c| c.starts_with("AT+CMGD"))
            .count(),
        2,
        "exactly one delete per store"
    );
    assert!(m
        .log
        .iter()
        .all(|c| c == "AT+CMGD=0,4" || !c.contains("CMGD")));
}
