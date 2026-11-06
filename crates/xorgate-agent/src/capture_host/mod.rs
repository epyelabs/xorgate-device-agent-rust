//! The capture host (Phase 7a WP0, `phase-7a-inprocess-gstreamer.md` §Design
//! 1-5): `xorgate-rs __capture-host [-e] <tokens…>`, run by the agent as a
//! per-camera child with `argv[0] = xorgate-capture`, executes the SAME
//! gst-launch argv tokens the core builders emit (`xorgate_core::
//! video_capture`) through gstreamer-rs instead of `gst-launch-1.0`.
//!
//! It is a pure pipeline runner: `main.rs` dispatches here before ANY agent
//! init (no `alloc::tune`, no tracing, no `agent.env`, `/etc/xorgate` or
//! settings read). Startup order: `PR_SET_NAME("xorgate-capture")`; block
//! SIGINT+SIGHUP and start the signal-wait thread (before any other thread
//! exists, so every GStreamer thread inherits the mask); `gst::init()`;
//! `gst_parse_launchv`; PAUSED → PLAYING from the bus loop.
//!
//! The behaviour is a line-by-line port of `tools/gst-launch.c` @ 1.22.0
//! (the Bookworm package the bench and the rig run), measured against the
//! real binary by `tools/capture-host/transcript.py` (fixtures under
//! `fixtures/capture-host/gst-launch-1.22.0-debian-12.13/`):
//!
//! - parse with `gst_parse_launchv` (no context, no flags — what 1.22 calls;
//!   the R2 "launchv_full with gst-launch's flags" is exactly this): NULL →
//!   `ERROR: pipeline could not be constructed: …` (exit 1); a partial
//!   pipeline with an error → `WARNING: erroneous pipeline: …` (exit 1);
//! - the bus SYNC handler dumps the dot graph on every pipeline state change
//!   (`gst-launch.<OLD>_<NEW>`, a no-op unless `GST_DEBUG_DUMP_DOT_DIR`) and
//!   prints errors from the posting thread, as gst-launch's does;
//! - the bus loop on the main thread (no GLib main loop, no tokio:
//!   `Bus::timed_pop`) is gst-launch's `bus_handler`: `New clock:`, `Got EOS
//!   from element "pipeline0".`, PREROLLED → PLAYING, latency
//!   redistribution, `-e` (EOS on SIGINT), …;
//! - exit codes: 0, 1 (`LEC_ERROR`), 255 (`LEC_STATE_CHANGE_FAILURE` = -1);
//!   a second SIGINT, or one before the watch is armed, kills the process
//!   with SIGINT; SIGTERM keeps its default action.
//!
//! Not ported (no builder argv uses them; D-row candidates for WP1): the
//! gst-launch options other than `-e` (`-v -q -m -t -c -X -f
//! --no-position --force-position`, the `--gst-*` group: rejected as
//! `Error initializing: Unknown option …`), `g_on_error_stack_trace`'s gdb
//! exec in the fault handler, and GLib's charset conversion of output in a
//! non-UTF-8 locale (the host writes UTF-8 as is).

mod diag;
pub mod nls;
mod sys;
mod timing;

use std::ffi::{CStr, CString, OsString};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

use nls::{tr, trf, Arg};

/// The hidden subcommand `main.rs` dispatches before anything else.
pub const SUBCOMMAND: &str = "__capture-host";
/// `argv[0]`, `comm` (PR_SET_NAME) and the tape's `child_spawn.argv[0]`.
pub const PROCESS_NAME: &str = "xorgate-capture";
const PROCESS_NAME_C: &CStr = c"xorgate-capture";
/// `g_set_prgname`: GLib/GStreamer log prefixes stay byte-equal to gst-launch's.
const PRGNAME: &str = "gst-launch-1.0";

/// `LaunchExitCode` (`gst-launch.c:65-71`).
const LEC_STATE_CHANGE_FAILURE: i32 = -1;
const LEC_ERROR: i32 = 1;

/// `gst-launch-1.0`'s options this host understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostArgs {
    /// `-e` / `--eos-on-shutdown`.
    pub eos_on_shutdown: bool,
    /// The pipeline description tokens, verbatim.
    pub tokens: Vec<Vec<u8>>,
}

