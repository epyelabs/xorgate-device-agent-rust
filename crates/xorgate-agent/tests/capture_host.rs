//! Phase 7a WP0: the capture host (`xorgate-rs __capture-host`) as a real
//! child process, on GStreamer CORE elements only (`fakesrc`, `identity`,
//! `fakesink` ship in libgstreamer1.0-0, so this runs wherever the agent
//! builds). The full gst-launch 1.22 comparison on the real capture argv is
//! the rig harness (`tools/capture-host/`, fixtures under
//! `fixtures/capture-host/`); these pin the contract pieces the supervisor
//! relies on: the spawn path, `argv[0]`/`comm`, the signal dispositions,
//! `-e` (SIGINT ⇒ EOS ⇒ exit 0), a second SIGINT and SIGTERM killing the
//! process, and gst-launch's parse-error texts and exit code.

use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use xorgate_agent::recording::capture::{CaptureBackend, HostBackend};
use xorgate_hal::child::{ChildDone, ChildEvent};

const BIN: &str = env!("CARGO_BIN_EXE_xorgate-rs");

fn toks(s: &str) -> Vec<String> {
    s.split(' ').map(str::to_string).collect()
}

#[derive(Default, Debug)]
struct Run {
    out: Vec<String>,
    err: Vec<String>,
    done: Option<ChildDone>,
}

/// Drain until `until` appears on stdout (then return, child still running)
/// or the child is done.
async fn drain(rx: &mut UnboundedReceiver<ChildEvent>, run: &mut Run, until: Option<&str>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("capture host timed out");
        match ev {
            Some(ChildEvent::Stdout(l)) => {
                let hit = until.is_some_and(|u| l.starts_with(u));
                run.out.push(l);
                if hit {
                    return;
                }
            }
            Some(ChildEvent::Stderr(l)) => run.err.push(l),
            Some(ChildEvent::Done(d)) => {
                run.done = Some(d);
                return;
            }
            None => return,
        }
    }
}

/// [`drain`] until a STDERR line starts with `until` (child still running)
/// or the child is done.
async fn drain_err(rx: &mut UnboundedReceiver<ChildEvent>, run: &mut Run, until: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("capture host timed out");
        match ev {
            Some(ChildEvent::Stdout(l)) => run.out.push(l),
            Some(ChildEvent::Stderr(l)) => {
                let hit = l.starts_with(until);
                run.err.push(l);
                if hit {
                    return;
                }
            }
            Some(ChildEvent::Done(d)) => {
                run.done = Some(d);
                return;
            }
            None => return,
        }
    }
}

fn status_field(pid: u32, key: &str) -> String {
    let st = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    st.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}:")))
        .unwrap()
        .trim()
        .to_string()
}

fn mask_has(hex: &str, sig: i32) -> bool {
    u64::from_str_radix(hex, 16).unwrap() >> (sig - 1) & 1 == 1
}

