//! Phase 5b WP3 — the SMS clear (`clearTransport` + `SmsReader.clear`,
//! `sms-modem.ts:436-733` @ 09da38d) against `fixtures/sms_reader/
//! sms-clear-io.json`, dumped by `tools/replay/js/dump-sms-clear.mts` from the
//! REAL TS over the test file's FakeModem: the 5 `clearTransport` its and the
//! 6 `SmsReader.clear` its of `test/sms-modem.test.ts` + 15 extras covering
//! every exit path. Each case replays over the Rust FakeModem (same
//! `allowClear` rule: a delete without it throws) and must produce the same
//! outcome / error, the same command log, delete order and end state, and —
//! through `SmsReader::clear` — the same sweep and `…/sms` payload bytes.
//!
//! Plus the Rust-only §6 obligations: mem1 is restored on every exit path
//! (one test per path, panic and a dropped caller included), and the
//! queue semantics hold under concurrency.

#[path = "../../xorgate-hal/tests/support/fake_modem.rs"]
mod fake_modem;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fake_modem::{FakeModem, FakeSpec, MemTransport, Store, TestClock};
use serde_json::{json, Value};
use xorgate_agent::ops::sms::{ClearRequest, OpenTransportFn, SmsReader, SmsReaderDeps};
use xorgate_core::sms::{build_sms_payload, SmsAction, SmsPayloadRequest, SmsReadStatus, SmsSweep};
use xorgate_hal::at_clear::{clear_transport, ClearOptions, ClearOutcome};
use xorgate_hal::at_port::{AtTransport, BoxFut, SmsReadError};

fn load() -> Value {
    let p = format!(
        "{}/../../fixtures/sms_reader/sms-clear-io.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn cases(kind: &str) -> Vec<Value> {
    load()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == kind)
        .cloned()
        .collect()
}

fn strings(v: &Value) -> Vec<String> {
    serde_json::from_value(v.clone()).unwrap()
}

fn cmd() -> ClearRequest {
    ClearRequest::AcceptedCommand {
        request_id: "r".into(),
    }
}

fn clear_payload(s: &SmsSweep) -> String {
    serde_json::to_string(&build_sms_payload(
        s,
        &SmsPayloadRequest {
            device_id: "d".into(),
            request_id: "r".into(),
            limit: 10,
            action: SmsAction::Clear,
        },
    ))
    .unwrap()
}

/// A Rust sweep vs the TS one: same fields, and the `storage` / `cleared`
/// key order exactly (wire bytes).
fn assert_sweep(got: &SmsSweep, want: &Value, name: &str) {
    assert_eq!(serde_json::to_value(got).unwrap(), *want, "{name}: sweep");
    for k in ["storage", "cleared"] {
        assert_eq!(
            serde_json::to_value(got)
                .unwrap()
                .get(k)
                .map(|v| v.to_string()),
            want.get(k).map(|v| v.to_string()),
            "{name}: {k} order"
        );
    }
}

/// The TS error a case expects, as `clear_transport`'s `Err` (the reader's
/// catch mapping: an AtTimeoutError → `error` + its text; any other Error →
/// `error` + its first 120 units).
fn want_error(e: &Value) -> SmsReadError {
    let msg = e["message"].as_str().unwrap();
    match e["kind"].as_str().unwrap() {
        "SmsReadError" => {
            SmsReadError::new(serde_json::from_value(e["status"].clone()).unwrap(), msg)
        }
        _ => SmsReadError::new(SmsReadStatus::Error, msg),
    }
}

fn outcome_json(o: &ClearOutcome) -> Value {
    json!({"cleared": o.cleared, "deleteError": o.delete_error})
}

fn opts_of(v: &Value) -> ClearOptions {
    let mut o = ClearOptions::default();
    if let Some(ms) = v["commandTimeoutMs"].as_u64() {
        o.command_timeout_ms = ms;
    }
    if let Some(ms) = v["deleteTimeoutMs"].as_u64() {
        o.delete_timeout_ms = ms;
    }
    o
}

/// The dump's single `fake2` behind the reader; its `closed` flag is shared.
fn single(modem: Arc<Mutex<FakeModem>>, closed: Arc<AtomicBool>) -> OpenTransportFn {
    Box::new(move |_p: PathBuf| {
        let mut t = MemTransport::new(modem.clone(), None);
        t.closed = closed.clone();
        Box::pin(async move { Ok(Box::new(t) as Box<dyn AtTransport>) })
    })
}

