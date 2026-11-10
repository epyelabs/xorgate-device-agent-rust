//! The capture child seam (phase-3 P2 / Q4): the supervisor spawns its
//! per-camera pipeline through [`CaptureBackend`], so the Phase 7 in-process
//! GStreamer backend is a contained swap. Phase 3 has exactly one
//! implementation, [`GstLaunchBackend`]: `gst-launch-1.0 <argv>` resolved
//! through `PATH` (the rig's shim and the bench's shared-profile shim depend
//! on that), run by `xorgate-hal::child` — own process group, `kill_on_drop`,
//! both pipes drained line by line, `Done` last, signals only to an unreaped
//! child (no bare-pid signalling).
//!
//! The TS seam is `CaptureSpawn = typeof runStream` (`video-capture.ts:264`);
//! `run.handle.stop(sig)` is execa's `child.kill(sig)`: the child pid only,
//! not its group (P11).
//!
//! Phase 7a WP0 adds [`HostBackend`]: the agent binary re-executed as
//! `xorgate-capture __capture-host <the same tokens>` (`crate::capture_host`,
//! gstreamer-rs in a supervised child, Q-A3/R1), spawned through the same
//! `xorgate-hal::child` path so process group, `kill_on_drop`, pipes,
//! signals and renice are unchanged. [`backend_from_env`] picks it only when
//! `XORGATE_CAPTURE_BACKEND=host`; unset (the default) is today's
//! [`GstLaunchBackend`], byte for byte.

use std::io;
use std::sync::{Arc, Once};

use tokio::sync::mpsc::UnboundedReceiver;
use xorgate_hal::child::{self, ChildEvent, ChildHandle, ChildSpec};

/// The program the TS spawns (`video-capture.ts:671`).
pub const GST_LAUNCH: &str = "gst-launch-1.0";

/// One running capture child (the TS `StreamHandle`).
pub trait CaptureChild: Send + Sync {
    /// The OS pid (absent when the spawn failed, or in test fakes).
    fn pid(&self) -> Option<u32>;
    /// `handle.stop(sig)`: signal the child itself, a no-op once it is reaped.
    fn signal(&self, sig: i32);
    /// [`Self::signal`], calling `report(errno)` once the `kill(2)` returned
    /// (0 = sent; not called when nothing was sent). The default (test
    /// fakes) signals and reports 0.
    fn signal_reported(&self, sig: i32, report: child::SignalReport) {
        self.signal(sig);
        report(0);
    }
    /// `setPriority(pid, nice)` (`video-capture.ts:690-696`).
    fn renice(&self, nice: i32) -> io::Result<()>;
}

/// Spawns one capture child for a gst-launch argv (the tokens AFTER the
/// program name, exactly as the core builders return them).
pub trait CaptureBackend: Send + Sync {
    /// The program name the argv is handed to (tape `child_spawn.argv[0]`).
    fn program(&self) -> &str;
    /// Spawn; never fails synchronously — a spawn failure is a lone `Done`.
    fn spawn(&self, args: &[String]) -> (Box<dyn CaptureChild>, UnboundedReceiver<ChildEvent>);
    /// The child prints the phase-7a `XG-FRAGMENT*` timing lines (only the
    /// capture host does), so `XORGATE_SIDECAR_TIMING=exact` can take effect.
    fn emits_timing(&self) -> bool {
        false
    }
}

/// The backend switch (phase-7a §Design 9). Unset/empty = `gst-launch`.
pub const CAPTURE_BACKEND_ENV: &str = "XORGATE_CAPTURE_BACKEND";
/// Overrides the program the host backend executes (the rig shim, the
/// shared profile's blocking shim). Unset = the running agent binary.
pub const CAPTURE_HOST_BIN_ENV: &str = "XORGATE_CAPTURE_HOST_BIN";

/// Which capture backend a `XORGATE_CAPTURE_BACKEND` value selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// `gst-launch-1.0` (the default; byte parity with JS 09da38d).
    GstLaunch,
    /// The gstreamer-rs capture host.
    Host,
}

/// `XORGATE_CAPTURE_BACKEND` → backend: unset or empty → `gst-launch`;
/// `gst-launch` / `host` (trimmed, case-sensitive); anything else →
/// `gst-launch` and `Err(value)` so the caller warns once.
pub fn select_backend(value: Option<&str>) -> Result<BackendKind, String> {
    match value.map(str::trim) {
        None | Some("") | Some("gst-launch") => Ok(BackendKind::GstLaunch),
        Some("host") => Ok(BackendKind::Host),
        Some(other) => Err(other.to_string()),
    }
}