/// gst-launch prints these in this order on a clean `-e` stop.
fn assert_in_order(lines: &[String], want: &[&str]) {
    let mut it = lines.iter();
    for w in want {
        assert!(
            it.any(|l| l == w || (w.ends_with('…') && l.starts_with(w.trim_end_matches('…')))),
            "missing {w:?} (in order) in {lines:#?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_backend_sigint_eos_process_name_and_dispositions() {
    let b = HostBackend::with_exe(BIN);
    assert_eq!(b.program(), "xorgate-capture");
    // ~100 buffers/s: a live-ish pipeline that runs until it is stopped.
    let (child, mut rx) = b.spawn(&toks("-e fakesrc ! identity sleep-time=10000 ! fakesink"));
    let pid = child.pid().expect("spawned");
    let mut run = Run::default();
    drain(&mut rx, &mut run, Some("New clock:")).await;
    assert!(run.done.is_none(), "{run:?}");

    assert_eq!(
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap()
            .trim(),
        "xorgate-capture"
    );
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    let argv: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    assert_eq!(argv[0], b"xorgate-capture");
    assert_eq!(argv[1], b"__capture-host");
    assert_eq!(argv[2], b"-e");
    // gst-launch 1.22 at PLAYING: SIGHUP SIGINT SIGQUIT SIGSEGV (+ glibc's
    // SIGSETXID) caught, nothing ignored; SIGTERM/SIGPIPE/SIGBUS default.
    let cgt = status_field(pid, "SigCgt");
    for s in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGSEGV] {
        assert!(mask_has(&cgt, s), "SigCgt {cgt} lacks {s}");
    }
    for s in [libc::SIGTERM, libc::SIGPIPE, libc::SIGBUS, libc::SIGCHLD] {
        assert!(!mask_has(&cgt, s), "SigCgt {cgt} has {s}");
    }
    // Ignored dispositions survive exec, so the host inherits whatever this
    // test process ignores, minus SIGPIPE (std resets it for children): it
    // must add nothing (the rig transcripts, from a clean parent, read 0 for
    // both gst-launch and the host).
    //
    // ci-sigint-race: glibc's internal signals 32 (SIGCANCEL) and 33
    // (SIGSETXID) are left out. glibc 2.36's posix_spawn child sets them to
    // SIG_IGN before exec (they are blocked during the spawn), and std spawns
    // through posix_spawn, so every child of a Rust parent starts with them
    // ignored: the host as the agent spawns it, and this test process too
    // when cargo runs it, but not when a shell does (then this assertion
    // failed with `left: 2147483648, right: 0`). Neither is ever used.
    let internal = (1u64 << 31) | (1u64 << 32);
    let parent_ign = u64::from_str_radix(&status_field(std::process::id(), "SigIgn"), 16).unwrap();
    let want = parent_ign & !(1u64 << (libc::SIGPIPE - 1)) & !internal;
    assert_eq!(
        u64::from_str_radix(&status_field(pid, "SigIgn"), 16).unwrap() & !internal,
        want
    );
    // R-WP0-4: the mask was in place before ANY thread existed, so every
    // thread (main + every GStreamer thread) blocks SIGINT|SIGHUP. Only the
    // wait thread reads 0: the kernel unblocks the waited set during
    // rt_sigtimedwait.
    let mut blocked = Vec::new();
    for t in std::fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
        let st = std::fs::read_to_string(t.unwrap().path().join("status")).unwrap();
        let get = |k: &str| {
            st.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}:")))
                .unwrap()
                .trim()
                .to_string()
        };
        let (name, blk) = (get("Name"), get("SigBlk"));
        if name == "xg-sigwait" {
            continue;
        }
        assert!(
            mask_has(&blk, libc::SIGINT) && mask_has(&blk, libc::SIGHUP),
            "thread {name:?} SigBlk {blk}: the mask was not inherited"
        );
        blocked.push(name);
    }
    // main + at least fakesrc's streaming thread: never vacuous.
    assert!(blocked.len() >= 2, "{blocked:?}");
    // Nice is per process at spawn and inherited (the supervisor renices).
    child.renice(10).unwrap();

    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, None).await;
    let d = run.done.clone().expect("exited");
    assert_eq!((d.exit_code, d.signal), (Some(0), None), "{run:?}");
    assert_in_order(
        &run.out,
        &[
            "Setting pipeline to PAUSED ...",
            "Pipeline is PREROLLING ...",
            "Pipeline is PREROLLED ...",
            "Setting pipeline to PLAYING ...",
            "New clock: GstSystemClock",
            "handling interrupt.",
            "Interrupt: Stopping pipeline ...",
            "EOS on shutdown enabled -- Forcing EOS on the pipeline",
            "Waiting for EOS...",
            "Got EOS from element \"pipeline0\".",
            "EOS received - stopping pipeline...",
            "Execution ended after …",
            "Setting pipeline to NULL ...",
            "Freeing pipeline ...",
        ],
    );
    assert!(run.err.is_empty(), "{:?}", run.err);
}