fn reader_with(open: OpenTransportFn, now: i64, opts: &Value) -> Arc<SmsReader> {
    let mut deps = SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/fake"))),
        open,
        TestClock::new(now),
    );
    let o = opts_of(opts);
    deps.clear_command_timeout_ms = o.command_timeout_ms;
    deps.delete_timeout_ms = o.delete_timeout_ms;
    // `sweep: { commandTimeoutMs: opts.commandTimeoutMs }` (3 s when unset).
    deps.command_timeout_ms = o.command_timeout_ms;
    SmsReader::new(deps)
}

// ---------------------------------------------------------------------------
// clearTransport, and the same scenario through SmsReader.clear
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn every_clear_case_matches_the_ts() {
    let all = cases("clear");
    assert_eq!(all.len(), 21);
    let fixed = load()["fixedNow"].as_i64().unwrap();
    for c in &all {
        let name = c["name"].as_str().unwrap();
        let spec: FakeSpec = serde_json::from_value(c["input"]["fake"].clone()).unwrap();
        assert!(spec.allow_clear, "{name}: the dump allows clears");
        let want = &c["expected"];

        // clearTransport over one transport.
        let modem = Arc::new(Mutex::new(FakeModem::new(spec.clone())));
        let mut t = MemTransport::new(modem.clone(), None);
        let got = clear_transport(&mut t, opts_of(&c["input"]["opts"])).await;
        match (&got, want.get("outcome")) {
            (Ok(o), Some(w)) => assert_eq!(outcome_json(o), *w, "{name}: outcome"),
            (Err(e), None) => assert_eq!(*e, want_error(&want["error"]), "{name}: error"),
            (r, _) => panic!("{name}: got {r:?}, want {want}"),
        }
        {
            let m = modem.lock().unwrap();
            assert_eq!(m.log, strings(&want["log"]), "{name}: log");
            assert_eq!(m.deletes, strings(&want["deletes"]), "{name}: deletes");
            assert_eq!(
                json!({"cmgf": m.cmgf, "mem1": m.mem1}),
                want["end"],
                "{name}: end"
            );
            // CMGF is never touched by the clear itself.
            assert!(!m.log.iter().any(|l| l.starts_with("AT+CMGF")), "{name}");
        }

        // SmsReader.clear over a fresh modem, fixed clock.
        let modem2 = Arc::new(Mutex::new(FakeModem::new(spec)));
        let closed = Arc::new(AtomicBool::new(false));
        let reader = reader_with(
            single(modem2.clone(), closed.clone()),
            fixed,
            &c["input"]["opts"],
        );
        let sweep = reader.clear(cmd()).await;
        let r = &want["reader"];
        assert_sweep(&sweep, &r["sweep"], name);
        assert_eq!(
            clear_payload(&sweep),
            r["payload"].to_string(),
            "{name}: payload bytes"
        );
        let m = modem2.lock().unwrap();
        assert_eq!(m.log, strings(&r["log"]), "{name}: reader log");
        assert_eq!(m.deletes, strings(&r["deletes"]), "{name}: reader deletes");
        assert_eq!(
            json!({"cmgf": m.cmgf, "mem1": m.mem1}),
            r["end"],
            "{name}: reader end"
        );
        assert_eq!(
            closed.load(Ordering::SeqCst),
            r["closed"].as_bool().unwrap()
        );
        assert!(
            reader.last().is_none(),
            "{name}: a clear never feeds the reuse window"
        );
    }
}

// ---------------------------------------------------------------------------
// SmsReader.clear (`sms-modem.test.ts:349-461` @ 09da38d)
// ---------------------------------------------------------------------------

type Opened = Arc<Mutex<Vec<(Arc<Mutex<FakeModem>>, Arc<AtomicBool>)>>>;

/// `bench(o)`: one modem whose stores persist across opens (each op opens
/// the port anew; the new fake starts from the last one's stores).
fn bench(spec: FakeSpec, stores: BTreeMap<String, Store>, opened: Opened) -> OpenTransportFn {
    let current = Arc::new(Mutex::new(stores));
    Box::new(move |_p: PathBuf| {
        let mut s = spec.clone();
        if let Some((last, _)) = opened.lock().unwrap().last() {
            *current.lock().unwrap() = last.lock().unwrap().spec.stores.clone();
        }
        s.stores = current.lock().unwrap().clone();
        s.allow_clear = true;
        let modem = Arc::new(Mutex::new(FakeModem::new(s)));
        let t = MemTransport::new(modem.clone(), None);
        opened.lock().unwrap().push((modem, t.closed.clone()));
        Box::pin(async move { Ok(Box::new(t) as Box<dyn AtTransport>) })
    })
}