/// The backend the process environment selects (read per supervisor build).
pub fn backend_from_env() -> Arc<dyn CaptureBackend> {
    backend_for(std::env::var(CAPTURE_BACKEND_ENV).ok().as_deref())
}

/// The backend a `XORGATE_CAPTURE_BACKEND` value selects (`None` = unset).
/// An unknown value warns ONCE per process and falls back to `gst-launch`.
pub fn backend_for(value: Option<&str>) -> Arc<dyn CaptureBackend> {
    let kind = select_backend(value).unwrap_or_else(|bad| {
        static WARN: Once = Once::new();
        WARN.call_once(|| {
            tracing::warn!(
                "[video-capture] {CAPTURE_BACKEND_ENV}={bad:?} is not gst-launch|host; using gst-launch"
            )
        });
        BackendKind::GstLaunch
    });
    match kind {
        BackendKind::GstLaunch => Arc::new(GstLaunchBackend::default()),
        BackendKind::Host => Arc::new(HostBackend::from_env()),
    }
}

/// `gst-launch-1.0` through `PATH` (P2).
#[derive(Debug, Clone)]
pub struct GstLaunchBackend {
    program: String,
}

impl Default for GstLaunchBackend {
    fn default() -> Self {
        Self {
            program: GST_LAUNCH.to_string(),
        }
    }
}

impl GstLaunchBackend {
    /// Another program in place of `gst-launch-1.0` (tests: a real child with
    /// chosen signal behaviour; the argv is unchanged).
    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl CaptureBackend for GstLaunchBackend {
    fn program(&self) -> &str {
        &self.program
    }

    fn spawn(&self, args: &[String]) -> (Box<dyn CaptureChild>, UnboundedReceiver<ChildEvent>) {
        let (handle, rx) = child::spawn(&ChildSpec::new(self.program.clone(), args.to_vec()));
        (Box::new(GstChild(handle)), rx)
    }
}

/// The capture host: `<exe> __capture-host <tokens…>` with `argv[0] =
/// xorgate-capture`. `program()` is `xorgate-capture` (tape
/// `child_spawn.argv[0]`); the tokens are exactly the gst-launch tokens.
#[derive(Debug, Clone)]
pub struct HostBackend {
    exe: String,
}

/// What the host backend executes when `XORGATE_CAPTURE_HOST_BIN` is unset:
/// the running binary's own inode, so a re-exec still works after
/// `xorgate-rs update` replaced the file on disk.
pub const SELF_EXE: &str = "/proc/self/exe";

impl HostBackend {
    /// [`SELF_EXE`], unless `XORGATE_CAPTURE_HOST_BIN` names another program.
    pub fn from_env() -> Self {
        Self::from_host_bin(std::env::var(CAPTURE_HOST_BIN_ENV).ok().as_deref())
    }

    /// [`Self::from_env`] for a given `XORGATE_CAPTURE_HOST_BIN` value
    /// (`None` = unset; empty/blank = unset).
    pub fn from_host_bin(value: Option<&str>) -> Self {
        let exe = value
            .filter(|v| !v.trim().is_empty())
            .map_or_else(|| SELF_EXE.to_string(), str::to_string);
        Self { exe }
    }

    /// A host backend executing `exe` (tests, rig).
    pub fn with_exe(exe: impl Into<String>) -> Self {
        Self { exe: exe.into() }
    }

    /// The program executed (not the tape name; see [`CaptureBackend::program`]).
    pub fn exe(&self) -> &str {
        &self.exe
    }

    /// The spawn spec for one capture argv.
    pub fn spec(&self, args: &[String]) -> ChildSpec {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(crate::capture_host::SUBCOMMAND.to_string());
        argv.extend_from_slice(args);
        let mut spec = ChildSpec::new(self.exe.clone(), argv);
        spec.arg0 = Some(crate::capture_host::PROCESS_NAME.to_string());
        spec
    }
}

impl CaptureBackend for HostBackend {
    fn program(&self) -> &str {
        crate::capture_host::PROCESS_NAME
    }

    fn spawn(&self, args: &[String]) -> (Box<dyn CaptureChild>, UnboundedReceiver<ChildEvent>) {
        let (handle, rx) = child::spawn(&self.spec(args));
        (Box::new(GstChild(handle)), rx)
    }

    fn emits_timing(&self) -> bool {
        true
    }
}

/// A [`ChildHandle`] as a [`CaptureChild`].
#[derive(Debug)]
struct GstChild(ChildHandle);

impl CaptureChild for GstChild {
    fn pid(&self) -> Option<u32> {
        self.0.pid()
    }

