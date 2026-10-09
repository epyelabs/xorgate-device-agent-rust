//! The IMU collector — `src/agent/collectors/imu.ts:18-133`
//! (`createImuCollector`) over `src/hardware/imu.ts:123-177` (`detectImu`,
//! `streamImuData`).
//!
//! Lifecycle (verbatim log lines in [`xorgate_core::imu_group`]):
//! 1. the first `read()`/`sample()` kicks off detection WITHOUT blocking the
//!    tick (`{}` this tick);
//! 2. i2cdetect missing/failed or no BNO085 on bus 1 → ONE warning, inert;
//! 3. found → `[imu] BNO085 detected at 0x4a — streaming` and the Python
//!    helper `python3 <XORGATE_PYTHON_DIR>/bno085_read.py --address 0x4a` is
//!    spawned (its own process group, see `xorgate_hal::child`);
//! 4. each `sample` line replaces `latest`; an unparsed line changes NOTHING
//!    (neither `latest` nor `updatedMono` — CF18); helper `error` messages and
//!    non-empty stderr lines are logged as `[imu] …` errors;
//! 5. when the helper exits: had samples → `[imu] reader stream ended; will
//!    re-detect` and a full reset (next tick re-detects); never sampled →
//!    `[imu] reader exited before any sample (exit N); IMU disabled. …` and
//!    the collector stays parked until the agent restarts.
//!
//! The euler mount correction re-reads `settings.json` `imuMount` per mapped
//! tick (`imu.ts:90`), so a cloud config change applies live.
//!
//! TS quirk reproduced: execa 9 (`reject:false`) resolves a spawn failure
//! with `notFound:false`, so `streamImuData`'s "python3 not found …" line
//! (`hardware/imu.ts:170-174`) NEVER prints; a missing interpreter reads as
//! `reader exited before any sample (exit ?)` (probed with tsx on the 89b5c64
//! scratch copy; WP2 evidence, ambiguity A3).

use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{error, info, warn};
use xorgate_core::imu::{parse_imu_line, ImuDetect, ImuMessage, ImuSample};
use xorgate_core::imu_group::{
    detected_line, exited_before_sample_line, map_latest, not_detected_line, on_reader_exit,
    stderr_error, to_hex_addr, ReaderExit, READER_ENDED,
};
use xorgate_core::telemetry::MetricGroup;
use xorgate_hal::child::{self, ChildEvent, ChildHandle, ChildSpec};
use xorgate_hal::imu_detect::ImuDetector;
use xorgate_hal::io::TapeSink;
use xorgate_hal::settings_io::load_settings;

use super::{to_group, Collector};
use crate::ctx::{AgentCtx, BoxFut, GroupSample};

/// The helper's file name (`pythonScript("bno085_read.py")`).
pub const IMU_HELPER: &str = "bno085_read.py";

/// The IMU's edges: detection and the helper spawn (injectable for tests).
pub trait ImuIo: Send + Sync + 'static {
    /// `detectImu()`.
    fn detect<'a>(&'a self, tape: &'a dyn TapeSink) -> BoxFut<'a, ImuDetect>;
    /// `runStream(spec…)`.
    fn spawn(&self, spec: &ChildSpec) -> (ChildHandle, mpsc::UnboundedReceiver<ChildEvent>);
}

/// Production: `i2cdetect -y 1` + a real child process.
#[derive(Debug, Default)]
pub struct RealImuIo {
    detector: ImuDetector,
}

impl ImuIo for RealImuIo {
    fn detect<'a>(&'a self, tape: &'a dyn TapeSink) -> BoxFut<'a, ImuDetect> {
        Box::pin(self.detector.detect(tape))
    }

    fn spawn(&self, spec: &ChildSpec) -> (ChildHandle, mpsc::UnboundedReceiver<ChildEvent>) {
        child::spawn(spec)
    }
}

/// `createImuCollector()`.
pub struct ImuCollector {
    shared: Arc<Shared>,
}

struct Shared {
    ctx: AgentCtx,
    io: Arc<dyn ImuIo>,
    st: Mutex<State>,
}

/// The TS closure's `let`s (`imu.ts:19-24`).
#[derive(Default)]
struct State {
    started: bool,
    stream: Option<ChildHandle>,
    latest: Option<ImuSample>,
    latest_mono: Option<Instant>,
    ever_sampled: bool,
    warned: bool,
}

impl State {
    /// `reset()` (`imu.ts:26-31`) — `warned` survives.
    fn reset(&mut self) {
        self.started = false;
        self.stream = None;
        self.latest = None;
        self.ever_sampled = false;
    }
}

impl ImuCollector {
    pub fn new(ctx: &AgentCtx, io: Arc<dyn ImuIo>) -> Self {
        Self {
            shared: Arc::new(Shared {
                ctx: ctx.clone(),
                io,
                st: Mutex::new(State::default()),
            }),
        }
    }

