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

use std::io;

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

impl CaptureBackend for GstLaunchBackend {
    fn program(&self) -> &str {
        &self.program
    }

    fn spawn(&self, args: &[String]) -> (Box<dyn CaptureChild>, UnboundedReceiver<ChildEvent>) {
        let (handle, rx) = child::spawn(&ChildSpec::new(self.program.clone(), args.to_vec()));
        (Box::new(GstChild(handle)), rx)
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