/// The bench stores (`benchStores()`), from the dump's first clear case.
fn bench_stores() -> BTreeMap<String, Store> {
    let c = &cases("clear")[0];
    serde_json::from_value(c["input"]["fake"]["stores"].clone()).unwrap()
}

fn assert_opened(opened: &Opened, want: &Value, name: &str) {
    let o = opened.lock().unwrap();
    let want = want.as_array().unwrap();
    assert_eq!(o.len(), want.len(), "{name}: opens");
    for (i, ((m, closed), w)) in o.iter().zip(want).enumerate() {
        let m = m.lock().unwrap();
        assert_eq!(m.log, strings(&w["log"]), "{name}: open {i} log");
        assert_eq!(
            m.deletes,
            strings(&w["deletes"]),
            "{name}: open {i} deletes"
        );
        assert_eq!(
            json!({"cmgf": m.cmgf, "mem1": m.mem1}),
            w["end"],
            "{name}: open {i} end"
        );
        assert_eq!(
            closed.load(Ordering::SeqCst),
            w["closed"].as_bool().unwrap()
        );
    }
}

fn bench_reader(spec: FakeSpec, clock: Arc<TestClock>) -> (Arc<SmsReader>, Opened) {
    let opened: Opened = Default::default();
    let reader = SmsReader::new(SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/fake"))),
        bench(spec, bench_stores(), opened.clone()),
        clock,
    ));
    (reader, opened)
}

#[tokio::test(start_paused = true)]
async fn reader_clear_its_match_the_ts() {
    let all = cases("reader_clear");
    assert_eq!(all.len(), 3);
    for c in &all {
        let name = c["name"].as_str().unwrap();
        let spec: FakeSpec = serde_json::from_value(c["input"]["spec"].clone()).unwrap();
        let (reader, opened) = bench_reader(spec, TestClock::new(1_000_000));
        let sweep = reader.clear(cmd()).await;
        assert_sweep(&sweep, &c["expected"]["sweep"], name);
        assert_eq!(
            clear_payload(&sweep),
            c["expected"]["payload"].to_string(),
            "{name}"
        );
        assert_opened(&opened, &c["expected"]["opened"], name);
    }
}

#[tokio::test(start_paused = true)]
async fn drops_the_cached_read_a_read_right_after_a_clear_sweeps_afresh() {
    let c = cases("reader_drop_cache").remove(0);
    let clock = TestClock::new(1_000_000);
    let (reader, opened) = bench_reader(FakeSpec::default(), clock.clone());
    let before = reader.read().await;
    clock.advance(1000);
    let cl = reader.clear(cmd()).await;
    clock.advance(1000); // still inside the 5 s window of `before`
    let after = reader.read().await;
    let w = &c["expected"];
    assert_sweep(&before, &w["before"], "before");
    assert_sweep(&cl, &w["clear"], "clear");
    assert_sweep(&after, &w["after"], "after");
    assert!(!Arc::ptr_eq(&after, &before));
    assert_eq!(w["afterIsBefore"], false);
    assert!(after.messages.is_empty());
    assert_opened(&opened, &w["opened"], "drop cache");
}

#[tokio::test(start_paused = true)]
async fn never_joins_an_in_flight_read_a_read_during_a_clear_waits_and_sweeps_afresh() {
    let c = cases("reader_queue").remove(0);
    let (reader, opened) = bench_reader(FakeSpec::default(), TestClock::new(1_000_000));
    let read1 = reader.read();
    let clear = reader.clear(cmd());
    let clear2 = reader.clear(cmd()); // joins the clear, not the read
    let read2 = reader.read(); // waits for the clear
    let (r1, c1, c2, r2) = tokio::join!(read1, clear, clear2, read2);
    let r3 = reader.read().await;
    let w = &c["expected"];
    assert_sweep(&r1, &w["r1"], "r1");
    assert_sweep(&c1, &w["c"], "c");
    assert_sweep(&r2, &w["r2"], "r2");
    assert!(Arc::ptr_eq(&c1, &c2));
    assert_eq!(w["clearsJoined"], true);
    assert!(!Arc::ptr_eq(&r2, &c1));
    assert!(r2.cleared.is_none());
    assert!(r2.messages.is_empty());
    assert!(
        Arc::ptr_eq(&r3, &r2),
        "the reuse window serves the post-clear read"
    );
    assert_eq!(w["r3IsR2"], true);
    assert_opened(&opened, &w["opened"], "queue");
    let deletes: Vec<usize> = opened
        .lock()
        .unwrap()
        .iter()
        .map(|(m, _)| m.lock().unwrap().deletes.len())
        .collect();
    assert_eq!(deletes, [0, 2, 0]);
}

