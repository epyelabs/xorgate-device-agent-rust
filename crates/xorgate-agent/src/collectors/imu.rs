//! The IMU collector — `src/agent/collectors/imu.ts:18-133`
//! (`createImuCollector`) over `src/hardware/imu.ts:123-177` (`detectImu`,
//! `streamImuData`).
//!
//! Lifecycle (verbatim log lines in [`xorgate_core::imu_group`]):
//! 1. the first `read()`/`sample()` kicks off detection WITHOUT blocking the
//!    tick (`{}` this tick);
//! 2. i2cdetect missing/failed or no BNO085 on bus 1 → ONE warning, inert;
//! 3. found → `[imu] <name> detected at 0x4a — starting the reader` (D129:
//!    the name is the hardware block's IMU kind upper-cased, else `BNO08x`;
//!    the TS says `BNO085 … — streaming`) and the Python
//!    helper `python3 <XORGATE_PYTHON_DIR>/bno085_read.py --address 0x4a` is
//!    spawned (its own process group, see `xorgate_hal::child`);
//! 4. each `sample` line replaces `latest`, and the FIRST of a reader run logs
//!    `[imu] <name> at 0x4a — streaming (firmware: part …, version …, build …)`
//!    with the identity of the reader's `ready` line (D129; no TS
//!    counterpart); an unparsed line changes NOTHING
//!    (neither `latest` nor `updatedMono` — CF18); helper `error` messages and
//!    non-empty stderr lines are logged as `[imu] …` errors;
//! 5. when the helper exits: had samples → `[imu] reader stream ended; will
//!    re-detect` and a full reset (next tick re-detects); never sampled →
//!    `[imu] reader exited before any sample (exit N); IMU disabled. …` and
//!    the collector stays parked until the agent restarts. Neither line when
//!    the exit follows `stop()` / the agent's cancellation: at shutdown the TS
//!    process just exits (execa's cleanup kills the helper) and logs nothing.
//!
//! The euler mount correction re-reads `settings.json` `imuMount` per mapped
//! tick (`imu.ts:90`), so a cloud config change applies live.
//!
//! Phase 7c (WP3, `phase-7c-rust-imu.md` §Design 1/3): `XORGATE_IMU_BACKEND`
//! picks the reader the collector spawns, and NOTHING else changes —
//! detection, the supervision ladder, the log lines (the park line keeps its
//! Python wording, R7) and the NDJSON consumer are the same code for both:
//! - unset / empty / `python` → `python3 <python_dir>/bno085_read.py
//!   --address 0x4a`, byte-identical to before (the default; C6);
//! - `rust` → the agent's own binary re-executed as the Rust reader:
//!   program `XORGATE_IMU_READER_BIN`, or [`SELF_EXE`] (`/proc/self/exe`)
//!   when that is unset or blank, `argv[0] = xorgate-imu`, args
//!   `__imu-reader --address 0x4a` (tape `imu_spawn` argv
//!   `["xorgate-imu", "__imu-reader", "--address", "0x4a"]`);
//! - anything else → `python` and ONE warn per process.
//!
//! Both are read when the collector starts a reader, so a re-detect re-reads
//! them. R9 / Q-C4 (CM5 only until a CM4 A/B row passes) is ENFORCED as an
//! allow-list: `rust` takes effect only when `/proc/device-tree/compatible`
//! names `brcm,bcm2712`; otherwise (another SoC, or the DT unreadable) one
//! warn `… falling back to python`. `XORGATE_IMU_SOC_CHECK=off` lifts it for
//! the rig and containers only (the bench guards refuse it on a device).
//!
//! TS quirk reproduced: execa 9 (`reject:false`) resolves a spawn failure
//! with `notFound:false`, so `streamImuData`'s "python3 not found …" line
//! (`hardware/imu.ts:170-174`) NEVER prints; a missing interpreter reads as
//! `reader exited before any sample (exit ?)` (probed with tsx on the 89b5c64
//! scratch copy; WP2 evidence, ambiguity A3).

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, Once};

