//! `xorgate_hal::at_port` against `fixtures/sms_reader/sms-modem-io.json`:
//! the REAL `src/hardware/sms-modem.ts` @ 89b5c64 run by
//! `tools/replay/js/dump-sms-reader.mts` over the test file's FakeModem
//! (CF29 / CF60: the 15 `sms-modem.test.ts` M/F its + extras). Each case is
//! replayed over the Rust FakeModem (`tests/support/fake_modem.rs`): the
//! exact command log, the result or error, and the modem's end state.
//! The `SmsReader` cases + the `reader` half of each sweep case are replayed
//! in `crates/xorgate-agent/tests/sms_reader.rs`.

#[path = "support/fake_modem.rs"]
mod fake_modem;

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fake_modem::{FakeModem, FakeSpec, MemTransport, TestClock};
use serde_json::Value;
use xorgate_core::sms::SmsReadStatus;
use xorgate_hal::at_port::{
    acquire_sms_lock, resolve_sms_port, sweep_transport, AtError, AtSession, ResolvePortOptions,
    SmsReadError, SweepOptions,
};

fn fixture() -> Value {
    let p = format!(
        "{}/../../fixtures/sms_reader/sms-modem-io.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn cases(kind: &str) -> Vec<Value> {
    fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == kind)
        .cloned()
        .collect()
}

fn status_of(s: &str) -> SmsReadStatus {
    serde_json::from_value(Value::String(s.to_string())).unwrap()
}

/// The TS error a case recorded, in the shape `SmsReader` maps it to
/// (`sms-modem.ts:533-535`).
fn expected_read_error(e: &Value) -> SmsReadError {
    let msg = e["message"].as_str().unwrap();
    match e["kind"].as_str().unwrap() {
        "SmsReadError" => SmsReadError::new(status_of(e["status"].as_str().unwrap()), msg),
        "AtTimeoutError" => SmsReadError::new(SmsReadStatus::Error, msg),
        _ => SmsReadError::new(
            SmsReadStatus::Error,
            String::from_utf16_lossy(&msg.encode_utf16().take(120).collect::<Vec<_>>()),
        ),
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// AtSession (`sms-modem.test.ts:131-143` + extras)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn at_session_cases() {
    let cs = cases("session");
    assert_eq!(cs.len(), 7);
    for c in cs {
        let name = c["name"].as_str().unwrap();
        let input = &c["input"];
        let spec: FakeSpec = serde_json::from_value(input["fake"].clone()).unwrap();
        let modem = Arc::new(Mutex::new(FakeModem::new(spec)));
        let mut t = MemTransport::new(modem.clone(), None);
        let mut at = AtSession::new(&mut t, input["timeoutMs"].as_u64().unwrap());
        let r = at
            .command(
                input["cmd"].as_str().unwrap(),
                input["prefix"].as_str(),
                false,
            )
            .await;
        let exp = &c["expected"];
        match r {
            Ok(resp) => assert_eq!(
                serde_json::to_value(&resp).unwrap(),
                exp["response"],
                "{name}"
            ),
            Err(AtError::Timeout(m)) => {
                assert_eq!(exp["error"]["kind"], "AtTimeoutError", "{name}");
                assert_eq!(m, exp["error"]["message"].as_str().unwrap(), "{name}");
            }
            Err(e) => panic!("{name}: unexpected {e:?}"),
        }
        assert_eq!(modem.lock().unwrap().log, strings(&exp["log"]), "{name}");
    }
}

// ---------------------------------------------------------------------------
// sweepTransport (`sms-modem.test.ts:146-220` + extras)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn sweep_transport_cases() {
    let cs = cases("sweep");
    assert_eq!(cs.len(), 22);
    for c in cs {
        let name = c["name"].as_str().unwrap();
        let input = &c["input"];
        let spec: FakeSpec = serde_json::from_value(input["fake"].clone()).unwrap();
        let clock = TestClock::new(spec.t0.unwrap_or(0));
        let tick = spec.tick_ms.map(|ms| (clock.clone(), ms));
        let modem = Arc::new(Mutex::new(FakeModem::new(spec)));
        let mut t = MemTransport::new(modem.clone(), tick);
        let mut opts = SweepOptions::new(clock.clone());
        if let Some(ms) = input["opts"]["commandTimeoutMs"].as_u64() {
            opts.command_timeout_ms = ms;
        }
        if let Some(ms) = input["opts"]["budgetMs"].as_u64() {
            opts.budget_ms = ms;
        }
        let r = sweep_transport(&mut t, opts).await;
        let exp = &c["expected"];
        match (&r, exp.get("result")) {
            (Ok(s), Some(want)) => {
                // Key order of `storage` is part of the wire bytes.
                assert_eq!(
                    serde_json::to_string(&s.storage).unwrap(),
                    serde_json::to_string(&want["storage"]).unwrap(),
                    "{name}: storage"
                );
                assert_eq!(Value::from(s.skipped), want["skipped"], "{name}: skipped");
                assert_eq!(Value::from(s.partial), want["partial"], "{name}: partial");
                assert_eq!(
                    serde_json::to_string(&s.messages).unwrap(),
                    serde_json::to_string(&want["messages"]).unwrap(),
                    "{name}: messages"
                );
                assert_eq!(s.status, SmsReadStatus::Ok);
            }
            (Err(e), None) => assert_eq!(*e, expected_read_error(&exp["error"]), "{name}"),
            (r, _) => panic!("{name}: got {r:?}, want {exp}"),
        }
        let m = modem.lock().unwrap();
        assert_eq!(m.log, strings(&exp["log"]), "{name}: command log");
        assert_eq!(Value::from(m.cmgf), exp["end"]["cmgf"], "{name}: end cmgf");
        assert_eq!(
            m.mem1,
            exp["end"]["mem1"].as_str().unwrap(),
            "{name}: end mem1"
        );
        // Read-only by construction (sms-modem.test.ts:158).
        for cmd in &m.log {
            assert!(!is_destructive(cmd), "{name}: sent {cmd}");
        }
    }
}