/// A record `-e` pipeline whose EOS cannot complete on its own: the one
/// buffer sleeps 600 s in identity, on the queue's thread, so fakesrc's
/// thread is free and the forced EOS is queued at once (`Waiting for EOS...`
/// is printed; without the queue `send_event` blocks behind the sleep, in
/// gst-launch too). `fakesink async=false` reaches PLAYING without a preroll
/// buffer. A SIGINT sent during the drain lands during the drain however
/// slow the machine is (ci-sigint-race; the same lines and death by the
/// second SIGINT from gst-launch 1.22 on the rig).
const UNDRAINABLE: &str =
    "-e fakesrc num-buffers=1 ! queue ! identity sleep-time=600000000 ! fakesink async=false";

/// Scenario 2b: once the first SIGINT's one-shot watch is removed, a second
/// SIGINT takes the default action; SIGTERM always does.
///
/// ci-sigint-race (CI runs 42845761508 and 42922380542): this test used to
/// send the second SIGINT the moment `handling interrupt.` was read, with
/// the EOS draining behind a 1 s sleep. Both are timing windows:
/// - gst-launch prints that line BEFORE its watch is removed, and the host
///   keeps that order (MH), so a SIGINT that lands right after the line is
///   coalesced into the dispatch being handled (gst-launch swallows it) and
///   the EOS completes: exit 0, the CI failure. Real gst-launch does the
///   same (`tools/capture-host/sigint2probe.py`, class W);
/// - the EOS completed ~1 s after PLAYING whatever the test did.
///
/// Now the second SIGINT waits for `Waiting for EOS...`, which the loop
/// prints on the NEXT dispatch, after `intr_handler` returned and so after
/// the watch was removed (SigCgt shows it), and the drain cannot end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_sigint_and_sigterm_take_the_default_action() {
    let b = HostBackend::with_exe(BIN);
    let (child, mut rx) = b.spawn(&toks(UNDRAINABLE));
    let pid = child.pid().expect("spawned");
    let mut run = Run::default();
    drain(&mut rx, &mut run, Some("New clock:")).await;
    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, Some("Waiting for EOS...")).await;
    assert!(run.done.is_none(), "{run:?}");
    assert!(
        !mask_has(&status_field(pid, "SigCgt"), libc::SIGINT),
        "the SIGINT watch is still installed after its dispatch: {run:?}"
    );
    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, None).await;
    let d = run.done.clone().unwrap();
    assert_eq!(
        (d.exit_code, d.signal),
        (None, Some(libc::SIGINT)),
        "{run:?}"
    );
    assert!(
        !run.out.iter().any(|l| l.starts_with("Got EOS")),
        "the EOS completed: the second SIGINT did not land mid-drain: {run:?}"
    );

    let (child, mut rx) = b.spawn(&toks("fakesrc ! identity sleep-time=10000 ! fakesink"));
    let mut run = Run::default();
    drain(&mut rx, &mut run, Some("New clock:")).await;
    child.signal(libc::SIGTERM);
    drain(&mut rx, &mut run, None).await;
    let d = run.done.clone().unwrap();
    assert_eq!(
        (d.exit_code, d.signal),
        (None, Some(libc::SIGTERM)),
        "{run:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_without_e_stops_and_exits_zero() {
    let b = HostBackend::with_exe(BIN);
    let (child, mut rx) = b.spawn(&toks("fakesrc ! identity sleep-time=10000 ! fakesink"));
    let mut run = Run::default();
    drain(&mut rx, &mut run, Some("New clock:")).await;
    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, None).await;
    let d = run.done.clone().unwrap();
    // LEC_INTERRUPT exists in gst-launch.c but is never assigned: exit 0.
    assert_eq!((d.exit_code, d.signal), (Some(0), None), "{run:?}");
    assert_in_order(
        &run.out,
        &[
            "handling interrupt.",
            "Interrupt: Stopping pipeline ...",
            "Execution ended after …",
            "Setting pipeline to NULL ...",
            "Freeing pipeline ...",
        ],
    );
    assert!(!run.out.iter().any(|l| l.contains("EOS")), "{run:?}");
}