use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{error, info, warn};
use xorgate_core::imu::{parse_imu_line, ImuDetect, ImuMessage, ImuSample};
use xorgate_core::imu_group::{
    detected_line, exited_before_sample_line, imu_name, map_latest, not_detected_line,
    on_reader_exit, stderr_error, streaming_line, to_hex_addr, ReaderExit, ReaderIdentity,
    READER_ENDED,
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

/// Phase 7c: the reader backend switch (§Design 3). Unset/empty = `python`.
pub const IMU_BACKEND_ENV: &str = "XORGATE_IMU_BACKEND";
/// Phase 7c: the program the `rust` backend executes (rig relay, the C10b
/// strace wrapper, the shared profile's blocking shim). Unset/blank = the
/// running agent binary ([`SELF_EXE`]).
pub const IMU_READER_BIN_ENV: &str = "XORGATE_IMU_READER_BIN";
/// What the `rust` backend executes by default: the running binary's own
/// inode, so a re-exec still works after `xorgate-rs update` replaced the
/// file on disk.
pub const SELF_EXE: &str = "/proc/self/exe";
/// Phase 7c R9 (Q-C4; review-wp3 F4): the `rust` backend is certified on
/// BCM2712 (CM5) only, so it takes effect only when the device tree's
/// `compatible` list names this SoC — an ALLOW-list: an unknown board or an
/// unreadable DT means python.
pub const CERTIFIED_SOC: &str = "brcm,bcm2712";
/// Where the SoC is read (NUL-separated `compatible` strings).
pub const DT_COMPATIBLE: &str = "/proc/device-tree/compatible";
/// The ONE override of the R9 allow-list, for the replay rig and containers
/// (no device tree there): `XORGATE_IMU_SOC_CHECK=off`. NOT for devices: the
/// bench profiles and env-file guards refuse it.
pub const IMU_SOC_CHECK_ENV: &str = "XORGATE_IMU_SOC_CHECK";

/// R9's verdict for the `rust` backend: `Ok` when `compatible` names
/// [`CERTIFIED_SOC`] as one of its NUL-separated entries (exactly), or when
/// `soc_check` is `off` (trimmed, case-sensitive); else `Err(reason)`. An
/// unreadable DT fails closed. Pure: the caller reads the file.
pub fn soc_allows_rust(
    compatible: std::io::Result<Vec<u8>>,
    soc_check: Option<&str>,
) -> Result<(), String> {
    if soc_check.map(str::trim) == Some("off") {
        return Ok(());
    }
    match compatible {
        Ok(bytes) => {
            if bytes
                .split(|b| *b == 0)
                .any(|entry| entry == CERTIFIED_SOC.as_bytes())
            {
                Ok(())
            } else {
                Err(format!("{DT_COMPATIBLE} does not name {CERTIFIED_SOC}"))
            }
        }
        Err(e) => Err(format!("{DT_COMPATIBLE} is unreadable ({e})")),
    }
}

/// Which reader a `XORGATE_IMU_BACKEND` value selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImuBackend {
    /// `python3 <python_dir>/bno085_read.py` (the default; byte parity).
    Python,
    /// `xorgate-rs __imu-reader` (phase 7c).
    Rust,
}

/// `XORGATE_IMU_BACKEND` → backend: unset or empty → `python`; `python` /
/// `rust` (trimmed, case-sensitive); anything else → `python` and
/// `Err(value)` so the caller warns once. Pure: never reads the env.
pub fn select_imu_backend(value: Option<&str>) -> Result<ImuBackend, String> {
    match value.map(str::trim) {
        None | Some("") | Some("python") => Ok(ImuBackend::Python),
        Some("rust") => Ok(ImuBackend::Rust),
        Some(other) => Err(other.to_string()),
    }
}

/// The python helper's spec: `python3 <python_dir>/bno085_read.py --address
/// 0x4a`, exactly main's (C6).
fn python_spec(python_dir: &Path, addr: u32) -> ChildSpec {
    let script = python_dir.join(IMU_HELPER);
    ChildSpec::new(
        "python3",
        vec![
            script.display().to_string(),
            "--address".to_string(),
            to_hex_addr(addr),
        ],
    )
}