/// GOption-style parse of the host argv (options may appear anywhere, `--`
/// ends them). `Err` = gst-launch's `Error initializing: …` text.
pub fn parse_args(args: &[OsString]) -> Result<HostArgs, String> {
    let mut out = HostArgs {
        eos_on_shutdown: false,
        tokens: Vec::new(),
    };
    let mut rest_are_tokens = false;
    for a in args {
        let b = a.as_bytes();
        if rest_are_tokens || b.len() < 2 || b[0] != b'-' {
            out.tokens.push(b.to_vec());
            continue;
        }
        match b {
            b"--" => rest_are_tokens = true,
            b"-e" | b"--eos-on-shutdown" => out.eos_on_shutdown = true,
            _ => return Err(format!("Unknown option {}", a.to_string_lossy())),
        }
    }
    Ok(out)
}

/// `xorgate-rs __capture-host …` → the process exit code (`-1` = 255).
pub fn main(args: &[OsString]) -> i32 {
    sys::set_process_name(PROCESS_NAME_C);
    sys::setlocale_from_env();
    // `g_setenv ("GST_GL_XINITTHREADS"/"GST_XINITTHREADS", "1")` (gst-launch.c:1155).
    std::env::set_var("GST_GL_XINITTHREADS", "1");
    std::env::set_var("GST_XINITTHREADS", "1");
    if let Err(e) = sys::init_signals() {
        printerr(&format!("Error initializing: {e}\n"));
        return 1;
    }
    gst::glib::set_prgname(Some(PRGNAME));
    let args = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            printerr(&format!("Error initializing: {e}\n"));
            return 1;
        }
    };
    if let Err(e) = gst::init() {
        printerr(&format!("Error initializing: {}\n", e.message()));
        return 1;
    }
    sys::fault_setup();
    let tokens: Vec<CString> = args
        .tokens
        .iter()
        .map(|t| CString::new(t.clone()).unwrap_or_default())
        .collect();
    let code = run(args.eos_on_shutdown, &tokens);
    sys::deinit();
    code
}

/// State the bus sync handler (streaming threads) shares with the loop.
#[derive(Default)]
struct Shared {
    target_playing: AtomicBool,
    interrupting: AtomicBool,
    quit: AtomicBool,
    code: AtomicI32,
}