/// The texts and exit codes WP0 recorded from the real gst-launch 1.22
/// (scenarios 4, 5a-5c).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parse_errors_print_gst_launch_texts_and_exit_1() {
    let cases: [(&[&str], &str); 4] = [
        (
            &["nosuchelement", "!", "fakesink"],
            "WARNING: erroneous pipeline: no element \"nosuchelement\"",
        ),
        (
            &["fakesrc", "!", "!", "fakesink"],
            "WARNING: erroneous pipeline: syntax error",
        ),
        (
            &["fakesrc", "nosuchprop=1", "!", "fakesink"],
            "WARNING: erroneous pipeline: no property \"nosuchprop\" in element \"fakesrc\"",
        ),
        (
            &[""],
            "ERROR: pipeline could not be constructed: empty pipeline not allowed.",
        ),
    ];
    let b = HostBackend::with_exe(BIN);
    for (argv, want) in cases {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let (_child, mut rx) = b.spawn(&args);
        let mut run = Run::default();
        drain(&mut rx, &mut run, None).await;
        let d = run.done.clone().unwrap();
        assert_eq!(
            (d.exit_code, d.signal),
            (Some(1), None),
            "{argv:?}: {run:?}"
        );
        assert_eq!(run.err, vec![want.to_string()], "{argv:?}");
        assert!(run.out.is_empty(), "{argv:?}: {run:?}");
    }
}

