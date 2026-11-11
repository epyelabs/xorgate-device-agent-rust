//! The capture host's process-level plumbing: the process name, the signal
//! dispositions gst-launch 1.22 has, the signal-wait thread, the fault
//! handler, the locale. Every `unsafe` of the host lives here.
//!
//! Signal model (phase-7a §Design 5, R3), measured on the rig
//! (`fixtures/capture-host/gst-launch-1.22.0-debian-12.13/`): gst-launch
//! catches SIGSEGV + SIGQUIT (its fault handler, from startup) and, from
//! `g_unix_signal_add` right after `set_state(PAUSED)`, SIGINT + SIGHUP;
//! glibc's SIGSETXID (33) is caught in every glibc process; nothing is
//! ignored; no thread but GLib's `gmain` blocks anything. GLib restores
//! SIG_DFL when a watch is removed, so the FIRST SIGINT is handled and a
//! second one kills the process (scenario 2b); SIGTERM is never touched.
//!
//! The host reproduces SigCgt/SigIgn bit for bit and the behaviour, with a
//! different mechanism: SIGINT and SIGHUP are BLOCKED in the main thread
//! before any other thread exists (every GStreamer/libcamera thread inherits
//! the mask) and ONE thread `sigwait`s for them. While a watch is armed its
//! signal has a (never-run) handler installed so `/proc` SigCgt shows it,
//! exactly like GLib's; a disarmed signal gets the default action: the wait
//! thread restores SIG_DFL, unblocks it for itself and re-raises, so the
//! process dies by that signal as gst-launch does.

use std::ffi::CStr;
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::Mutex;

use nix::sys::signal::{SigSet, Signal};

/// Watch states of one signal (SIGINT / SIGHUP).
const OFF: u8 = 0; // no watch: the default action
const ARMED: u8 = 1; // watched: the next signal is dispatched
const PENDING: u8 = 2; // received, not yet dispatched by the bus loop (coalesces)

static INTR: AtomicU8 = AtomicU8::new(OFF);
static HUP: AtomicU8 = AtomicU8::new(OFF);

/// The structure name of the private bus message the wait thread posts for a
/// watched signal (the bus loop is the GLib main-context dispatch).
pub const SIGNAL_MESSAGE: &str = "XorgateCaptureSignal";

/// Where the wait thread posts (set while the pipeline exists).
static POST: Mutex<Option<gstreamer::Bus>> = Mutex::new(None);

fn state(sig: i32) -> &'static AtomicU8 {
    if sig == libc::SIGINT {
        &INTR
    } else {
        &HUP
    }
}

/// `PR_SET_NAME`: `comm` (and so `ps`/`pkill -x`) reads `xorgate-capture`.
pub fn set_process_name(name: &CStr) {
    let _ = nix::sys::prctl::set_name(name);
}

fn sigset(sigs: &[Signal]) -> SigSet {
    let mut set = SigSet::empty();
    for &s in sigs {
        set.add(s);
    }
    set
}