fn run(eos_on_shutdown: bool, tokens: &[CString]) -> i32 {
    let element = match sys::parse_launchv(tokens) {
        sys::Parsed::Null(Some(msg)) => {
            printerr(&trf(
                c"ERROR: pipeline could not be constructed: %s.\n",
                &[Arg::S(&msg)],
            ));
            return 1;
        }
        sys::Parsed::Null(None) => {
            printerr(&tr(c"ERROR: pipeline could not be constructed.\n"));
            return 1;
        }
        sys::Parsed::Erroneous(msg) => {
            printerr(&trf(c"WARNING: erroneous pipeline: %s\n", &[Arg::S(&msg)]));
            return 1;
        }
        sys::Parsed::Ok(e) => e,
    };
    // "If the top-level object is not a pipeline, place it in a pipeline."
    let pipeline = if element.is::<gst::Pipeline>() {
        element
    } else {
        let Ok(p) = gst::ElementFactory::make("pipeline").build() else {
            printerr(&tr(c"ERROR: the 'pipeline' element wasn't found.\n"));
            return 1;
        };
        let Some(bin) = p.downcast_ref::<gst::Bin>() else {
            return 1;
        };
        if bin.add(&element).is_err() {
            return 1;
        }
        p
    };
    let Some(bus) = pipeline.bus() else {
        return 1;
    };
    let shared = Arc::new(Shared::default());
    // WP1 (§Design 8): exact fragment timing on the one splitmuxsink (record
    // mode); bridge-only and every other argv have none.
    let timing = timing::Timing::find_splitmuxsink(&pipeline).map(|sm| {
        let t = Arc::new(timing::Timing::default());
        t.connect(&sm, &pipeline);
        t
    });
    // §Design 6: nothing is installed unless XORGATE_CAPTURE_HOST_DIAG=1.
    let diag = diag::enabled_from_env().then(|| diag::Diag::install(&pipeline));
    install_sync_handler(&bus, &pipeline, &shared, timing.clone());

    let mut l = Launch {
        pipeline: pipeline.clone(),
        bus: bus.clone(),
        eos_on_shutdown,
        is_live: false,
        buffering: false,
        prerolled: false,
        in_progress: false,
        waiting_eos: false,
        tfthen: None,
        shared: shared.clone(),
        timing,
        pending_end: None,
        diag,
        playing_seen: false,
    };

    print(&tr(c"Setting pipeline to PAUSED ...\n"));
    let mut loop_ran = false;
    match pipeline.set_state(gst::State::Paused) {
        Err(_) => {
            printerr(&tr(c"Failed to set pipeline to PAUSED.\n"));
            shared
                .code
                .store(LEC_STATE_CHANGE_FAILURE, Ordering::SeqCst);
        }
        Ok(ret) => {
            match ret {
                gst::StateChangeSuccess::NoPreroll => {
                    print(&tr(c"Pipeline is live and does not need PREROLL ...\n"));
                    l.is_live = true;
                }
                gst::StateChangeSuccess::Async => print(&tr(c"Pipeline is PREROLLING ...\n")),
                gst::StateChangeSuccess::Success => {}
            }
            sys::arm(&bus);
            l.main_loop();
            loop_ran = true;
        }
    }
    if loop_ran {
        if let Some(then) = l.tfthen {
            let diff = gst::get_timestamp().saturating_sub(then);
            print(&trf(
                c"Execution ended after %u:%02u:%02u.%09u\n",
                &time_args(Some(diff)),
            ));
        }
    }
    // end:
    print(&tr(c"Setting pipeline to NULL ...\n"));
    let _ = pipeline.set_state(gst::State::Null);
    sys::disarm();
    print(&tr(c"Freeing pipeline ...\n"));
    bus.unset_sync_handler();
    drop(l);
    drop(bus);
    drop(pipeline);
    shared.code.load(Ordering::SeqCst)
}

/// `bus_sync_handler` (`gst-launch.c:945-1004`): runs on the POSTING thread.
/// Never blocks, takes no lock the loop holds, and a panic is contained.
///
/// WP1 additions (both no-ops without a splitmuxsink): the R5 clock check at
/// the pipeline's PLAYING, and the end stamp of `splitmuxsink-fragment-
/// closed`, read here on the posting thread so the clocks are read at the
/// close and not when the loop gets to the message.
fn install_sync_handler(
    bus: &gst::Bus,
    pipeline: &gst::Element,
    shared: &Arc<Shared>,
    timing: Option<Arc<timing::Timing>>,
) {
    let weak = pipeline.downgrade();
    let sh = shared.clone();
    bus.set_sync_handler(move |_bus, msg| {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Some(p) = weak.upgrade() else { return };
            match msg.view() {
                gst::MessageView::StateChanged(sc) if is_from(msg, &p) => {
                    let name = format!("{}_{}", state_name(sc.old()), state_name(sc.current()));
                    dump(&p, &format!("gst-launch.{name}"));
                    // "place a marker into e.g. strace logs" (an access(2) probe).
                    let _ = gst::glib::tmp_dir().join("gst-launch").join(&name).exists();
                    if sc.current() == gst::State::Playing {
                        if let Some(line) = timing.as_ref().and_then(|t| t.on_playing(&p)) {
                            printerr(&line);
                        }
                    }
                }
                gst::MessageView::Element(e) => {
                    if let (Some(t), Some(s)) = (timing.as_ref(), e.structure()) {
                        if s.name() == timing::FRAGMENT_CLOSED {
                            if let Some(ev) = t.on_fragment_closed(s, &p) {
                                timing::post(&p, ev);
                            }
                        }
                    }
                }
                gst::MessageView::Error(e) => {
                    dump(&p, "gst-launch.error");
                    let name = msg.src().map(|s| s.path_string()).unwrap_or_default();
                    printerr(&trf(
                        c"ERROR: from element %s: %s\n",
                        &[Arg::S(&name), Arg::S(e.error().message())],
                    ));
                    if let Some(d) = e.debug() {
                        printerr(&trf(c"Additional debug info:\n%s\n", &[Arg::S(&d)]));
                    }
                    if !sh.target_playing.load(Ordering::SeqCst) {
                        printerr(&tr(c"ERROR: pipeline doesn't want to preroll.\n"));
                    } else if sh.interrupting.load(Ordering::SeqCst) {
                        print(&tr(c"An error happened while waiting for EOS\n"));
                    }
                    sh.code.store(LEC_ERROR, Ordering::SeqCst);
                    sh.quit.store(true, Ordering::SeqCst);
                }
                _ => {}
            }
        }));
        gst::BusSyncReply::Pass
    });
}