/// The reader's [`ChildSpec`] for a detected address, given the two knob
/// values (`None` = unset) and the R9 SoC gate. An invalid backend warns ONCE
/// per process and falls back to `python`; so does `rust` when `soc_gate`
/// says no (R9). `soc_gate` is called ONLY for `rust`, so the default path
/// reads nothing. Pure apart from those warns.
pub fn reader_spec(
    backend: Option<&str>,
    reader_bin: Option<&str>,
    soc_gate: &dyn Fn() -> Result<(), String>,
    python_dir: &Path,
    addr: u32,
) -> ChildSpec {
    let kind = select_imu_backend(backend).unwrap_or_else(|bad| {
        static WARN: Once = Once::new();
        WARN.call_once(|| {
            warn!("[imu] {IMU_BACKEND_ENV}={bad:?} is not python|rust; using python")
        });
        ImuBackend::Python
    });
    match kind {
        ImuBackend::Python => python_spec(python_dir, addr),
        ImuBackend::Rust => {
            if let Err(why) = soc_gate() {
                static SOC_WARN: Once = Once::new();
                SOC_WARN.call_once(|| {
                    warn!(
                        "[imu] {IMU_BACKEND_ENV}=rust is certified on {CERTIFIED_SOC} (CM5) only: {why}; falling back to python"
                    )
                });
                return python_spec(python_dir, addr);
            }
            let exe = reader_bin
                .filter(|v| !v.trim().is_empty())
                .map_or_else(|| SELF_EXE.to_string(), str::to_string);
            let mut spec = ChildSpec::new(
                exe,
                vec![
                    crate::imu_reader::SUBCOMMAND.to_string(),
                    "--address".to_string(),
                    to_hex_addr(addr),
                ],
            );
            spec.arg0 = Some(crate::imu_reader::PROCESS_NAME.to_string());
            spec
        }
    }
}

/// The argv the tape's `imu_spawn` records: `argv[0]` as the child sees it
/// (`arg0` when set: `xorgate-imu`), then the args. For the python backend
/// this is exactly [`ChildSpec::argv`] (`python3 …`), unchanged.
pub fn tape_argv(spec: &ChildSpec) -> Vec<String> {
    std::iter::once(spec.arg0.clone().unwrap_or_else(|| spec.program.clone()))
        .chain(spec.args.iter().cloned())
        .collect()
}

/// The IMU's edges: detection and the helper spawn (injectable for tests).
pub trait ImuIo: Send + Sync + 'static {
    /// `detectImu()`.
    fn detect<'a>(&'a self, tape: &'a dyn TapeSink) -> BoxFut<'a, ImuDetect>;
    /// `runStream(spec…)`.
    fn spawn(&self, spec: &ChildSpec) -> (ChildHandle, mpsc::UnboundedReceiver<ChildEvent>);
    /// R9's input: the device tree's `compatible` list ([`DT_COMPATIBLE`]).
    /// The collector reads it ONLY inside the SoC gate, i.e. only when the
    /// `rust` backend is selected; the default path never calls this
    /// (review-wp3 FC2, pinned in `tests/collectors_imu.rs`).
    fn dt_compatible(&self) -> std::io::Result<Vec<u8>> {
        std::fs::read(DT_COMPATIBLE)
    }
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
    /// `stop()` was called (agent shutdown): the helper's death that follows
    /// is ours, not a reader failure — the TS logs nothing there.
    stopping: bool,
    /// The running reader's chip (journal name) and address (D129).
    chip: Option<(String, u32)>,
    /// The identity the running reader's `ready` line carried.
    identity: Option<ReaderIdentity>,
}