/// R-WP0-12: a SIGINT before the watch is armed (`g_unix_signal_add` runs
/// after `set_state(PAUSED)` returns) has the default action. A FIFO with no
/// writer blocks filesrc's open() inside `set_state(PAUSED)`, holding the
/// host in that window for as long as the test needs (rig scenario 9).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_before_the_watch_is_armed_kills_by_sigint() {
    let dir = std::env::temp_dir().join(format!("xg-host-fifo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("in.fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    let b = HostBackend::with_exe(BIN);
    let (child, mut rx) = b.spawn(&toks(&format!(
        "filesrc location={} ! fakesink",
        fifo.display()
    )));
    let mut run = Run::default();
    drain(&mut rx, &mut run, Some("Setting pipeline to PAUSED ...")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, None).await;
    let d = run.done.clone().unwrap();
    assert_eq!(
        (d.exit_code, d.signal),
        (None, Some(libc::SIGINT)),
        "{run:?}"
    );
    assert_eq!(run.out, vec!["Setting pipeline to PAUSED ...".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// R-WP0-12: SIGHUP's watch is G_SOURCE_CONTINUE: two SIGHUPs, two lines,
/// the pipeline keeps running, then a SIGINT stops it (exit 0).
///
/// Each SIGHUP is sent the moment the previous line is read. That is
/// deterministic only because the host re-arms the watch BEFORE the line,
/// as GLib clears its `pending` flag before the callback (ci-sigint-race,
/// D47 dropped). With the old order a SIGHUP landing right after the line
/// was coalesced and the second line never came (CI run 42922380542: drain
/// timeout). Pinned exactly by `capture_host::tests::a_sighup_after_its_line_is_dispatched_again`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sighup_watch_stays_armed() {
    let b = HostBackend::with_exe(BIN);
    let (child, mut rx) = b.spawn(&toks("fakesrc ! identity sleep-time=10000 ! fakesink"));
    let mut run = Run::default();
    drain(&mut rx, &mut run, Some("New clock:")).await;
    for _ in 0..2 {
        child.signal(libc::SIGHUP);
        drain(&mut rx, &mut run, Some("SIGHUP: ")).await;
        assert!(run.done.is_none(), "{run:?}");
    }
    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, None).await;
    let d = run.done.clone().unwrap();
    assert_eq!((d.exit_code, d.signal), (Some(0), None), "{run:?}");
    assert_eq!(
        run.out.iter().filter(|l| l.starts_with("SIGHUP: ")).count(),
        2
    );
}

/// `/bin/sh` that ignores SIGINT, SIGHUP and SIGQUIT and then execs
/// `program args…`: SIG_IGN survives exec (std's spawn resets only SIGPIPE
/// and the mask), so the program starts with all three ignored, as under a
/// CI job shell, `nohup` or `cmd &` in a non-interactive shell.
fn ignoring(program: &str, args: &[String]) -> xorgate_hal::child::ChildSpec {
    let mut argv = vec![
        "-c".to_string(),
        "trap '' INT HUP QUIT; exec \"$0\" \"$@\"".to_string(),
        program.to_string(),
    ];
    argv.extend_from_slice(args);
    xorgate_hal::child::ChildSpec::new("/bin/sh".to_string(), argv)
}

/// ci-sigint-race (the orchestrator's inherited-disposition hypothesis):
/// a host that INHERITS SIGINT/SIGHUP/SIGQUIT as SIG_IGN keeps the whole
/// signal contract. The watches replace the inherited SIG_IGN (as
/// `g_unix_signal_add` does), each SIGHUP dispatches, and after the first
/// SIGINT the default action is SIG_DFL itself, not the inherited
/// disposition (GLib's `unref_unix_signal_handler_unlocked` sets SIG_DFL),
/// so the second SIGINT kills mid-drain. A host that restored the inherited
/// disposition would ignore it and this test would time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_ignored_signals_keep_the_signal_contract() {
    let bits = |sigs: &[i32]| sigs.iter().fold(0u64, |m, s| m | 1 << (s - 1));
    let ign3 = bits(&[libc::SIGINT, libc::SIGHUP, libc::SIGQUIT]);
    // The wrapper really hands over SIG_IGN.
    let (_c, mut rx) =
        xorgate_hal::child::spawn(&ignoring("/bin/cat", &["/proc/self/status".to_string()]));
    let mut run = Run::default();
    drain(&mut rx, &mut run, None).await;
    let ign = run
        .out
        .iter()
        .find_map(|l| l.strip_prefix("SigIgn:"))
        .map(|v| u64::from_str_radix(v.trim(), 16).unwrap())
        .expect("SigIgn");
    assert_eq!(ign & ign3, ign3, "{run:?}");

    let spec = HostBackend::with_exe(BIN).spec(&toks(UNDRAINABLE));
    let (child, mut rx) = xorgate_hal::child::spawn(&ignoring(BIN, &spec.args));
    let pid = child.pid().expect("spawned");
    let mut run = Run::default();
    drain(&mut rx, &mut run, Some("New clock:")).await;
    // Caught, no longer ignored, at PLAYING (as gst-launch).
    let cgt = u64::from_str_radix(&status_field(pid, "SigCgt"), 16).unwrap();
    let ign = u64::from_str_radix(&status_field(pid, "SigIgn"), 16).unwrap();
    assert_eq!(cgt & ign3, ign3, "SigCgt {cgt:x}");
    assert_eq!(ign & ign3, 0, "SigIgn {ign:x}");
    for _ in 0..2 {
        child.signal(libc::SIGHUP);
        drain(&mut rx, &mut run, Some("SIGHUP: ")).await;
        assert!(run.done.is_none(), "{run:?}");
    }
    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, Some("Waiting for EOS...")).await;
    assert!(run.done.is_none(), "{run:?}");
    let ign = u64::from_str_radix(&status_field(pid, "SigIgn"), 16).unwrap();
    assert_eq!(
        ign & bits(&[libc::SIGINT]),
        0,
        "SIGINT back to SIG_IGN: {ign:x}"
    );
    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, None).await;
    let d = run.done.clone().unwrap();
    assert_eq!(
        (d.exit_code, d.signal),
        (None, Some(libc::SIGINT)),
        "{run:?}"
    );
    assert!(!run.out.iter().any(|l| l.starts_with("Got EOS")), "{run:?}");
}