    /// SIGTERM the helper's process group (execa's `cleanup` at process exit
    /// in the TS). A no-op once the helper has exited: the handle is dropped
    /// on that path, and `ChildHandle` never signals a reaped pid anyway.
    pub fn stop(&self) {
        let st = self.shared.lock();
        if let Some(h) = &st.stream {
            h.signal_group(nix::libc::SIGTERM);
        }
    }

    /// The helper's pid while one runs (tests / diagnostics; never `kill` it).
    pub fn helper_pid(&self) -> Option<u32> {
        self.shared
            .lock()
            .stream
            .as_ref()
            .and_then(ChildHandle::pid)
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `if (!started) { void start(); … }` — returns true when this call kicked it off.
    fn kick(self: &Arc<Self>, st: &mut State) -> bool {
        if st.started {
            return false;
        }
        st.started = true; // set before the first await so reads don't double-start
        let me = self.clone();
        self.ctx.spawn("imu-start", async move { me.start().await });
        true
    }

    /// `start()` (`imu.ts:37-79`).
    async fn start(self: Arc<Self>) {
        let detect = self.io.detect(self.ctx.tape.as_ref()).await;
        if !detect.available {
            let mut st = self.lock();
            if !st.warned {
                warn!("[imu] {}", detect.reason.as_deref().unwrap_or(""));
                st.warned = true;
            }
            return;
        }
        let addr = match detect.imu_address {
            Some(a) if detect.imu_present => a,
            _ => {
                let mut st = self.lock();
                if !st.warned {
                    warn!("{}", not_detected_line());
                    st.warned = true;
                }
                return;
            }
        };
        info!("{}", detected_line(addr));
        let script = self.ctx.paths.python_dir.join(super::imu::IMU_HELPER);
        let spec = ChildSpec::new(
            "python3",
            vec![
                script.display().to_string(),
                "--address".to_string(),
                to_hex_addr(addr),
            ],
        );
        self.ctx.tape.imu_spawn(&spec.argv());
        let (handle, rx) = self.io.spawn(&spec);
        self.lock().stream = Some(handle);
        self.run(rx).await;
    }

    /// The helper's stdout/stderr handlers and its `done` (`hardware/imu.ts:153-175`,
    /// `collectors/imu.ts:55-78`).
    async fn run(&self, mut rx: mpsc::UnboundedReceiver<ChildEvent>) {
        while let Some(ev) = rx.recv().await {
            match ev {
                ChildEvent::Stdout(line) => {
                    self.ctx.tape.imu(&line);
                    match parse_imu_line(&line) {
                        Some(ImuMessage::Sample { quat, linaccel }) => {
                            let mut st = self.lock();
                            st.latest = Some(ImuSample { quat, linaccel });
                            st.latest_mono = Some(Instant::now());
                            st.ever_sampled = true;
                        }
                        Some(ImuMessage::Error { error: msg }) => error!("[imu] {msg}"),
                        // `ready` has no handler in the collector; an unparsed
                        // line is ignored (CF18: `latest` and its stamp unchanged).
                        Some(ImuMessage::Ready { .. }) | None => {}
                    }
                }
                ChildEvent::Stderr(line) => {
                    if let Some(t) = stderr_error(&line) {
                        error!("[imu] {t}");
                    }
                }
                ChildEvent::Done(done) => {
                    self.ctx.tape.imu_exit(done.exit_code);
                    let mut st = self.lock();
                    match on_reader_exit(st.ever_sampled) {
                        ReaderExit::Redetect => {
                            warn!("{READER_ENDED}");
                            st.reset();
                        }
                        ReaderExit::Park => {
                            error!("{}", exited_before_sample_line(done.exit_code));
                            // Parked (`started` stays true): drop the dead
                            // handle so `stop()` has nothing to signal.
                            st.stream = None;
                        }
                    }
                    break;
                }
            }
        }
    }

    /// `mapLatest(sample)` with this tick's `imuMount`.
    fn map(&self, sample: &ImuSample) -> MetricGroup {
        let mount = load_settings(&self.ctx.paths.settings).imu_mount;
        to_group(&map_latest(sample, mount.as_ref()))
    }
}

impl Collector for ImuCollector {
    fn group(&self) -> &'static str {
        "imu"
    }

    /// `read()` (`imu.ts:110-119`).
    fn read(&self) -> BoxFut<'_, MetricGroup> {
        Box::pin(async move {
            let sh = &self.shared;
            let latest = {
                let mut st = sh.lock();
                if sh.kick(&mut st) {
                    return MetricGroup::new();
                }
                st.latest
            };
            match latest {
                Some(s) => sh.map(&s),
                None => MetricGroup::new(),
            }
        })
    }

    /// `sample()` (`imu.ts:124-131`).
    fn sample(&self) -> BoxFut<'_, Option<GroupSample>> {
        Box::pin(async move {
            let sh = &self.shared;
            let (latest, mono) = {
                let mut st = sh.lock();
                if sh.kick(&mut st) {
                    return None;
                }
                (st.latest?, st.latest_mono?)
            };
            Some(GroupSample {
                fields: sh.map(&latest),
                updated_mono: mono,
            })
        })
    }
}