struct Launch {
    pipeline: gst::Element,
    bus: gst::Bus,
    eos_on_shutdown: bool,
    is_live: bool,
    buffering: bool,
    prerolled: bool,
    in_progress: bool,
    waiting_eos: bool,
    tfthen: Option<gst::ClockTime>,
    shared: Arc<Shared>,
    /// WP1: exact fragment timing (record mode only).
    timing: Option<Arc<timing::Timing>>,
    /// The last `XG-FRAGMENT-END` seen, printed on the pipeline's EOS.
    pending_end: Option<(u32, String)>,
    /// WP1: the optional buffer-flow probes (`XORGATE_CAPTURE_HOST_DIAG=1`).
    diag: Option<diag::Diag>,
    /// The pipeline reached PLAYING once (nice sync, diag start).
    playing_seen: bool,
}

/// The loop's poll period: the late-signal check and the tty position line.
const TICK: Duration = Duration::from_millis(100);

impl Launch {
    /// `g_main_loop_run (loop)`: until EOS, an interrupt without `-e`, or an
    /// error. `g_main_loop_run` sets `is_running` on entry, so a quit posted
    /// by the sync handler BEFORE the loop started is lost (as in 1.22).
    fn main_loop(&mut self) {
        self.shared.quit.store(false, Ordering::SeqCst);
        let position = sys::stdout_is_tty();
        let mut next_position = Instant::now() + TICK;
        while !self.shared.quit.load(Ordering::SeqCst) {
            self.tick(position, &mut next_position);
        }
    }

    /// One loop iteration (≤ one [`TICK`]): hand any race-path signal to
    /// `deliver` (R-WP0-6 / R-WP0R-4), pop one bus message for up to 100 ms,
    /// the tty position line, then dispatch the message unless a quit landed
    /// meanwhile.
    fn tick(&mut self, position: bool, next_position: &mut Instant) {
        sys::poll_late();
        let msg = self.bus.timed_pop(gst::ClockTime::from_mseconds(100));
        if let Some(d) = self.diag.as_mut() {
            for line in d.check() {
                printerr(&line);
            }
        }
        if position && Instant::now() >= *next_position {
            *next_position = Instant::now() + TICK;
            self.print_position();
        }
        let Some(msg) = msg else { return };
        if self.shared.quit.load(Ordering::SeqCst) {
            return;
        }
        self.bus_handler(&msg);
    }

    fn quit(&self) {
        self.shared.quit.store(true, Ordering::SeqCst);
    }

    /// `do_initial_play` (`gst-launch.c:557-574`).
    fn do_initial_play(&mut self) {
        print(&tr(c"Setting pipeline to PLAYING ...\n"));
        self.tfthen = Some(gst::get_timestamp());
        if self.pipeline.set_state(gst::State::Playing).is_err() {
            printerr(&tr(c"ERROR: pipeline doesn't want to play.\n"));
            self.shared
                .code
                .store(LEC_STATE_CHANGE_FAILURE, Ordering::SeqCst);
            return;
        }
        self.shared.target_playing.store(true, Ordering::SeqCst);
    }