#[tokio::test(start_paused = true)]
async fn reports_modem_busy_when_another_process_holds_the_lock_and_opens_nothing() {
    let c = cases("reader_locked").remove(0);
    let dir = std::env::temp_dir().join(format!("xg-clear-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let lock = dir.join("sms.lock");
    std::fs::write(&lock, std::process::id().to_string()).unwrap();
    let opens = Arc::new(AtomicUsize::new(0));
    let o2 = opens.clone();
    let mut deps = SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/x"))),
        Box::new(move |_| {
            o2.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(SmsReadError::new(SmsReadStatus::Error, "unreachable")) })
        }),
        TestClock::new(c["input"]["now"].as_i64().unwrap()),
    );
    deps.lock_path = Some(lock.clone());
    let r = SmsReader::new(deps).clear(cmd()).await;
    assert_sweep(&r, &c["expected"]["sweep"], "locked");
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    assert_eq!(c["expected"]["opens"], 0);
    assert!(lock.exists(), "another process's lock is never removed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(start_paused = true)]
async fn no_port_is_no_modem_with_an_empty_cleared() {
    let c = cases("reader_noport").remove(0);
    let r = SmsReader::new(SmsReaderDeps::with(
        Box::new(|| None),
        Box::new(|_| Box::pin(async { Err(SmsReadError::new(SmsReadStatus::Error, "x")) })),
        TestClock::new(c["input"]["now"].as_i64().unwrap()),
    ))
    .clear(cmd())
    .await;
    assert_sweep(&r, &c["expected"]["sweep"], "noport");
    assert_eq!(clear_payload(&r), c["expected"]["payload"].to_string());
}

// ---------------------------------------------------------------------------
// §6 point 4: mem1 restored on EVERY exit path (one test per path)
// ---------------------------------------------------------------------------

fn bench_spec(extra: impl FnOnce(&mut FakeSpec)) -> FakeSpec {
    let mut s = FakeSpec {
        stores: bench_stores(),
        allow_clear: true,
        ..FakeSpec::default()
    };
    extra(&mut s);
    s
}

async fn run_clear(
    spec: FakeSpec,
    o: ClearOptions,
) -> (Result<ClearOutcome, SmsReadError>, FakeModem) {
    let modem = Arc::new(Mutex::new(FakeModem::new(spec)));
    let mut t = MemTransport::new(modem.clone(), None);
    let r = clear_transport(&mut t, o).await;
    let m = modem.lock().unwrap().clone();
    (r, m)
}

fn short() -> ClearOptions {
    ClearOptions {
        command_timeout_ms: 30,
        delete_timeout_ms: 40,
        ..ClearOptions::default()
    }
}

#[tokio::test(start_paused = true)]
async fn mem1_restored_after_a_clean_clear() {
    let (r, m) = run_clear(bench_spec(|s| s.mem1 = Some("ME".into())), short()).await;
    assert!(r.unwrap().delete_error.is_none());
    assert_eq!(
        (m.log.last().unwrap().as_str(), m.mem1.as_str()),
        ("AT+CPMS=\"ME\"", "ME")
    );
}

#[tokio::test(start_paused = true)]
async fn mem1_restored_after_a_delete_error() {
    let (r, m) = run_clear(
        bench_spec(|s| {
            s.cmgd_error.insert("SM".into(), "+CMS ERROR: 500".into());
        }),
        short(),
    )
    .await;
    assert_eq!(
        r.unwrap().delete_error.as_deref(),
        Some("delete SM failed (+CMS ERROR: 500)")
    );
    assert_eq!(m.mem1, "SR");
}

#[tokio::test(start_paused = true)]
async fn mem1_restored_after_a_delete_timeout_and_the_phase_stops() {
    let (r, m) = run_clear(bench_spec(|s| s.hang = vec!["AT+CMGD=0,4".into()]), short()).await;
    assert_eq!(
        r.unwrap().delete_error.as_deref(),
        Some("delete SM failed (timeout)")
    );
    assert!(
        !m.log.contains(&"AT+CPMS=\"ME\"".to_string()),
        "a late OK must not answer ME"
    );
    assert_eq!(
        (m.log.last().unwrap().as_str(), m.mem1.as_str()),
        ("AT+CPMS=\"SR\"", "SR")
    );
}

#[tokio::test(start_paused = true)]
async fn mem1_restored_after_a_select_timeout() {
    let (r, m) = run_clear(
        bench_spec(|s| s.hang = vec!["AT+CPMS=\"ME\"".into()]),
        short(),
    )
    .await;
    assert_eq!(
        r.unwrap().delete_error.as_deref(),
        Some("delete ME failed (select timeout)")
    );
    assert_eq!(m.log.last().unwrap(), "AT+CPMS=\"SR\"");
}

#[tokio::test(start_paused = true)]
async fn mem1_restored_after_a_select_error() {
    let (r, m) = run_clear(
        bench_spec(|s| {
            s.answers
                .insert("AT+CPMS=\"SM\"".into(), vec!["ERROR".into()]);
        }),
        short(),
    )
    .await;
    assert_eq!(
        r.unwrap().delete_error.as_deref(),
        Some("delete SM failed (select: ERROR)")
    );
    assert_eq!(m.mem1, "SR");
}

#[tokio::test(start_paused = true)]
async fn mem1_restored_before_a_transport_error_is_rethrown() {
    let (r, m) = run_clear(
        bench_spec(|s| {
            s.write_error
                .insert("AT+CMGD=0,4".into(), "EIO: i/o error, write".into());
        }),
        short(),
    )
    .await;
    assert_eq!(
        r.unwrap_err(),
        SmsReadError::new(SmsReadStatus::Error, "EIO: i/o error, write")
    );
    assert_eq!(
        (m.log.last().unwrap().as_str(), m.mem1.as_str()),
        ("AT+CPMS=\"SR\"", "SR")
    );
}

/// A transport that panics on the delete: the restore still runs, then the
/// panic resumes (the TS `finally` around a throw).
struct PanicOnDelete(MemTransport);

impl AtTransport for PanicOnDelete {
    fn write<'a>(&'a mut self, data: &'a [u8]) -> BoxFut<'a, std::io::Result<()>> {
        if data.starts_with(b"AT+CMGD") {
            panic!("injected panic on the delete");
        }
        self.0.write(data)
    }
    fn read_chunk(&mut self) -> BoxFut<'_, std::io::Result<Vec<u8>>> {
        self.0.read_chunk()
    }
    fn close(&mut self) -> BoxFut<'_, ()> {
        self.0.close()
    }
}