/// R-WP0-12: a READY→PAUSED failure is `LEC_STATE_CHANGE_FAILURE` (-1 =
/// exit 255) after `Failed to set pipeline to PAUSED.` (scenario 6).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_failure_exits_255() {
    let b = HostBackend::with_exe(BIN);
    let (_child, mut rx) = b.spawn(&toks("fakesrc ! filesink location=/nonexistent-xg/dir/x"));
    let mut run = Run::default();
    drain(&mut rx, &mut run, None).await;
    let d = run.done.clone().unwrap();
    assert_eq!((d.exit_code, d.signal), (Some(255), None), "{run:?}");
    assert_eq!(
        run.out,
        vec![
            "Setting pipeline to PAUSED ...".to_string(),
            "Setting pipeline to NULL ...".to_string(),
            "Freeing pipeline ...".to_string(),
        ]
    );
    assert!(
        run.err
            .iter()
            .any(|l| l
                .starts_with("ERROR: from element /GstPipeline:pipeline0/GstFileSink:filesink0:")),
        "{:?}",
        run.err
    );
    assert!(
        run.err
            .iter()
            .any(|l| l == "ERROR: pipeline doesn't want to preroll."),
        "{:?}",
        run.err
    );
    assert_eq!(
        run.err.last().map(String::as_str),
        Some("Failed to set pipeline to PAUSED.")
    );
}