/// `/CMGR=|CMGL|CMGD|CMGW|CMSS|CPMS="[^"]*",/` (`sms-modem.test.ts:158`).
fn is_destructive(c: &str) -> bool {
    if ["CMGR=", "CMGL", "CMGD", "CMGW", "CMSS"]
        .iter()
        .any(|t| c.contains(t))
    {
        return true;
    }
    // CPMS="<anything but ">", — a 2-argument CPMS.
    let mut rest = c;
    while let Some(i) = rest.find("CPMS=\"") {
        let after = &rest[i + 6..];
        if let Some(j) = after.find('"') {
            if after[j + 1..].starts_with(',') {
                return true;
            }
        }
        rest = after;
    }
    false
}

#[test]
fn destructive_matcher_is_the_ts_regex() {
    assert!(is_destructive("AT+CMGR=1"));
    assert!(is_destructive("AT+CMGL=4"));
    assert!(is_destructive("AT+CMGD=1"));
    assert!(is_destructive("AT+CMGW"));
    assert!(is_destructive("AT+CMSS=1"));
    assert!(is_destructive("AT+CPMS=\"SM\",\"SM\""));
    assert!(!is_destructive("AT+CMGRO=1"));
    assert!(!is_destructive("AT+CPMS=\"SM\""));
    assert!(!is_destructive("AT+CPMS?"));
}

/// The WP2 rule: the module's own text never names a command that marks
/// read, deletes, writes or sends, nor a 2-argument `CPMS`, nor a modem tty
/// path — the only paths it opens are what `resolve_sms_port` returns.
#[test]
fn at_port_source_is_read_only_and_names_no_tty() {
    let dir = env!("CARGO_MANIFEST_DIR");
    for (file, src) in [
        (
            "at_port.rs",
            std::fs::read_to_string(format!("{dir}/src/at_port.rs")).unwrap(),
        ),
        (
            "ops/sms.rs",
            std::fs::read_to_string(format!("{dir}/../xorgate-agent/src/ops/sms.rs")).unwrap(),
        ),
    ] {
        // Code only: comment and doc lines may explain WHY ttyUSB2 is off-limits.
        let code: String = src
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for bad in [
            "CMGR=", "CMGL", "CMGD", "CMGW", "CMSS", "/dev/tty", "ttyUSB2",
        ] {
            assert!(!code.contains(bad), "{file} names {bad}");
        }
        // Every `CPMS="…"` literal in the code is one-argument.
        for (i, _) in code.match_indices("CPMS=\\\"") {
            let tail = &code[i + 7..];
            let close = tail.find("\\\"").expect("closing quote");
            assert!(
                !tail[close + 2..].starts_with(','),
                "{file}: 2-argument CPMS at byte {i}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// acquireSmsLock (`sms-modem.test.ts:281-289` + extras)
// ---------------------------------------------------------------------------

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-at-port-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn acquire_sms_lock_case() {
    let c = cases("lock").remove(0);
    let steps = c["expected"]["steps"].as_array().unwrap().clone();
    let home = scratch("lock");
    let p = home.join("l");
    let pid = std::process::id().to_string();

    // free → a release fn; the file holds our pid, mode 0644 (umask 022).
    let lock = acquire_sms_lock(&p).expect("free lock");
    assert!(lock.is_held());
    assert_eq!(steps[0]["got"], "function");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), pid);
    let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    let umask = current_umask();
    assert_eq!(mode, 0o644 & !umask);
    if umask == 0o022 {
        assert_eq!(format!("{mode:o}"), steps[0]["mode"].as_str().unwrap());
    }
    // held (our live pid) → null.
    assert!(acquire_sms_lock(&p).is_none());
    assert!(steps[1]["got"].is_null());
    // release → unlinked.
    drop(lock);
    assert!(!p.exists());
    assert_eq!(steps[2]["exists"], false);
    // stale pid → taken over, our pid written.
    std::fs::write(&p, "999999999").unwrap();
    let l2 = acquire_sms_lock(&p).expect("stale taken over");
    assert_eq!(steps[3]["got"], "function");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), pid);
    drop(l2);
    // extra: an unparseable pid is stale.
    std::fs::write(&p, "garbage").unwrap();
    let l3 = acquire_sms_lock(&p);
    assert_eq!(steps[4]["got"], "function");
    assert!(l3.is_some());
    drop(l3);
    // extra: pid 1 is alive (kill succeeds as root, EPERM otherwise).
    std::fs::write(&p, "1").unwrap();
    assert!(acquire_sms_lock(&p).is_none());
    assert!(steps[5]["got"].is_null());
    // extra: the lock dir is missing → proceed unlocked (not EEXIST).
    let l4 = acquire_sms_lock(&home.join("nope/l")).expect("unlocked");
    assert!(!l4.is_held());
    let _ = std::fs::remove_dir_all(&home);
}

