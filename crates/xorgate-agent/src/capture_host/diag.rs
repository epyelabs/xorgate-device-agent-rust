//! Optional buffer-flow diagnostics (phase-7a §Design 6), OFF unless
//! `XORGATE_CAPTURE_HOST_DIAG=1`. When off, nothing is installed and nothing
//! is printed. When on, one BUFFER/BUFFER_LIST pad probe sits on each sink
//! pad of each top-level sink element (`record` = splitmuxsink, `bridge` =
//! shmsink, otherwise the element name), and the bus loop prints one stderr
//! line when a branch has had no buffer for the stall period (and one when
//! buffers flow again). It exists for a contained 1080p30 or bridge-only
//! investigation; the agent's 45 s file-growth watchdog stays authoritative.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

/// The switch: exactly `1` turns the probes on.
pub const DIAG_ENV: &str = "XORGATE_CAPTURE_HOST_DIAG";
/// The stall period in ms (default [`DEFAULT_STALL`]); tests shorten it.
pub const DIAG_STALL_MS_ENV: &str = "XORGATE_CAPTURE_HOST_DIAG_STALL_MS";
/// "no buffers for N s" after 10 s without one (well inside the agent's 45 s).
pub const DEFAULT_STALL: Duration = Duration::from_secs(10);

/// Whether the environment turns the probes on (read once at host start).
pub fn enabled_from_env() -> bool {
    std::env::var(DIAG_ENV).is_ok_and(|v| v == "1")
}

fn stall_from_env() -> Duration {
    std::env::var(DIAG_STALL_MS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_STALL, Duration::from_millis)
}

/// One probed branch.
struct Branch {
    label: String,
    /// ns since [`Diag::epoch`] of the last buffer (0 = none yet).
    last_ns: AtomicU64,
    stalled: AtomicBool,
}

/// The probes and their per-branch flow state.
pub struct Diag {
    epoch: Instant,
    stall: Duration,
    branches: Vec<Arc<Branch>>,
    /// Flow is measured from PLAYING (set by the bus loop).
    since: Option<Instant>,
    next_check: Instant,
}

fn label_of(el: &gst::Element) -> String {
    match el.factory().map(|f| f.name().to_string()).as_deref() {
        Some("splitmuxsink") => "record".to_string(),
        Some("shmsink") => "bridge".to_string(),
        _ => el.name().to_string(),
    }
}

impl Diag {
    /// Installs the probes (call only when [`enabled_from_env`]).
    pub fn install(pipeline: &gst::Element) -> Self {
        let epoch = Instant::now();
        let mut branches = Vec::new();
        if let Some(bin) = pipeline.downcast_ref::<gst::Bin>() {
            for el in bin.iterate_sinks().into_iter().flatten() {
                let b = Arc::new(Branch {
                    label: format!("{} ({})", label_of(&el), el.name()),
                    last_ns: AtomicU64::new(0),
                    stalled: AtomicBool::new(false),
                });
                for pad in el.sink_pads() {
                    let bb = b.clone();
                    pad.add_probe(
                        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                        move |_, _| {
                            super::contain((), || {
                                let ns = u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(1);
                                bb.last_ns.store(ns.max(1), Ordering::Relaxed);
                            });
                            gst::PadProbeReturn::Ok
                        },
                    );
                }
                branches.push(b);
            }
        }
        Self {
            epoch,
            stall: stall_from_env(),
            branches,
            since: None,
            next_check: epoch,
        }
    }

    /// The pipeline reached PLAYING: start measuring.
    pub fn playing(&mut self) {
        if self.since.is_none() {
            self.since = Some(Instant::now());
        }
    }

    /// The bus loop's periodic check: the lines to print on stderr.
    pub fn check(&mut self) -> Vec<String> {
        let now = Instant::now();
        let Some(since) = self.since else {
            return Vec::new();
        };
        if now < self.next_check {
            return Vec::new();
        }
        self.next_check = now + (self.stall / 4).max(Duration::from_millis(50));
        let mut out = Vec::new();
        for b in &self.branches {
            let last = b.last_ns.load(Ordering::Relaxed);
            let last_at = if last == 0 {
                since
            } else {
                (self.epoch + Duration::from_nanos(last)).max(since)
            };
            let quiet = now.saturating_duration_since(last_at);
            let was = b.stalled.load(Ordering::Relaxed);
            if quiet >= self.stall && !was {
                b.stalled.store(true, Ordering::Relaxed);
                out.push(format!(
                    "xorgate-capture diag: {} no buffers for {} s\n",
                    b.label,
                    quiet.as_secs()
                ));
            } else if quiet < self.stall && was {
                b.stalled.store(false, Ordering::Relaxed);
                out.push(format!(
                    "xorgate-capture diag: {} buffers flowing again\n",
                    b.label
                ));
            }
        }
        out
    }
}