    /// `bus_handler` (`gst-launch.c:576-943`), minus the option-gated cases
    /// (`-m` messages, `-t` tags, `-c` toc, `-v` property notify).
    fn bus_handler(&mut self, msg: &gst::Message) {
        use gst::MessageView as V;
        match msg.view() {
            V::NewClock(c) => {
                let name = c.clock().map(|c| c.name().to_string());
                print(&format!(
                    "New clock: {}\n",
                    name.as_deref().unwrap_or("NULL")
                ));
            }
            V::ClockLost(_) => {
                print("Clock lost, selecting a new one\n");
                let _ = self.pipeline.set_state(gst::State::Paused);
                let _ = self.pipeline.set_state(gst::State::Playing);
            }
            V::Eos(_) => {
                // WP1: the last fragment's end, on a clean EOS only.
                if let (Some((id, line)), Some(t)) = (self.pending_end.take(), self.timing.as_ref())
                {
                    if t.last_open() == Some(id) {
                        print(&format!("{line}\n"));
                    }
                }
                print(&trf(
                    c"Got EOS from element \"%s\".\n",
                    &[Arg::S(&src_name(msg))],
                ));
                if self.eos_on_shutdown {
                    print(&tr(c"EOS received - stopping pipeline...\n"));
                }
                self.quit();
            }
            V::Info(i) => {
                if let Some(d) = i.debug() {
                    print(&trf(c"INFO:\n%s\n", &[Arg::S(&d)]));
                }
            }
            V::Warning(w) => {
                dump(&self.pipeline, "gst-launch.warning");
                let name = msg.src().map(|s| s.path_string()).unwrap_or_default();
                print(&trf(
                    c"WARNING: from element %s: %s\n",
                    &[Arg::S(&name), Arg::S(w.error().message())],
                ));
                if let Some(d) = w.debug() {
                    print(&trf(c"Additional debug info:\n%s\n", &[Arg::S(&d)]));
                }
            }
            V::StateChanged(sc) => {
                if !is_from(msg, &self.pipeline) {
                    return;
                }
                if sc.current() == gst::State::Playing && !self.playing_seen {
                    self.playing_seen = true;
                    // WP1: every thread at the main thread's nice (the
                    // agent's renice can land after `xg-sigwait` exists).
                    sys::nice_threads_like_main();
                    if let Some(d) = self.diag.as_mut() {
                        d.playing();
                    }
                }
                if !self.shared.target_playing.load(Ordering::SeqCst)
                    && sc.current() == gst::State::Paused
                {
                    self.prerolled = true;
                    print(&tr(c"Pipeline is PREROLLED ...\n"));
                    if self.buffering {
                        print(&tr(c"Prerolled, waiting for buffering to finish...\n"));
                        return;
                    }
                    if self.in_progress {
                        print(&tr(c"Prerolled, waiting for progress to finish...\n"));
                        return;
                    }
                    self.do_initial_play();
                }
            }
            V::Buffering(b) => {
                let percent = b.percent();
                print(&format!("{} {percent}%  \r", tr(c"buffering...")));
                if self.is_live {
                    return;
                }
                let playing = self.shared.target_playing.load(Ordering::SeqCst);
                if percent == 100 {
                    self.buffering = false;
                    if !playing {
                        self.do_initial_play();
                        return;
                    }
                    print(&tr(c"Done buffering, setting pipeline to PLAYING ...\n"));
                    let _ = self.pipeline.set_state(gst::State::Playing);
                } else {
                    if !self.buffering && playing {
                        print(&tr(c"Buffering, setting pipeline to PAUSED ...\n"));
                        let _ = self.pipeline.set_state(gst::State::Paused);
                    }
                    self.buffering = true;
                }
            }
            V::Latency(_) => {
                print(&tr(c"Redistribute latency...\n"));
                if let Some(bin) = self.pipeline.downcast_ref::<gst::Bin>() {
                    let _ = bin.recalculate_latency();
                }
            }
            V::RequestState(r) => {
                let state = r.requested_state();
                let name = msg.src().map(|s| s.path_string()).unwrap_or_default();
                print(&trf(
                    c"Setting state to %s as requested by %s...\n",
                    &[Arg::S(state_name(state)), Arg::S(&name)],
                ));
                let _ = self.pipeline.set_state(state);
            }
            V::Application(a) => {
                let Some(s) = a.structure() else { return };
                if s.name() == sys::SIGNAL_MESSAGE {
                    match s.get::<i32>("signo") {
                        Ok(libc::SIGINT) => self.intr_handler(),
                        Ok(libc::SIGHUP) => self.hup_handler(),
                        // A wake from the race path: poll it now.
                        Ok(0) => sys::poll_late(),
                        _ => {}
                    }
                } else if s.name() == timing::TIMING_MESSAGE {
                    match timing::decode(s) {
                        Some(timing::Event::Start(line)) => print(&format!("{line}\n")),
                        Some(timing::Event::ClockOff(line)) => printerr(&line),
                        Some(timing::Event::End(id, line)) => self.pending_end = Some((id, line)),
                        Some(timing::Event::Rejected(line))
                            if self.timing.as_ref().is_some_and(|t| t.first_rejection()) =>
                        {
                            printerr(&line)
                        }
                        _ => {}
                    }
                } else if s.name() == "GstLaunchInterrupt" {
                    print(&tr(c"Interrupt: Stopping pipeline ...\n"));
                    self.shared.interrupting.store(true, Ordering::SeqCst);
                    if self.eos_on_shutdown {
                        if self.waiting_eos {
                            print(&tr(
                                c"Interrupt while waiting for EOS - stopping pipeline...\n",
                            ));
                            self.quit();
                        } else {
                            print(&tr(
                                c"EOS on shutdown enabled -- Forcing EOS on the pipeline\n",
                            ));
                            self.pipeline.send_event(gst::event::Eos::new());
                            print(&tr(c"Waiting for EOS...\n"));
                            self.waiting_eos = true;
                        }
                    } else {
                        self.quit();
                    }
                }
            }
            V::Progress(p) => {
                let (ty, code, text) = p.get();
                match ty {
                    gst::ProgressType::Start | gst::ProgressType::Continue => {
                        self.in_progress = true
                    }
                    gst::ProgressType::Complete
                    | gst::ProgressType::Canceled
                    | gst::ProgressType::Error => self.in_progress = false,
                    _ => {}
                }
                print(&trf(c"Progress: (%s) %s\n", &[Arg::S(code), Arg::S(text)]));
                if !self.in_progress
                    && self.prerolled
                    && !self.shared.target_playing.load(Ordering::SeqCst)
                {
                    self.do_initial_play();
                }
            }
            V::Element(e) => {
                if let Some(s) = e.structure() {
                    if s.name() == "missing-plugin" {
                        let desc = s.get::<&str>("name").ok();
                        print(&trf(
                            c"Missing element: %s\n",
                            &[Arg::S(desc.unwrap_or("(no description)"))],
                        ));
                    }
                }
            }
            V::HaveContext(h) => {
                let ctx = h.context();
                print(&trf(
                    c"Got context from element '%s': %s=%s\n",
                    &[
                        Arg::S(&src_name(msg)),
                        Arg::S(ctx.context_type()),
                        Arg::S(&ctx.structure().to_string()),
                    ],
                ));
            }
            _ => {}
        }
    }