#[allow(unsafe_code)]
fn set_disposition(sig: i32, handler: libc::sighandler_t) {
    // SAFETY: a plain sigaction(2) with an empty mask; `handler` is SIG_DFL
    // or one of this module's `extern "C"` handlers.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

/// The handler a watched signal carries while ARMED. The signal is blocked in
/// every thread, so this only runs if some library thread unblocked it, or
/// in the `default_action` race (R-WP0-6) on the wait thread itself.
/// Async-signal-safe: atomics and the raw `gettid`/`tgkill` syscalls only.
///
/// - On another thread (WP1 residual (a)): the signal is FORWARDED to the
///   wait thread (`tgkill`; it has the signal blocked, so it becomes
///   pending there and `sigwait` returns it at once) with its [`FWD`] bit
///   set, so it is dispatched immediately instead of at the next loop tick.
/// - On the wait thread (the race): recorded in [`LATE`]; `default_action`
///   then wakes the bus loop ([`post_wake`]), which polls it.
///
/// Either way the signal is handled as GLib handles one its handler caught
/// ([`deliver_late`]): dispatched, coalesced, or swallowed — never fatal.
extern "C" fn watched_handler(sig: libc::c_int) {
    let bit = if sig == libc::SIGINT { 1 } else { 2 };
    let wait = WAIT_THREAD.load(Ordering::SeqCst);
    if wait != 0 && nix::unistd::gettid().as_raw() != wait {
        FWD.fetch_or(bit, Ordering::SeqCst);
        if thread_kill(wait, sig) {
            return;
        }
        // The forward failed: fall back to the loop's poll.
        FWD.fetch_and(!bit, Ordering::SeqCst);
    }
    LATE.fetch_or(bit, Ordering::SeqCst);
}

/// Signals that reached [`watched_handler`] on the wait thread, or whose
/// forward failed (bit 1 = SIGINT, 2 = SIGHUP); polled by the bus loop.
static LATE: AtomicU8 = AtomicU8::new(0);
/// Signals [`watched_handler`] forwarded to the wait thread (same bits).
static FWD: AtomicU8 = AtomicU8::new(0);
/// The wait thread's tid (0 = not started yet).
static WAIT_THREAD: AtomicI32 = AtomicI32::new(0);

/// `tgkill(getpid(), tid, sig)`: a signal to one thread of this process.
/// Async-signal-safe (two raw syscalls).
#[allow(unsafe_code)]
fn thread_kill(tid: i32, sig: i32) -> bool {
    // SAFETY: raw getpid/tgkill syscalls with plain integer arguments.
    unsafe {
        let pid = libc::syscall(libc::SYS_getpid);
        libc::syscall(
            libc::SYS_tgkill,
            pid,
            libc::c_long::from(tid),
            libc::c_long::from(sig),
        ) == 0
    }
}

/// Startup, BEFORE any thread exists (`main` dispatches here first):
/// undo what the Rust runtime set that gst-launch does not have (SIGPIPE
/// ignored, SIGBUS's stack-overflow handler), block SIGINT + SIGHUP and start
/// the wait thread. SIGSEGV's Rust handler is replaced by [`fault_setup`].
pub fn init_signals() -> std::io::Result<()> {
    set_disposition(libc::SIGPIPE, libc::SIG_DFL);
    set_disposition(libc::SIGBUS, libc::SIG_DFL);
    block(&[Signal::SIGINT, Signal::SIGHUP]);
    std::thread::Builder::new()
        .name("xg-sigwait".into())
        .spawn(wait_loop)?;
    Ok(())
}

fn block(sigs: &[Signal]) {
    let _ = sigset(sigs).thread_block();
}

/// The default action of `sig`, now, from the calling thread (SIGINT and
/// SIGHUP default to Term, so the process dies by `sig`).
///
/// It returns only in one race (R-WP0-6): `arm()` installed
/// [`watched_handler`] between the `SIG_DFL` below and `raise`, so the
/// handler ran instead and recorded the signal in [`LATE`]. That is
/// gst-launch's "the signal arrived just as `g_unix_signal_add` installed
/// GLib's handler": it is dispatched. The signal is re-blocked on this thread
/// and the bus loop's `take_late()` hands it to [`deliver`], now ARMED. It
/// never `exit()`s off the main thread.
fn default_action(sig: Signal) {
    set_disposition(sig as i32, libc::SIG_DFL);
    #[cfg(test)]
    tests::before_raise();
    let set = sigset(&[sig]);
    // Unblock `sig` for this thread only and raise it at this thread.
    let _ = set.thread_unblock();
    let _ = nix::sys::signal::raise(sig);
    // Still alive: the race above. Hand the signal to the loop, and wake it
    // now rather than at its next 100 ms tick (WP1 residual (a)).
    let _ = set.thread_block();
    if LATE.load(Ordering::SeqCst) != 0 {
        post_wake();
    }
}

fn wait_loop() {
    WAIT_THREAD.store(nix::unistd::gettid().as_raw(), Ordering::SeqCst);
    let set = sigset(&[Signal::SIGINT, Signal::SIGHUP]);
    loop {
        // sigwait on signals this thread (and every other) blocks.
        if let Ok(sig) = set.wait() {
            on_waited(sig as i32);
        }
    }
}

/// One signal `sigwait` returned on the wait thread (R-WP1-3: the unit the
/// tests drive). Forwarded by `watched_handler` (its [`FWD`] bit set): a
/// signal GLib's handler would have caught, so never fatal
/// ([`deliver_late`]). (A genuine one coalesced with it is handled the same
/// way: a µs-wide double race.) Otherwise a genuine signal ([`deliver`]).
fn on_waited(sig: i32) {
    let bit = if sig == libc::SIGINT { 1 } else { 2 };
    if FWD.fetch_and(!bit, Ordering::SeqCst) & bit != 0 {
        deliver_late(sig);
    } else {
        deliver(sig);
    }
}

/// One received SIGINT/SIGHUP: dispatch it if watched, coalesce it if a
/// dispatch is pending (GLib's handler only sets a flag), else die by it.
pub fn deliver(sig: i32) {
    let st = state(sig);
    match st.compare_exchange(ARMED, PENDING, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => post_signal(sig),
        Err(PENDING) => {}
        Err(_) => default_action(if sig == libc::SIGINT {
            Signal::SIGINT
        } else {
            Signal::SIGHUP
        }),
    }
}

/// A signal that reached [`watched_handler`]: GLib's handler caught it, so
/// it is never fatal (WP1 residual (b)). Watched → dispatched; pending →
/// coalesced; the watch already removed (dispatched, or `disarm`) →
/// swallowed, as GLib swallows a signal its handler catches with no source
/// left. This removes the start-up inversion where a race-path SIGINT,
/// polled after a second SIGINT had already been dispatched, killed the
/// host while GLib coalesces both.
pub fn deliver_late(sig: i32) {
    let st = state(sig);
    if st
        .compare_exchange(ARMED, PENDING, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        post_signal(sig);
    }
}

/// Wakes the bus loop (a `signo: 0` message): it then polls [`LATE`].
fn post_wake() {
    post_signal(0);
}

/// Posts while HOLDING the `POST` lock, so once `disarm()` has taken the lock
/// and emptied it, no post is in flight or can start (R-WP0-10). The bus's
/// sync handler runs inside `post` on this thread and never takes `POST`.
fn post_signal(sig: i32) {
    let Ok(g) = POST.lock() else { return };
    if let Some(bus) = g.as_ref() {
        let s = gstreamer::Structure::builder(SIGNAL_MESSAGE)
            .field("signo", sig)
            .build();
        let _ = bus.post(gstreamer::message::Application::new(s));
    }
}

/// The bus loop's per-tick poll: hand every signal that reached the
/// fallback handler (a library thread with it unblocked, or the
/// `default_action` race) to [`deliver`].
pub fn poll_late() {
    for sig in take_late() {
        deliver_late(sig);
    }
}

/// Signals that reached the fallback handler since the last call.
pub fn take_late() -> Vec<i32> {
    let bits = LATE.swap(0, Ordering::SeqCst);
    let mut v = Vec::new();
    if bits & 1 != 0 {
        v.push(libc::SIGINT);
    }
    if bits & 2 != 0 {
        v.push(libc::SIGHUP);
    }
    v
}

/// `g_unix_signal_add(SIGINT)` + `(SIGHUP)` (`gst-launch.c:1288-1291`).
pub fn arm(bus: &gstreamer::Bus) {
    if let Ok(mut g) = POST.lock() {
        *g = Some(bus.clone());
    }
    // ARMED first, then the handler: a signal the handler catches always
    // finds its watch armed (or later), never OFF (WP1).
    for sig in [libc::SIGINT, libc::SIGHUP] {
        state(sig).store(ARMED, Ordering::SeqCst);
        set_disposition(sig, watched_handler as *const () as libc::sighandler_t);
    }
}

/// The bus loop dispatches a received signal. SIGINT's watch is one-shot
/// (`intr_handler` returns G_SOURCE_REMOVE → GLib restores SIG_DFL), called
/// at the END of `intr_handler`, so a second SIGINT before it coalesces
/// (GLib swallows it: its source is removed); SIGHUP's stays
/// (G_SOURCE_CONTINUE), called at the START of `hup_handler`, as GLib clears
/// the source's `pending` before the callback (ci-sigint-race, drops D47).
pub fn dispatched(sig: i32) {
    if sig == libc::SIGINT {
        INTR.store(OFF, Ordering::SeqCst);
        set_disposition(libc::SIGINT, libc::SIG_DFL);
    } else {
        let _ = HUP.compare_exchange(PENDING, ARMED, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// `g_source_remove` of both watches at `end:` (`gst-launch.c:1345-1349`).
pub fn disarm() {
    for sig in [libc::SIGINT, libc::SIGHUP] {
        state(sig).store(OFF, Ordering::SeqCst);
        set_disposition(sig, libc::SIG_DFL);
    }
    if let Ok(mut g) = POST.lock() {
        *g = None;
    }
}

// ---------------------------------------------------------------------------
// fault handler (`gst-launch.c:98-163`)
// ---------------------------------------------------------------------------

/// `fault_setup()`: SIGSEGV + SIGQUIT → [`fault_handler`] (replacing the Rust
/// runtime's SIGSEGV stack-overflow handler).
pub fn fault_setup() {
    set_disposition(
        libc::SIGSEGV,
        fault_handler as *const () as libc::sighandler_t,
    );
    set_disposition(
        libc::SIGQUIT,
        fault_handler as *const () as libc::sighandler_t,
    );
}

/// `fault_handler_sighandler` + `fault_spin`: restore the defaults, say what
/// was caught, then sleep forever (the agent's watchdog or stop SIGKILLs us,
/// as it does a spinning gst-launch). Async-signal-safe: write(2) + sleep(3)
/// only. `g_on_error_stack_trace` (which execs gdb) is not reproduced.
#[allow(unsafe_code)]
extern "C" fn fault_handler(sig: libc::c_int) {
    set_disposition(libc::SIGSEGV, libc::SIG_DFL);
    set_disposition(libc::SIGQUIT, libc::SIG_DFL);
    // SAFETY: write(2)/isatty(3)/getpid(2)/sleep(3) are async-signal-safe;
    // the buffers are stack/static.
    unsafe {
        if sig == libc::SIGSEGV {
            raw_write(2, b"Caught SIGSEGV\n");
        } else if sig == libc::SIGQUIT {
            // gst-launch printf()s this to stdout and never flushes it, so it
            // only shows when stdout is line-buffered (a tty).
            if libc::isatty(1) == 1 {
                raw_write(1, b"Caught SIGQUIT\n");
            }
        } else {
            raw_write(2, b"signo:  ");
            let mut b = [0u8; 12];
            raw_write(2, itoa(sig as i64, &mut b));
            raw_write(2, b"\n");
        }
        raw_write(2, b"Spinning.  Please run 'gdb gst-launch-1.0 ");
        let mut b = [0u8; 12];
        raw_write(2, itoa(i64::from(libc::getpid()), &mut b));
        raw_write(
            2,
            b"' to continue debugging, Ctrl-C to quit, or Ctrl-\\ to dump core.\n",
        );
        loop {
            libc::sleep(1);
        }
    }
}

#[allow(unsafe_code)]
unsafe fn raw_write(fd: i32, b: &[u8]) {
    // SAFETY: caller passes a valid fd number and a live buffer.
    unsafe {
        libc::write(fd, b.as_ptr().cast(), b.len());
    }
}

/// Decimal digits of `v` into the tail of `buf` (no allocation).
fn itoa(mut v: i64, buf: &mut [u8; 12]) -> &[u8] {
    let neg = v < 0;
    let mut i = buf.len();
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while v != 0 && i > 1 {
        i -= 1;
        buf[i] = b'0' + (v % 10).unsigned_abs() as u8;
        v /= 10;
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    &buf[i..]
}

/// `setlocale (LC_ALL, "")` (`gst-launch.c:1145`): GStreamer's own messages
/// (and [`super::nls`]) follow the environment's locale as gst-launch's do.
#[allow(unsafe_code)]
pub fn setlocale_from_env() {
    // SAFETY: called on the main thread before ANY other thread exists
    // (`capture_host::main` spawns the signal-wait thread afterwards), so
    // nothing can read locale data concurrently.
    unsafe {
        libc::setlocale(libc::LC_ALL, c"".as_ptr());
    }
}

/// Nice inheritance (phase-7a §Design 5, WP1): the agent renices the host's
/// pid (= its main thread; Linux nice is per thread) right after spawn, and
/// every thread created after that inherits the value. The one thread that
/// can predate the renice is `xg-sigwait` (started first, R3). At PLAYING
/// the bus loop raises every thread whose nice is BELOW the main thread's to
/// the main thread's value (raising nice needs no privilege; a thread is
/// never made less nice). Returns the number of threads changed.
#[allow(unsafe_code)]
pub fn nice_threads_like_main() -> usize {
    let pid = std::process::id();
    let prio = |tid: u32| thread_nice(&format!("/proc/self/task/{tid}/stat"));
    let Some(main) = prio(pid) else { return 0 };
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    let mut changed = 0;
    for t in tasks.flatten() {
        let Some(tid) = t.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if tid == pid || prio(tid).is_none_or(|n| n >= main) {
            continue;
        }
        // SAFETY: setpriority(2) on a thread of this process, to a nice
        // value no lower than its own (an unprivileged raise).
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, main) } == 0 {
            changed += 1;
        }
    }
    changed
}

/// The nice value (field 19) of a `/proc/<pid>/task/<tid>/stat`.
pub fn thread_nice(stat_path: &str) -> Option<i32> {
    let st = std::fs::read_to_string(stat_path).ok()?;
    // `pid (comm) state ppid …`: comm may hold spaces/parens; split after
    // the LAST ')'. Field 19 (nice) is the 17th after it.
    st.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(16)?
        .parse()
        .ok()
}

/// `isatty (STDOUT_FILENO)` (position printing, `gst-launch.c:1298`).
pub fn stdout_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

// ---------------------------------------------------------------------------
// GStreamer calls that need raw FFI
// ---------------------------------------------------------------------------

/// The outcome of `gst_parse_launchv` (`gst-launch.c:1187-1206`).
pub enum Parsed {
    /// A pipeline (or element) and no error.
    Ok(gstreamer::Element),
    /// A partially built pipeline AND an error (`WARNING: erroneous pipeline`).
    Erroneous(String),
    /// NULL (`ERROR: pipeline could not be constructed[: …]`).
    Null(Option<String>),
}

/// `gst_parse_launchv (argvn, &error)`: the raw call, because gst-launch
/// distinguishes a NULL result from a partial one and the safe wrapper
/// cannot.
#[allow(unsafe_code)]
pub fn parse_launchv(tokens: &[std::ffi::CString]) -> Parsed {
    use gstreamer::glib::translate::{from_glib_full, from_glib_none};
    let mut argv: Vec<*const libc::c_char> = tokens.iter().map(|t| t.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut err: *mut gstreamer::glib::ffi::GError = std::ptr::null_mut();
    // SAFETY: `argv` is a NULL-terminated array of NUL-terminated strings
    // that outlive the call; gst is initialised (caller); `err` is an
    // out-pointer initialised to NULL.
    let el = unsafe { gstreamer::ffi::gst_parse_launchv(argv.as_mut_ptr(), &mut err) };
    let error: Option<gstreamer::glib::Error> = if err.is_null() {
        None
    } else {
        // SAFETY: a non-NULL GError we own (transfer full).
        Some(unsafe { from_glib_full(err) })
    };
    let element: Option<gstreamer::Element> = if el.is_null() {
        None
    } else {
        // SAFETY: transfer floating; from_glib_none ref-sinks it (what the
        // safe `gst::parse::launchv*` wrappers do).
        Some(unsafe { from_glib_none(el) })
    };
    match (element, error) {
        (Some(e), None) => Parsed::Ok(e),
        (Some(_), Some(err)) => Parsed::Erroneous(err.message().to_string()),
        (None, err) => Parsed::Null(err.map(|e| e.message().to_string())),
    }
}

/// `gst_deinit ()` (`gst-launch.c:1363`), after every GStreamer object of
/// the run was dropped.
#[allow(unsafe_code)]
pub fn deinit() {
    // SAFETY: the caller (`capture_host::main`) has dropped the pipeline,
    // its bus and every message. The wait thread touches GStreamer only in
    // `post_signal`, under the `POST` lock; `disarm()` took that lock (so no
    // post was in flight) and emptied it (so none can start). After a run
    // that never armed, `POST` was never set.
    unsafe { gstreamer::deinit() }
}

/// Test hooks for `capture_host` unit tests (the statics are process-wide).
#[cfg(test)]
pub(super) mod testing {
    use super::*;

    static LOCK: Mutex<()> = Mutex::new(());

    /// Serialises the tests that touch `LATE` / the watch states / `POST`.
    pub fn lock() -> std::sync::MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// As if a library thread caught a SIGINT while the watch is ARMED:
    /// `POST` = `bus`, INTR = ARMED, `LATE` holds SIGINT. No real handler is
    /// installed in the test process.
    pub fn prime_race_path_sigint(bus: &gstreamer::Bus) {
        *POST.lock().unwrap() = Some(bus.clone());
        INTR.store(ARMED, Ordering::SeqCst);
        LATE.store(1, Ordering::SeqCst);
    }

    pub fn take_late_for_test() {
        LATE.store(0, Ordering::SeqCst);
    }

    /// As if the wait thread had taken the ARMED watch to PENDING.
    pub fn set_intr_pending() {
        INTR.store(PENDING, Ordering::SeqCst);
    }

    pub fn intr_is_off() -> bool {
        INTR.load(Ordering::SeqCst) == OFF
    }

    /// As if the wait thread had posted an ARMED SIGHUP (now PENDING) and the
    /// loop popped it: `POST` = `bus`.
    pub fn prime_hup_pending(bus: &gstreamer::Bus) {
        *POST.lock().unwrap() = Some(bus.clone());
        HUP.store(PENDING, Ordering::SeqCst);
    }

    pub fn hup_is_pending() -> bool {
        HUP.load(Ordering::SeqCst) == PENDING
    }

    pub fn reset() {
        INTR.store(OFF, Ordering::SeqCst);
        HUP.store(OFF, Ordering::SeqCst);
        LATE.store(0, Ordering::SeqCst);
        FWD.store(0, Ordering::SeqCst);
        WAIT_THREAD.store(0, Ordering::SeqCst);
        *POST.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs between `default_action`'s `SIG_DFL` and its `raise` (tests only).
    pub(super) static BEFORE_RAISE: Mutex<Option<fn()>> = Mutex::new(None);

    pub(super) fn before_raise() {
        let hook = *BEFORE_RAISE.lock().unwrap();
        if let Some(f) = hook {
            f();
        }
    }

    /// R-WP0-6, forced: `arm()` installs the watch between `SIG_DFL` and
    /// `raise`. The process must NOT die or exit: the handler records the
    /// signal, `default_action` re-blocks it and returns, and `take_late`
    /// hands it to the loop. SIGHUP: nothing else in this test binary uses it.
    #[test]
    fn a_watch_armed_mid_default_action_hands_the_signal_to_the_loop() {
        let _g = testing::lock();
        fn arm_now() {
            set_disposition(
                libc::SIGHUP,
                watched_handler as *const () as libc::sighandler_t,
            );
        }
        let set = sigset(&[Signal::SIGHUP]);
        set.thread_block().unwrap(); // as on the wait thread
        LATE.store(0, Ordering::SeqCst);
        *BEFORE_RAISE.lock().unwrap() = Some(arm_now);
        default_action(Signal::SIGHUP);
        *BEFORE_RAISE.lock().unwrap() = None;
        assert_eq!(take_late(), vec![libc::SIGHUP]);
        assert!(SigSet::thread_get_mask().unwrap().contains(Signal::SIGHUP));
        set_disposition(libc::SIGHUP, libc::SIG_DFL);
        set.thread_unblock().unwrap();
    }

    fn signal_messages(bus: &gstreamer::Bus) -> Vec<i32> {
        let mut v = Vec::new();
        while let Some(m) = bus.pop() {
            if let gstreamer::MessageView::Application(a) = m.view() {
                if let Some(s) = a.structure() {
                    if s.name() == SIGNAL_MESSAGE {
                        v.push(s.get::<i32>("signo").unwrap());
                    }
                }
            }
        }
        v
    }

    fn test_bus() -> gstreamer::Bus {
        gstreamer::init().unwrap();
        gstreamer::Bus::new()
    }

    /// WP1 residual (b): a signal the handler caught (LATE) whose watch is
    /// already gone is SWALLOWED, as GLib swallows a signal its handler
    /// catches with no source left; it never takes the default action. The
    /// start-up inversion, forced: SIGINT #1 lands on the race path (LATE),
    /// SIGINT #2 is dispatched first and removes the watch, then the loop
    /// polls #1. Before WP1 that poll killed the host (death by SIGINT; GLib
    /// coalesces both). SIGHUP alone, OFF + LATE, is the same rule.
    #[test]
    fn a_late_signal_after_its_watch_is_gone_is_swallowed() {
        let _g = testing::lock();
        testing::reset();
        let bus = test_bus();
        *POST.lock().unwrap() = Some(bus.clone());
        INTR.store(ARMED, Ordering::SeqCst);
        LATE.store(1, Ordering::SeqCst); // #1, race path
        deliver(libc::SIGINT); // #2, through the wait thread
        assert_eq!(signal_messages(&bus), vec![libc::SIGINT]);
        dispatched(libc::SIGINT); // the loop ran intr_handler: watch OFF
        poll_late(); // #1 now: must be swallowed
        assert!(testing::intr_is_off());
        assert_eq!(signal_messages(&bus), Vec::<i32>::new());
        // SIGHUP, never armed, caught by the handler: swallowed too.
        LATE.store(2, Ordering::SeqCst);
        poll_late();
        assert_eq!(signal_messages(&bus), Vec::<i32>::new());
        set_disposition(libc::SIGINT, libc::SIG_DFL);
        testing::reset();
    }

    /// WP1 residual (a), race path: when `default_action` survives because
    /// the handler ran (R-WP0-6), it wakes the bus loop at once with a
    /// `signo: 0` message instead of leaving the signal for the next tick.
    #[test]
    fn the_race_path_wakes_the_bus_loop_at_once() {
        let _g = testing::lock();
        testing::reset();
        let bus = test_bus();
        *POST.lock().unwrap() = Some(bus.clone());
        fn arm_now() {
            set_disposition(
                libc::SIGHUP,
                watched_handler as *const () as libc::sighandler_t,
            );
        }
        let set = sigset(&[Signal::SIGHUP]);
        set.thread_block().unwrap();
        *BEFORE_RAISE.lock().unwrap() = Some(arm_now);
        default_action(Signal::SIGHUP);
        *BEFORE_RAISE.lock().unwrap() = None;
        assert_eq!(signal_messages(&bus), vec![0], "no wake posted");
        assert_eq!(take_late(), vec![libc::SIGHUP]);
        set_disposition(libc::SIGHUP, libc::SIG_DFL);
        set.thread_unblock().unwrap();
        testing::reset();
    }

    /// WP1 residual (a), library thread: a watched signal caught on a thread
    /// that is NOT the wait thread is forwarded to the wait thread (pending
    /// there, so its sigwait returns at once) instead of waiting for the
    /// next loop tick in LATE.
    #[test]
    fn a_signal_caught_off_the_wait_thread_is_forwarded_to_it() {
        let _g = testing::lock();
        testing::reset();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (got_tx, got_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let set = sigset(&[Signal::SIGHUP]);
            set.thread_block().unwrap();
            WAIT_THREAD.store(nix::unistd::gettid().as_raw(), Ordering::SeqCst);
            ready_tx.send(()).unwrap();
            if let Ok(s) = set.wait() {
                let _ = got_tx.send(s as i32);
            }
        });
        ready_rx.recv().unwrap();
        set_disposition(
            libc::SIGHUP,
            watched_handler as *const () as libc::sighandler_t,
        );
        // This thread has SIGHUP unblocked: raise() runs the handler HERE.
        nix::sys::signal::raise(Signal::SIGHUP).unwrap();
        let got = got_rx.recv_timeout(std::time::Duration::from_secs(2));
        set_disposition(libc::SIGHUP, libc::SIG_DFL);
        assert_eq!(got, Ok(libc::SIGHUP), "the signal was not forwarded");
        assert_eq!(FWD.swap(0, Ordering::SeqCst), 2);
        assert_eq!(take_late(), Vec::<i32>::new());
        testing::reset();
    }

    /// Runs `name` (this test binary's own test) in a child process with
    /// `XG_SYS_CHILD=1` and returns its exit status: a test whose failure
    /// mode is "the process dies by a signal" then FAILS BY ASSERTION here.
    #[allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: test harness spawns, not the agent
    fn in_child(name: &str) -> (std::process::ExitStatus, String) {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--test-threads=1", "--nocapture"])
            .env("XG_SYS_CHILD", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        (out.status, text)
    }

    /// R-WP1-3: the wait thread's FORWARDED branch. A SIGINT that
    /// `watched_handler` caught on another thread and forwarded (FWD set)
    /// after the watch is gone (OFF) is SWALLOWED — never the default action
    /// — and one forwarded while ARMED is dispatched. Run in a child so a
    /// regression (treating it as genuine → death by SIGINT) fails by
    /// assertion.
    #[test]
    fn a_forwarded_signal_is_never_fatal() {
        const NAME: &str = "capture_host::sys::tests::a_forwarded_signal_is_never_fatal";
        if std::env::var_os("XG_SYS_CHILD").is_none() {
            let (st, out) = in_child(NAME);
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(
                st.signal(),
                None,
                "the child died by a signal: {st:?}\n{out}"
            );
            assert!(st.success(), "{st:?}\n{out}");
            assert!(out.contains("1 passed"), "{out}");
            return;
        }
        let _g = testing::lock();
        testing::reset();
        let bus = test_bus();
        *POST.lock().unwrap() = Some(bus.clone());
        // Watch gone + forwarded: swallowed.
        FWD.store(1, Ordering::SeqCst);
        on_waited(libc::SIGINT);
        assert_eq!(signal_messages(&bus), Vec::<i32>::new());
        assert_eq!(
            FWD.load(Ordering::SeqCst),
            0,
            "the FWD bit was not consumed"
        );
        // Watch armed + forwarded: dispatched once.
        INTR.store(ARMED, Ordering::SeqCst);
        FWD.store(1, Ordering::SeqCst);
        on_waited(libc::SIGINT);
        assert_eq!(signal_messages(&bus), vec![libc::SIGINT]);
        testing::reset();
    }

    /// A SIGHUP that arrives while its dispatch is pending (posted, and the
    /// loop has not started `hup_handler` yet: GLib's `pending` flag is still
    /// set) COALESCES (no second post); after `dispatched` (the START of
    /// `hup_handler`, as GLib clears `pending` before the callback) the watch
    /// is armed again and the next one posts. (Was D47's pin, when the host
    /// called `dispatched` at the END of the handler; ci-sigint-race.)
    #[test]
    fn d47_a_sighup_during_its_dispatch_coalesces() {
        let _g = testing::lock();
        testing::reset();
        let bus = test_bus();
        *POST.lock().unwrap() = Some(bus.clone());
        HUP.store(ARMED, Ordering::SeqCst);
        deliver(libc::SIGHUP);
        deliver(libc::SIGHUP); // "during hup_handler"
        assert_eq!(signal_messages(&bus), vec![libc::SIGHUP]);
        dispatched(libc::SIGHUP);
        deliver(libc::SIGHUP);
        assert_eq!(signal_messages(&bus), vec![libc::SIGHUP]);
        testing::reset();
    }

    #[test]
    fn itoa_formats_without_allocating() {
        let mut b = [0u8; 12];
        assert_eq!(itoa(0, &mut b), b"0");
        assert_eq!(itoa(4242, &mut b), b"4242");
        assert_eq!(itoa(-17, &mut b), b"-17");
        assert_eq!(itoa(i64::from(i32::MAX), &mut b), b"2147483647");
    }
}