fn current_umask() -> u32 {
    let old = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));
    nix::sys::stat::umask(old);
    old.bits() as u32
}

// ---------------------------------------------------------------------------
// resolveSmsPort (`sms-modem.test.ts:316-330` + extras)
// ---------------------------------------------------------------------------

/// `fakeSys` (`sms-modem.test.ts:300-314`): `/sys/class/tty/<n>/device` →
/// `<usb>/<iface>/<n>`, `idVendor` on the usb dir, `bInterfaceNumber` on
/// the interface dir.
fn fake_sys(home: &Path, ports: &[Value]) -> PathBuf {
    let sys = home.join("sys");
    std::fs::create_dir(&sys).unwrap();
    for p in ports {
        let (name, iface, vendor) = (
            p["name"].as_str().unwrap(),
            p["iface"].as_str().unwrap(),
            p["vendor"].as_str().unwrap(),
        );
        let usb = home.join("usb").join(format!("{vendor}-{name}"));
        let ifdir = usb.join(format!("1.{iface}"));
        let dev = ifdir.join(name);
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(usb.join("idVendor"), format!("{vendor}\n")).unwrap();
        std::fs::write(ifdir.join("bInterfaceNumber"), format!("{iface}\n")).unwrap();
        std::fs::create_dir(sys.join(name)).unwrap();
        symlink(&dev, sys.join(name).join("device")).unwrap();
    }
    sys
}

#[test]
fn resolve_sms_port_cases() {
    let cs = cases("resolve");
    assert_eq!(cs.len(), 8);
    for (i, c) in cs.iter().enumerate() {
        let name = c["name"].as_str().unwrap();
        let home = scratch(&format!("resolve{i}"));
        let sys = match c["input"]["ports"].as_array() {
            Some(ports) => fake_sys(&home, ports),
            None => home.join("nope"),
        };
        let got = resolve_sms_port(&ResolvePortOptions {
            sys_class_tty: sys,
            dev_dir: PathBuf::from("/dev"),
            port_override: c["input"]["env"]["XORGATE_SMS_PORT"]
                .as_str()
                .map(str::to_string),
        });
        let want = c["expected"]["port"].as_str().map(PathBuf::from);
        assert_eq!(got, want, "{name}");
        let _ = std::fs::remove_dir_all(&home);
    }
}

// ---------------------------------------------------------------------------
// I9 tape hooks: `at_tx` at every write (no `\r`), `at_rx` at every chunk read
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecTape(Mutex<Vec<String>>);

impl xorgate_hal::io::TapeSink for RecTape {
    fn at_tx(&self, port: &str, line: &str) {
        self.0.lock().unwrap().push(format!("tx {port} {line}"));
    }
    fn at_rx(&self, port: &str, bytes: &[u8]) {
        self.0.lock().unwrap().push(format!(
            "rx {port} {}",
            String::from_utf8_lossy(bytes).escape_debug()
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn session_reports_at_tx_and_at_rx_to_the_tape() {
    let modem = Arc::new(Mutex::new(FakeModem::new(FakeSpec::default())));
    let mut t = MemTransport::new(modem, None);
    let tape = Arc::new(RecTape::default());
    let mut at = AtSession::new(&mut t, 200).with_tape("/dev/ttyUSB3", tape.clone());
    at.command("AT+CMGF?", Some("+CMGF:"), false).await.unwrap();
    assert_eq!(
        *tape.0.lock().unwrap(),
        vec![
            "tx /dev/ttyUSB3 AT+CMGF?".to_string(),
            "rx /dev/ttyUSB3 +CMGF: 0\\r\\nOK\\r\\n".to_string(),
        ]
    );
}