    /// `intr_handler` (`gst-launch.c:504-520`), dispatched on the loop thread,
    /// in gst-launch's order: print `handling interrupt.`, post the
    /// interrupt, and only then remove the one-shot watch (G_SOURCE_REMOVE →
    /// SIG_DFL). A second SIGINT inside that window coalesces into the
    /// pending dispatch (swallowed), one after it takes the default action.
    ///
    /// WP1 (mutation MH of review-wp0-r2, adopted; D50 dropped): on a RELEASE
    /// build the rig transcripts 1 (`SigCgt@after_intr`) and 2b (death by the
    /// second SIGINT) PASS with this order (2 × 15/15). The window is the
    /// same mechanism as gst-launch's but narrower (the host's print → post →
    /// removal is a few µs; GLib's return-and-remove ~50-75 µs: a second
    /// SIGINT 15-43 µs after the line was swallowed 4/10 by gst-launch, 0/10
    /// by the host): timing only, not an ordering difference.
    fn intr_handler(&mut self) {
        print("handling interrupt.\n");
        let s = gst::Structure::builder("GstLaunchInterrupt")
            .field("message", "Pipeline interrupted")
            .build();
        let _ = self.pipeline.post_message(
            gst::message::Application::builder(s)
                .src(&self.pipeline)
                .build(),
        );
        sys::dispatched(libc::SIGINT);
    }