/// R-WP0R-5: the host dumps exactly as gst-launch does,
/// `GST_DEBUG_GRAPH_SHOW_ALL` WITHOUT `SHOW_FULL_PARAMS` (`gst-launch.c:966`),
/// so a non-default value over 80 characters is cut with "…" (the rig's K3
/// shim ORs FULL_PARAMS into both programs and so cannot see this flag).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dot_dumps_use_show_all_without_full_params() {
    let dir = std::env::temp_dir().join(format!("xg-host-dot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A filesink location of 120+ characters: a long, non-default string.
    let long = dir.join(format!("{}.bin", "x".repeat(100)));
    let long = long.display().to_string();
    assert!(long.len() > 100);
    let mut spec = HostBackend::with_exe(BIN).spec(&toks(&format!(
        "fakesrc num-buffers=3 ! filesink location={long}"
    )));
    spec.env.push((
        "GST_DEBUG_DUMP_DOT_DIR".to_string(),
        dir.display().to_string(),
    ));
    let (_child, mut rx) = xorgate_hal::child::spawn(&spec);
    let mut run = Run::default();
    drain(&mut rx, &mut run, None).await;
    let d = run.done.clone().unwrap();
    assert_eq!((d.exit_code, d.signal), (Some(0), None), "{run:?}");
    let dot = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("gst-launch.NULL_READY.dot")
        })
        .expect("the NULL_READY dump (gst-launch's name)");
    let text = std::fs::read_to_string(dot.path()).unwrap();
    assert!(text.contains("location="), "{text}");
    assert!(
        !text.contains(&long),
        "the full {}-char value is in the dump: FULL_PARAMS is set",
        long.len()
    );
    assert!(text.contains('\u{2026}'), "no truncated value in the dump");
    // SHOW_ALL's other bits: states ([-] in NULL→READY) and non-default params.
    assert!(text.contains("num-buffers=3"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: test harness spawns, not the agent
fn unknown_option_is_an_initialisation_error() {
    let out = std::process::Command::new(BIN)
        .args(["__capture-host", "-v", "fakesrc", "!", "fakesink"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error initializing: Unknown option -v\n"
    );
}

fn task_nices(pid: u32) -> Vec<(String, i32)> {
    let mut v = Vec::new();
    for t in std::fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
        let p = t.unwrap().path();
        let comm = std::fs::read_to_string(p.join("comm")).unwrap_or_default();
        let st = std::fs::read_to_string(p.join("stat")).unwrap_or_default();
        let nice = st
            .rsplit_once(')')
            .and_then(|(_, r)| r.split_whitespace().nth(16))
            .and_then(|n| n.parse().ok())
            .unwrap_or(i32::MIN);
        v.push((comm.trim().to_string(), nice));
    }
    v
}

/// WP1 (§Design 5 "Nice"): after the agent's renice (the supervisor renices
/// the pid right after spawn, `capture/mod.rs` `child.renice(10)`), EVERY
/// host thread runs at nice 10, including `xg-sigwait`, which exists from
/// startup and so predates the renice, and the threads created at PAUSED
/// and PLAYING. A FIFO with no writer holds the host inside
/// `set_state(PAUSED)` so the renice lands deterministically AFTER the wait
/// thread started (the race the real agent can lose).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_thread_runs_at_the_agents_nice_after_playing() {
    let dir = std::env::temp_dir().join(format!("xg-host-nice-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("in.fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    let b = HostBackend::with_exe(BIN);
    let (child, mut rx) = b.spawn(&toks(&format!(
        "filesrc location={} ! queue ! identity sleep-time=10000 ! fakesink",
        fifo.display()
    )));
    let pid = child.pid().unwrap();
    let mut run = Run::default();
    drain(&mut rx, &mut run, Some("Setting pipeline to PAUSED ...")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let before = task_nices(pid);
    assert!(
        before.iter().any(|(n, v)| n == "xg-sigwait" && *v == 0),
        "{before:?}"
    );
    child.renice(10).unwrap();
    // Unblock PAUSED: the writer opens the FIFO and keeps feeding it until
    // the checks are done (at most 30 s; a killed host ends it with EPIPE).
    // It used to stop after 2 s, so a slow renice ended in a vanished pid.
    let fifo2 = fifo.clone();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done2 = done.clone();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let mut w = std::fs::OpenOptions::new().write(true).open(fifo2).unwrap();
        let chunk = [0u8; 4096];
        for _ in 0..3000 {
            if done2.load(std::sync::atomic::Ordering::SeqCst) || w.write_all(&chunk).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    drain(&mut rx, &mut run, Some("New clock:")).await;
    // The host renices on its PLAYING dispatch, after `New clock:` (the old
    // fixed 300 ms wait was a wall-clock assumption, ci-sigint-race): poll.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let after = loop {
        let after = task_nices(pid);
        // main, xg-sigwait, filesrc, queue
        if (after.len() >= 4 && after.iter().all(|(_, v)| *v == 10))
            || std::time::Instant::now() >= deadline
        {
            break after;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(after.len() >= 4, "{after:?}");
    assert!(after.iter().all(|(_, v)| *v == 10), "{after:?}");
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, None).await;
    let _ = writer.join();
    let _ = std::fs::remove_dir_all(&dir);
}

/// WP1 (§Design 6): the buffer-flow probes are OFF by default — a branch
/// that never gets a buffer prints nothing — and with
/// `XORGATE_CAPTURE_HOST_DIAG=1` one stderr line names the starved branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diag_probes_are_off_by_default_and_name_a_starved_branch_when_on() {
    // valve drop=true: buffers never reach the sink; async=false lets the
    // pipeline reach PLAYING without a preroll buffer.
    let argv = toks("fakesrc is-live=true ! valve drop=true ! fakesink async=false");
    for on in [false, true] {
        let mut spec = HostBackend::with_exe(BIN).spec(&argv);
        spec.env.push((
            "XORGATE_CAPTURE_HOST_DIAG_STALL_MS".to_string(),
            "300".to_string(),
        ));
        if on {
            spec.env
                .push(("XORGATE_CAPTURE_HOST_DIAG".to_string(), "1".to_string()));
        }
        let (child, mut rx) = xorgate_hal::child::spawn(&spec);
        let mut run = Run::default();
        drain(&mut rx, &mut run, Some("New clock:")).await;
        if on {
            // Wait for the line itself (ci-sigint-race: no wall-clock window).
            drain_err(&mut rx, &mut run, "xorgate-capture diag: ").await;
            assert!(run.done.is_none(), "{run:?}");
        } else {
            // Absence: 4x the stall period. A slow host can only make this
            // vacuous, never fail it.
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
        child.signal(libc::SIGINT);
        drain(&mut rx, &mut run, None).await;
        let d = run.done.clone().unwrap();
        assert_eq!((d.exit_code, d.signal), (Some(0), None), "{run:?}");
        let diag: Vec<&String> = run
            .err
            .iter()
            .chain(run.out.iter())
            .filter(|l| l.contains("diag"))
            .collect();
        if on {
            // `N s` is the quiet time when the loop's check ran: 0 on time, more
            // only if the host was starved past the 300 ms stall by a second.
            let n = run.err.first().and_then(|l| {
                l.strip_prefix("xorgate-capture diag: fakesink0 (fakesink0) no buffers for ")?
                    .strip_suffix(" s")?
                    .parse::<u64>()
                    .ok()
            });
            assert!(run.err.len() == 1 && n.is_some(), "{run:?}");
        } else {
            assert!(diag.is_empty() && run.err.is_empty(), "{run:?}");
        }
        assert!(!run.out.iter().any(|l| l.starts_with("XG-")), "{run:?}");
    }
}

/// WP1: nothing of the host is reachable from the agent's default path. The
/// agent names the host only to DECLARE it (`lib.rs`), to DISPATCH
/// `__capture-host` (`main.rs`) and to build the `HostBackend` spec
/// (`backend.rs`, selected only by `XORGATE_CAPTURE_BACKEND=host`; its default
/// is pinned by `backend::tests::backend_from_env_is_gst_launch_when_the_switch_is_unset`).
/// The timing callbacks, the nice sync and the diag probes live in
/// `capture_host/` and run only inside that child; the diag switch is named
/// nowhere else.
///
/// R-WP1-5: the check is on the IDENTIFIER `capture_host` (word-bounded, in
/// every non-comment line of every crate's `src/` outside `capture_host/`),
/// not on the text `capture_host::`, so any way of reaching the module —
/// a path, `use … as alias`, `use crate::{capture_host, …}`, a glob of the
/// crate root followed by `capture_host::…` — has to name it and shows up
/// with whatever follows it. What it cannot see: a reference built by a
/// macro from pieces (`concat_idents!`-style); no such macro exists in the
/// workspace, and Rust has no other way to name a module without its ident.
#[test]
fn the_host_is_reachable_only_through_its_dispatch_and_backend() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut refs = Vec::new();
    let mut diag_env = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = std::fs::read_dir(&crates)
        .unwrap()
        .map(|e| e.unwrap().path().join("src"))
        .filter(|p| p.is_dir())
        .collect();
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let rel = p.strip_prefix(&crates).unwrap().display().to_string();
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            if text.contains("XORGATE_CAPTURE_HOST_DIAG") {
                diag_env.push(rel.clone());
            }
            if rel.starts_with("xorgate-agent/src/capture_host/") {
                continue;
            }
            for l in text.lines().filter(|l| !l.trim_start().starts_with("//")) {
                let b = l.as_bytes();
                for (at, _) in l.match_indices("capture_host") {
                    let end = at + "capture_host".len();
                    if (at > 0 && ident(b[at - 1])) || (end < b.len() && ident(b[end])) {
                        continue; // another identifier (capture_host_or_continue)
                    }
                    let rest = &l[end..];
                    let what: String = match rest.strip_prefix("::") {
                        Some(r) => r
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect(),
                        None => rest.trim().chars().take(24).collect(),
                    };
                    refs.push(format!("{rel} {what}"));
                }
            }
        }
    }
    refs.sort();
    refs.dedup();
    assert_eq!(
        refs,
        [
            "xorgate-agent/src/lib.rs ;",
            "xorgate-agent/src/main.rs SUBCOMMAND",
            "xorgate-agent/src/main.rs main",
            "xorgate-agent/src/recording/capture/backend.rs PROCESS_NAME",
            "xorgate-agent/src/recording/capture/backend.rs SUBCOMMAND",
        ]
    );
    assert!(
        diag_env.contains(&"xorgate-agent/src/capture_host/diag.rs".to_string())
            && diag_env
                .iter()
                .all(|f| f.starts_with("xorgate-agent/src/capture_host/")),
        "{diag_env:?}"
    );
}