    fn signal(&self, sig: i32) {
        self.0.signal(sig);
    }

    fn signal_reported(&self, sig: i32, report: child::SignalReport) {
        self.0.signal_reported(sig, report);
    }

    #[allow(unsafe_code)]
    fn renice(&self, nice: i32) -> io::Result<()> {
        let Some(pid) = self.0.pid() else {
            return Ok(());
        };
        // SAFETY: setpriority(2) on the pid we just spawned (the TS's
        // `os.setPriority(pid, 10)` right after spawn; same race window).
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, nice) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_switch_defaults_to_gst_launch() {
        assert_eq!(select_backend(None), Ok(BackendKind::GstLaunch));
        assert_eq!(select_backend(Some("")), Ok(BackendKind::GstLaunch));
        assert_eq!(
            select_backend(Some("gst-launch")),
            Ok(BackendKind::GstLaunch)
        );
        assert_eq!(select_backend(Some(" host ")), Ok(BackendKind::Host));
        assert_eq!(select_backend(Some("Host")), Err("Host".to_string()));
        assert_eq!(
            select_backend(Some("gstreamer")),
            Err("gstreamer".to_string())
        );
    }

    /// R-WP0-5: the switch's default is today's object, not the host.
    #[test]
    fn backend_for_maps_unset_empty_and_gst_launch_to_gst_launch() {
        for v in [
            None,
            Some(""),
            Some("  "),
            Some("gst-launch"),
            Some("bogus"),
        ] {
            assert_eq!(backend_for(v).program(), GST_LAUNCH, "{v:?}");
        }
        assert_eq!(backend_for(Some("host")).program(), "xorgate-capture");
        assert_eq!(backend_for(Some(" host\n")).program(), "xorgate-capture");
    }

    /// R-WP0R-3: the env read itself. With `XORGATE_CAPTURE_BACKEND` unset
    /// (the default on every device), the supervisor's backend is
    /// gst-launch, i.e. `recording/mod.rs:150` installs today's object.
    #[test]
    fn backend_from_env_is_gst_launch_when_the_switch_is_unset() {
        if std::env::var_os(CAPTURE_BACKEND_ENV).is_some() {
            eprintln!("skip: {CAPTURE_BACKEND_ENV} is set in this test process");
            return;
        }
        assert_eq!(backend_from_env().program(), GST_LAUNCH);
    }

    #[test]
    fn host_backend_reexecs_proc_self_exe_unless_overridden() {
        assert_eq!(HostBackend::from_host_bin(None).exe(), "/proc/self/exe");
        assert_eq!(HostBackend::from_host_bin(Some("")).exe(), "/proc/self/exe");
        assert_eq!(
            HostBackend::from_host_bin(Some(" ")).exe(),
            "/proc/self/exe"
        );
        assert_eq!(
            HostBackend::from_host_bin(Some("/var/lib/xorgate-rs/shared-shims/xorgate-capture"))
                .exe(),
            "/var/lib/xorgate-rs/shared-shims/xorgate-capture"
        );
        let spec = HostBackend::from_host_bin(None).spec(&["videotestsrc".to_string()]);
        assert_eq!(spec.program, "/proc/self/exe");
        assert_eq!(spec.arg0.as_deref(), Some("xorgate-capture"));
    }

    #[test]
    fn host_spec_reexecs_with_the_unchanged_tokens() {
        let b = HostBackend::with_exe("/opt/xorgate-rs/bin/xorgate-rs");
        let toks: Vec<String> = [
            "-e",
            "videotestsrc",
            "!",
            "muxer=mp4mux fragment-duration=1000",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let spec = b.spec(&toks);
        assert_eq!(spec.program, "/opt/xorgate-rs/bin/xorgate-rs");
        assert_eq!(spec.arg0.as_deref(), Some("xorgate-capture"));
        assert_eq!(spec.args[0], "__capture-host");
        assert_eq!(&spec.args[1..], &toks[..]);
        assert!(spec.env.is_empty() && spec.cwd.is_none());
        assert_eq!(b.program(), "xorgate-capture");
        assert_eq!(GstLaunchBackend::default().program(), "gst-launch-1.0");
    }

    /// Phase 7a WP2: only the host prints timing lines, so only it lets
    /// `XORGATE_SIDECAR_TIMING=exact` take effect.
    #[test]
    fn only_the_host_emits_timing() {
        assert!(HostBackend::with_exe("/x").emits_timing());
        assert!(!GstLaunchBackend::default().emits_timing());
        assert!(backend_for(Some("host")).emits_timing());
        assert!(!backend_for(None).emits_timing());
    }
}