    /// `hup_handler` (`gst-launch.c:523-540`).
    fn hup_handler(&mut self) {
        if std::env::var_os("GST_DEBUG_DUMP_DOT_DIR").is_some() {
            print("SIGHUP: dumping dot file snapshot ...\n");
        } else {
            print(
                "SIGHUP: not dumping dot file snapshot, GST_DEBUG_DUMP_DOT_DIR environment variable not set.\n",
            );
        }
        dump(&self.pipeline, "gst-launch.snapshot");
        sys::dispatched(libc::SIGHUP);
    }

    /// `query_pipeline_position` (`gst-launch.c:1006-1038`), tty stdout only.
    fn print_position(&self) {
        if self.buffering {
            return;
        }
        let Some(pos) = self.pipeline.query_position::<gst::ClockTime>() else {
            return;
        };
        let dur = self.pipeline.query_duration::<gst::ClockTime>();
        let p = time_string(Some(pos));
        let d = time_string(dur);
        let (p, d) = (&p[..p.len().min(9)], &d[..d.len().min(9)]);
        match dur {
            Some(dur) if dur.nseconds() > 0 && dur >= pos => {
                let percent = 100.0 * pos.nseconds() as f64 / dur.nseconds() as f64;
                print(&format!("{p} / {d} ({percent:.1} %)\r"));
            }
            _ => print(&format!("{p} / {d}\r")),
        }
    }
}

fn is_from(msg: &gst::Message, el: &gst::Element) -> bool {
    msg.src()
        .is_some_and(|s| s.as_ptr() == el.upcast_ref::<gst::Object>().as_ptr())
}

/// `GST_MESSAGE_SRC_NAME`.
fn src_name(msg: &gst::Message) -> String {
    msg.src()
        .map(|s| s.name().to_string())
        .unwrap_or_else(|| "(NULL)".to_string())
}

/// `gst_element_state_get_name`.
fn state_name(s: gst::State) -> &'static str {
    match s {
        gst::State::VoidPending => "VOID_PENDING",
        gst::State::Null => "NULL",
        gst::State::Ready => "READY",
        gst::State::Paused => "PAUSED",
        gst::State::Playing => "PLAYING",
    }
}

/// `GST_DEBUG_BIN_TO_DOT_FILE_WITH_TS (bin, GST_DEBUG_GRAPH_SHOW_ALL, name)`
/// (pinned by `tests/capture_host.rs::dot_dumps_use_show_all_without_full_params`).
fn dump(p: &gst::Element, name: &str) {
    if let Some(bin) = p.downcast_ref::<gst::Bin>() {
        bin.debug_to_dot_file_with_ts(gst::DebugGraphDetails::ALL, name);
    }
}

/// `GST_TIME_ARGS` (`CLOCK_TIME_NONE` → 99:99:99.999999999).
fn time_args(t: Option<gst::ClockTime>) -> [Arg<'static>; 4] {
    match t {
        Some(t) => {
            let ns = t.nseconds();
            let s = 1_000_000_000u64;
            [
                Arg::U(ns / (s * 3600)),
                Arg::U((ns / (s * 60)) % 60),
                Arg::U((ns / s) % 60),
                Arg::U(ns % s),
            ]
        }
        None => [Arg::U(99), Arg::U(99), Arg::U(99), Arg::U(999_999_999)],
    }
}

fn time_string(t: Option<gst::ClockTime>) -> String {
    nls::format("%u:%02u:%02u.%09u", &time_args(t))
}

