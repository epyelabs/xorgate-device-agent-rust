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
    let parent_ign = u64::from_str_radix(&status_field(std::process::id(), "SigIgn"), 16).unwrap();
    let want = parent_ign & !(1u64 << (libc::SIGPIPE - 1));
    assert_eq!(
        u64::from_str_radix(&status_field(pid, "SigIgn"), 16).unwrap(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_sigint_and_sigterm_take_the_default_action() {
    let b = HostBackend::with_exe(BIN);
    // EOS waits behind a 1 s sleep in identity: the second SIGINT lands
    // while the first one's EOS is still draining (scenario 2b).
    let (child, mut rx) = b.spawn(&toks("-e fakesrc ! identity sleep-time=1000000 ! fakesink"));
    let mut run = Run::default();
    drain(&mut rx, &mut run, Some("New clock:")).await;
    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, Some("handling interrupt.")).await;
    child.signal(libc::SIGINT);
    drain(&mut rx, &mut run, None).await;
    let d = run.done.clone().unwrap();
    assert_eq!(
        (d.exit_code, d.signal),
        (None, Some(libc::SIGINT)),
        "{run:?}"
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

#[test]
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
