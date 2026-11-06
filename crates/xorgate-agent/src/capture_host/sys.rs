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
use std::sync::atomic::{AtomicU8, Ordering};
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
/// every thread, so this only runs if some library thread unblocked it; it
/// then just records the signal for the bus loop's poll (async-signal-safe).
extern "C" fn watched_handler(sig: libc::c_int) {
    LATE.fetch_or(if sig == libc::SIGINT { 1 } else { 2 }, Ordering::SeqCst);
}

/// Signals that reached [`watched_handler`] (bit 1 = SIGINT, 2 = SIGHUP).
static LATE: AtomicU8 = AtomicU8::new(0);

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

/// The default action of `sig`, now, from the calling thread (the process
/// terminates: SIGINT/SIGHUP default to Term).
fn die_by(sig: Signal) -> ! {
    set_disposition(sig as i32, libc::SIG_DFL);
    // Unblock `sig` for this thread only, then raise it at this thread: the
    // default action terminates the whole process.
    let _ = sigset(&[sig]).thread_unblock();
    let _ = nix::sys::signal::raise(sig);
    // Unreachable for Term signals; keep the type honest.
    std::process::exit(128 + sig as i32);
}

fn wait_loop() {
    let set = sigset(&[Signal::SIGINT, Signal::SIGHUP]);
    loop {
        // sigwait on signals this thread (and every other) blocks.
        if let Ok(sig) = set.wait() {
            deliver(sig as i32);
        }
    }
}

/// One received SIGINT/SIGHUP: dispatch it if watched, coalesce it if a
/// dispatch is pending (GLib's handler only sets a flag), else die by it.
pub fn deliver(sig: i32) {
    let st = state(sig);
    match st.compare_exchange(ARMED, PENDING, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => post_signal(sig),
        Err(PENDING) => {}
        Err(_) => die_by(if sig == libc::SIGINT {
            Signal::SIGINT
        } else {
            Signal::SIGHUP
        }),
    }
}

fn post_signal(sig: i32) {
    let bus = POST.lock().ok().and_then(|g| g.clone());
    if let Some(bus) = bus {
        let s = gstreamer::Structure::builder(SIGNAL_MESSAGE)
            .field("signo", sig)
            .build();
        let _ = bus.post(gstreamer::message::Application::new(s));
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
    for sig in [libc::SIGINT, libc::SIGHUP] {
        set_disposition(sig, watched_handler as *const () as libc::sighandler_t);
        state(sig).store(ARMED, Ordering::SeqCst);
    }
}

/// The bus loop dispatched a received signal. SIGINT's watch is one-shot
/// (`intr_handler` returns G_SOURCE_REMOVE → GLib restores SIG_DFL); SIGHUP's
/// stays (G_SOURCE_CONTINUE).
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
    // SAFETY: called on the main thread before any other thread but the
    // signal-wait thread (which never reads locale data) exists.
    unsafe {
        libc::setlocale(libc::LC_ALL, c"".as_ptr());
    }
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
    // its bus and every message; the wait thread posts only through `POST`,
    // which `disarm()` emptied; nothing touches GStreamer afterwards.
    unsafe { gstreamer::deinit() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn itoa_formats_without_allocating() {
        let mut b = [0u8; 12];
        assert_eq!(itoa(0, &mut b), b"0");
        assert_eq!(itoa(4242, &mut b), b"4242");
        assert_eq!(itoa(-17, &mut b), b"-17");
        assert_eq!(itoa(i64::from(i32::MAX), &mut b), b"2147483647");
    }
}