/// `gst_print`: stdout, flushed per call (GLib's default print handler).
fn print(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

/// `gst_printerr`: stderr.
fn printerr(s: &str) {
    let mut e = std::io::stderr().lock();
    let _ = e.write_all(s.as_bytes());
    let _ = e.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn options_are_gst_launch_s_e_only_and_tokens_stay_verbatim() {
        let a = parse_args(&os(&[
            "-e",
            "videotestsrc",
            "!",
            "splitmuxsink",
            "muxer=mp4mux fragment-duration=1000",
        ]))
        .unwrap();
        assert!(a.eos_on_shutdown);
        assert_eq!(a.tokens.len(), 4);
        assert_eq!(a.tokens[3], b"muxer=mp4mux fragment-duration=1000");
        let b = parse_args(&os(&["videotestsrc", "!", "fakesink"])).unwrap();
        assert!(!b.eos_on_shutdown);
        // A lone "-" and tokens after "--" are pipeline tokens.
        let c = parse_args(&os(&["--", "-e", "-"])).unwrap();
        assert!(!c.eos_on_shutdown);
        assert_eq!(c.tokens, vec![b"-e".to_vec(), b"-".to_vec()]);
        assert_eq!(
            parse_args(&os(&["-v", "fakesrc"])).unwrap_err(),
            "Unknown option -v"
        );
    }

    /// R-WP0R-4: a race-path signal (the fallback handler recorded it in
    /// `LATE` while the watch is ARMED) is dispatched within ONE loop tick:
    /// `tick` polls it, the bus pops the private signal message, and
    /// `intr_handler` runs (the watch goes OFF, `GstLaunchInterrupt` is
    /// posted). Deleting the poll leaves the watch ARMED after the tick.
    #[test]
    fn a_race_path_signal_is_dispatched_within_one_tick() {
        let _g = sys::testing::lock();
        gst::init().unwrap();
        let pipeline: gst::Element = gst::Pipeline::new().upcast();
        let bus = pipeline.bus().unwrap();
        let mut l = Launch {
            pipeline: pipeline.clone(),
            bus: bus.clone(),
            eos_on_shutdown: true,
            is_live: false,
            buffering: false,
            prerolled: false,
            in_progress: false,
            waiting_eos: false,
            tfthen: None,
            shared: Arc::new(Shared::default()),
            timing: None,
            pending_end: None,
            diag: None,
            playing_seen: false,
        };
        sys::testing::prime_race_path_sigint(&bus);
        let mut next = Instant::now() + TICK;
        let t0 = Instant::now();
        l.tick(false, &mut next);
        assert!(t0.elapsed() < TICK * 2, "{:?}", t0.elapsed());
        assert!(
            sys::testing::intr_is_off(),
            "the race-path SIGINT was not dispatched within one tick"
        );
        let msg = bus.pop().expect("GstLaunchInterrupt posted");
        let name = match msg.view() {
            gst::MessageView::Application(a) => a.structure().map(|s| s.name().to_string()),
            _ => None,
        };
        assert_eq!(name.as_deref(), Some("GstLaunchInterrupt"));
        sys::testing::reset();
    }

    /// WP1 (MH adopted, D50 dropped): gst-launch's order. When the
    /// interrupt is POSTED the SIGINT watch is still installed (a second
    /// SIGINT then coalesces); it is removed only after, before the handler
    /// returns. The old host order removed it before the print.
    #[test]
    fn intr_handler_removes_the_watch_only_after_print_and_post() {
        let _g = sys::testing::lock();
        sys::testing::reset();
        gst::init().unwrap();
        let pipeline: gst::Element = gst::Pipeline::new().upcast();
        let bus = pipeline.bus().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(None::<bool>));
        let s2 = seen.clone();
        bus.set_sync_handler(move |_, m| {
            if let gst::MessageView::Application(a) = m.view() {
                if a.structure()
                    .is_some_and(|s| s.name() == "GstLaunchInterrupt")
                {
                    *s2.lock().unwrap() = Some(sys::testing::intr_is_off());
                }
            }
            gst::BusSyncReply::Pass
        });
        let mut l = Launch {
            pipeline: pipeline.clone(),
            bus: bus.clone(),
            eos_on_shutdown: true,
            is_live: false,
            buffering: false,
            prerolled: false,
            in_progress: false,
            waiting_eos: false,
            tfthen: None,
            shared: Arc::new(Shared::default()),
            timing: None,
            pending_end: None,
            diag: None,
            playing_seen: false,
        };
        sys::testing::prime_race_path_sigint(&bus); // INTR = ARMED
        sys::testing::take_late_for_test();
        sys::testing::set_intr_pending();
        l.intr_handler();
        assert_eq!(
            *seen.lock().unwrap(),
            Some(false),
            "watch removed before the post"
        );
        assert!(sys::testing::intr_is_off(), "watch not removed at all");
        bus.unset_sync_handler();
        sys::testing::reset();
    }

    #[test]
    fn time_args_match_gst_time_format() {
        assert_eq!(
            time_string(Some(gst::ClockTime::from_nseconds(3_723_000_000_123))),
            "1:02:03.000000123"
        );
        assert_eq!(time_string(None), "99:99:99.999999999");
    }
}