impl State {
    /// `reset()` (`imu.ts:26-31`) — `warned` survives.
    fn reset(&mut self) {
        self.started = false;
        self.stream = None;
        self.latest = None;
        self.ever_sampled = false;
        self.chip = None;
        self.identity = None;
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
        let mut st = self.shared.lock();
        st.stopping = true;
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
        // The journal's name for the chip: the hardware block's IMU kind
        // (any string; nothing else depends on it), else `BNO08x` (D129).
        let name = imu_name(
            self.ctx
                .hardware
                .borrow()
                .as_ref()
                .and_then(|b| b.imu.as_deref()),
        );
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
                    warn!("{}", not_detected_line(&name));
                    st.warned = true;
                }
                return;
            }
        };
        info!("{}", detected_line(&name, addr));
        {
            let mut st = self.lock();
            st.chip = Some((name, addr));
            st.identity = None;
        }
        let soc_check = std::env::var(IMU_SOC_CHECK_ENV).ok();
        let spec = reader_spec(
            std::env::var(IMU_BACKEND_ENV).ok().as_deref(),
            std::env::var(IMU_READER_BIN_ENV).ok().as_deref(),
            &|| soc_allows_rust(self.io.dt_compatible(), soc_check.as_deref()),
            &self.ctx.paths.python_dir,
            addr,
        );
        self.ctx.tape.imu_spawn(&tape_argv(&spec));
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
                            if !st.ever_sampled {
                                // The first sample of this run (D129).
                                if let Some((name, addr)) = &st.chip {
                                    info!("{}", streaming_line(name, *addr, st.identity.as_ref()));
                                }
                            }
                            st.ever_sampled = true;
                        }
                        Some(ImuMessage::Ready {
                            part,
                            version,
                            build,
                            ..
                        }) => {
                            self.lock().identity = Some(ReaderIdentity {
                                part,
                                version,
                                build,
                            });
                        }
                        Some(ImuMessage::Error { error: msg }) => error!("[imu] {msg}"),
                        // An unparsed line is ignored (CF18: `latest` and its
                        // stamp unchanged).
                        None => {}
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
                    if st.stopping || self.ctx.cancel.is_cancelled() {
                        // Shutdown: the TS process exits and execa's cleanup
                        // takes the helper down without a log line (CF: no
                        // `[imu] reader stream ended` at stop). No re-detect.
                        st.stream = None;
                        break;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PY: &str = "/opt/xorgate-rs/python";

    /// The spec main (`4d66542`) built, verbatim (`collectors/imu.rs:183-191`
    /// there): the C6 reference for "knob unset → byte-identical".
    fn main_spec(python_dir: &Path, addr: u32) -> ChildSpec {
        let script = python_dir.join(IMU_HELPER);
        ChildSpec::new(
            "python3",
            vec![
                script.display().to_string(),
                "--address".to_string(),
                to_hex_addr(addr),
            ],
        )
    }

    /// The SoC gate of a python path: it must never be consulted.
    fn no_gate() -> Result<(), String> {
        panic!("the R9 SoC gate was consulted on the python path")
    }

    fn pass() -> Result<(), String> {
        Ok(())
    }

    fn same(a: &ChildSpec, b: &ChildSpec) -> bool {
        a.program == b.program
            && a.args == b.args
            && a.env == b.env
            && a.cwd == b.cwd
            && a.arg0 == b.arg0
    }

    #[test]
    fn the_backend_value_table() {
        use ImuBackend::*;
        assert_eq!(select_imu_backend(None), Ok(Python));
        assert_eq!(select_imu_backend(Some("")), Ok(Python));
        assert_eq!(select_imu_backend(Some("  ")), Ok(Python));
        assert_eq!(select_imu_backend(Some("python")), Ok(Python));
        assert_eq!(select_imu_backend(Some(" python\n")), Ok(Python));
        assert_eq!(select_imu_backend(Some("rust")), Ok(Rust));
        assert_eq!(select_imu_backend(Some("\trust ")), Ok(Rust));
        // case-sensitive; anything else is an error the caller warns about
        assert_eq!(select_imu_backend(Some("Rust")), Err("Rust".into()));
        assert_eq!(select_imu_backend(Some("PYTHON")), Err("PYTHON".into()));
        assert_eq!(select_imu_backend(Some("rust2")), Err("rust2".into()));
    }

    #[test]
    fn knob_unset_gives_a_byte_identical_python_spec() {
        let dir = PathBuf::from(PY);
        let want = main_spec(&dir, 0x4a);
        for backend in [
            None,
            Some(""),
            Some("python"),
            Some(" python "),
            Some("bogus"),
        ] {
            for bin in [None, Some(""), Some("/x/y")] {
                let got = reader_spec(backend, bin, &no_gate, &dir, 0x4a);
                assert!(same(&got, &want), "{backend:?}/{bin:?}: {got:?}");
                assert_eq!(tape_argv(&got), want.argv(), "{backend:?}/{bin:?}");
            }
        }
        assert_eq!(
            want.argv(),
            [
                "python3",
                "/opt/xorgate-rs/python/bno085_read.py",
                "--address",
                "0x4a"
            ]
        );
        // 0x4b too (the other address detection can report)
        assert!(same(
            &reader_spec(None, None, &no_gate, &dir, 0x4b),
            &main_spec(&dir, 0x4b)
        ));
    }

    #[test]
    fn rust_reexecs_proc_self_exe_as_xorgate_imu() {
        let dir = PathBuf::from(PY);
        for bin in [None, Some(""), Some("   ")] {
            let s = reader_spec(Some("rust"), bin, &pass, &dir, 0x4a);
            assert_eq!(s.program, "/proc/self/exe", "{bin:?}: a blank BIN is unset");
            assert_eq!(s.arg0.as_deref(), Some("xorgate-imu"));
            assert_eq!(s.args, ["__imu-reader", "--address", "0x4a"]);
            assert!(s.env.is_empty() && s.cwd.is_none());
            assert_eq!(
                tape_argv(&s),
                ["xorgate-imu", "__imu-reader", "--address", "0x4a"]
            );
        }
        assert_eq!(SELF_EXE, "/proc/self/exe");
        let s = reader_spec(
            Some("rust"),
            Some("/rig/cache/bin/xorgate-replay"),
            &pass,
            &dir,
            0x4b,
        );
        assert_eq!(s.program, "/rig/cache/bin/xorgate-replay");
        assert_eq!(s.arg0.as_deref(), Some("xorgate-imu"));
        assert_eq!(s.args, ["__imu-reader", "--address", "0x4b"]);
    }

    // ---- R9 (Q-C4, review-wp3 F4): the SoC allow-list ----

    const CM5: &[u8] = b"raspberrypi,5-compute-module\0brcm,bcm2712\0";
    const CM4: &[u8] = b"raspberrypi,4-compute-module\0brcm,bcm2711\0";

    fn unreadable() -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    #[test]
    fn r9_bcm2712_allows_rust() {
        assert_eq!(soc_allows_rust(Ok(CM5.to_vec()), None), Ok(()));
        assert_eq!(soc_allows_rust(Ok(b"brcm,bcm2712".to_vec()), None), Ok(()));
        let dir = PathBuf::from(PY);
        let gate = || soc_allows_rust(Ok(CM5.to_vec()), None);
        let s = reader_spec(Some("rust"), None, &gate, &dir, 0x4a);
        assert_eq!(s.program, SELF_EXE);
        assert_eq!(s.arg0.as_deref(), Some("xorgate-imu"));
    }

    #[test]
    fn r9_bcm2711_falls_back_to_python() {
        let e = soc_allows_rust(Ok(CM4.to_vec()), None).unwrap_err();
        assert!(e.contains("does not name brcm,bcm2712"), "{e}");
        // exact entries only: no substring or prefix match
        for near in [
            &b"brcm,bcm27120\0"[..],
            &b"xbrcm,bcm2712\0"[..],
            &b"brcm,bcm2712x"[..],
            &b"brcm,bcm271"[..],
            &b""[..],
        ] {
            assert!(
                soc_allows_rust(Ok(near.to_vec()), None).is_err(),
                "{near:?}"
            );
        }
        let dir = PathBuf::from(PY);
        let gate = || soc_allows_rust(Ok(CM4.to_vec()), None);
        let s = reader_spec(Some("rust"), Some("/x"), &gate, &dir, 0x4a);
        assert!(same(&s, &main_spec(&dir, 0x4a)), "{s:?}");
    }

    #[test]
    fn r9_an_unreadable_dt_falls_back_to_python() {
        let e = soc_allows_rust(unreadable(), None).unwrap_err();
        assert!(e.contains("is unreadable"), "{e}");
        let dir = PathBuf::from(PY);
        let gate = || soc_allows_rust(unreadable(), None);
        let s = reader_spec(Some("rust"), None, &gate, &dir, 0x4a);
        assert!(same(&s, &main_spec(&dir, 0x4a)), "{s:?}");
    }

    #[test]
    fn r9_the_soc_check_override_allows_rust_anywhere() {
        for dt in [Ok(CM4.to_vec()), unreadable(), Ok(CM5.to_vec())] {
            assert_eq!(soc_allows_rust(dt, Some("off")), Ok(()));
        }
        assert_eq!(soc_allows_rust(Ok(CM4.to_vec()), Some(" off\n")), Ok(()));
        // only exactly `off` (trimmed, case-sensitive) lifts it
        for v in ["OFF", "0", "", "no", "false", "disabled"] {
            assert!(soc_allows_rust(Ok(CM4.to_vec()), Some(v)).is_err(), "{v:?}");
        }
        let dir = PathBuf::from(PY);
        let gate = || soc_allows_rust(unreadable(), Some("off"));
        let s = reader_spec(Some("rust"), None, &gate, &dir, 0x4a);
        assert_eq!(s.program, SELF_EXE);
        assert_eq!(s.arg0.as_deref(), Some("xorgate-imu"));
        assert_eq!(IMU_SOC_CHECK_ENV, "XORGATE_IMU_SOC_CHECK");
        assert_eq!(DT_COMPATIBLE, "/proc/device-tree/compatible");
    }
}