#[tokio::test(start_paused = true)]
async fn mem1_restored_when_the_phase_panics_then_the_panic_resumes() {
    let modem = Arc::new(Mutex::new(FakeModem::new(bench_spec(|_| {}))));
    let m2 = modem.clone();
    let joined = tokio::spawn(async move {
        let mut t = PanicOnDelete(MemTransport::new(m2, None));
        let _ = clear_transport(&mut t, short()).await;
    })
    .await;
    assert!(
        joined.is_err_and(|e| e.is_panic()),
        "the panic is not swallowed"
    );
    let m = modem.lock().unwrap();
    assert_eq!(
        m.log.last().unwrap(),
        "AT+CPMS=\"SR\"",
        "restored before unwinding"
    );
    assert_eq!(m.mem1, "SR");
}

/// Cancellation: the caller's future dropped mid-clear does not cancel the
/// clear (it runs on the reader's own task), so the restore still happens.
#[tokio::test(start_paused = true)]
async fn mem1_restored_when_the_caller_goes_away() {
    let (reader, opened) = bench_reader(FakeSpec::default(), TestClock::new(1));
    drop(reader.clear(cmd()));
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    let o = opened.lock().unwrap();
    assert_eq!(o.len(), 1);
    let m = o[0].0.lock().unwrap();
    assert_eq!(m.deletes, ["SM", "ME"]);
    assert_eq!(m.mem1, "SR");
    assert!(o[0].1.load(Ordering::SeqCst), "closed");
}

// ---------------------------------------------------------------------------
// The rig rule on the fake itself (§6 point 7)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_fake_modem_refuses_a_delete_unless_allow_clear_is_set() {
    let mut spec = bench_spec(|_| {});
    spec.allow_clear = false;
    let (r, m) = run_clear(spec, short()).await;
    assert_eq!(
        r.unwrap_err(),
        SmsReadError::new(SmsReadStatus::Error, "rig: delete without allowClear")
    );
    assert!(m.deletes.is_empty());
    assert_eq!(m.spec.stores["SM"].slots.len(), 7, "nothing deleted");
    assert_eq!(m.mem1, "SR", "restored anyway");
}
