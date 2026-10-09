//! `xorgate-replay play` — the rig player (runs INSIDE the rig container).
//!
//! Starts two agents (JS `node dist/worker.js` and/or `xorgate-rs run`) with
//! distinct deviceIds against one TLS mosquitto, feeds both the SAME tape inputs
//! through the OS-boundary knobs the TS already reads (phase file §Record/replay
//! "Black-box"), and captures everything they publish and write.
//!
//! Per agent (`<out>/<label>/`):
//! - `GPS_NMEA_PORT` → a pty in raw mode (no echo, no CR/NL translation; the
//!   player keeps the slave open so a reopen never sees a hangup);
//! - `LTE_TELEMETRY_FILE` → a file the player rewrites atomically (or removes);
//! - `XORGATE_PYTHON_DIR` → the fake `bno085_read.py`, which relays the lines the
//!   player sends on the agent's unix socket and exits on `__EXIT__ <code>`;
//! - PATH shims (`vcgencmd`, `i2cdetect`, `timedatectl`, `sudo`, `rpicam-hello`)
//!   answering from `RIG_STATE` (per-argv files the player writes) and logging
//!   sudo argv to `sudo.log`;
//! - `IOT_DATA_ENDPOINT=broker` resolves to this container, where a TCP proxy
//!   forwards to the real mosquitto: a tape `conn closed` span is reproduced by
//!   dropping every proxied connection and refusing new ones until the span ends
//!   (the phase file says "docker pause of the broker"; the proxy does the same
//!   thing to the agents without freezing the capture client or needing the
//!   docker socket in the rig — see the README);
//! - KVS_MANIFEST `{"channels":[]}`, HARDWARE_MANIFEST from the tape meta,
//!   KVS_PIPELINE_DIR / XORGATE_HOME scratch, `TZ=UTC`, `MODE=real`.
//!
//! Input timing — the hold gate. Both agents tick at 1 Hz with their OWN phase
//! (and Node's `setInterval` drifts ~1 ms/tick). An input landing between the
//! two agents' ticks would be seen by one tick and not by its partner, which
//! would make every stream-fed field (gps, imu, lte) differ by one sample. The
//! player therefore never changes an input between the two ticks of a pair: it
//! predicts each agent's next tick from the `ts` of its last telemetry publish
//! and HOLDS releases from `lead tick − guard` until the partner's tick has
//! been observed. Inputs are delayed by at most one pair gap (< 1 s), their
//! order is preserved, and each pair is written to `pairs.ndjson` so the diff
//! compares exactly the ticks that saw the same inputs. When the drifting gap
//! exceeds 800 ms the roles flip and the one lead tick in between is recorded
//! as a `skip` (it has no partner that saw the same inputs).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use xorgate_hal::tape::{wall_now_ms, ConnState, Tape, TapeEvent};

use crate::diff::{PlayInfo, FILES};
use crate::rewrite;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AgentKind {
    Js,
    Rust,
}

impl AgentKind {
    fn as_str(self) -> &'static str {
        match self {
            AgentKind::Js => "js",
            AgentKind::Rust => "rust",
        }
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct PlayArgs {
    #[arg(long)]
    pub tape: PathBuf,
    #[arg(long)]
    pub out: PathBuf,
    /// Container-local scratch for homes, ptys, unix sockets and shims (bind
    /// mounts from macOS cannot hold sockets).
    #[arg(long, default_value = "/tmp/rig-work")]
    pub work: PathBuf,
    #[arg(long, value_enum, default_value = "js")]
    pub a: AgentKind,
    #[arg(long, value_enum, default_value = "rust")]
    pub b: AgentKind,
    /// The JS build (`dist/worker.js`, `dist/cli.js`, `node_modules/`).
    #[arg(long, default_value = "/rig/cache/js-89b5c64")]
    pub js_dir: PathBuf,
    #[arg(long, default_value = "/rig/cache/bin/xorgate-rs")]
    pub rs_bin: PathBuf,
    #[arg(long, default_value = "node")]
    pub node: String,
    /// The real broker (capture client + proxy upstream).
    #[arg(long, default_value = "mosquitto")]
    pub broker_host: String,
    #[arg(long, default_value_t = 8883)]
    pub broker_port: u16,
    /// Where the agents connect (`broker` → 127.0.0.1 in the rig).
    #[arg(long, default_value = "0.0.0.0:8883")]
    pub proxy_listen: String,
    #[arg(long, default_value = "broker")]
    pub endpoint: String,
    #[arg(long, default_value = "/rig/certs")]
    pub certs: PathBuf,
    #[arg(long, default_value = "/rig/tools/shims")]
    pub shims: PathBuf,
    #[arg(long, default_value = "/rig/tools/fake-python")]
    pub fake_python: PathBuf,
    /// Real time only: other speeds are NOT parity evidence (timers differ).
    #[arg(long, default_value_t = 1.0)]
    pub speed: f64,
    /// Do not shift embedded wall times (rewrite.rs).
    #[arg(long)]
    pub no_shift: bool,
    /// Keep running this long after the last tape event.
    #[arg(long, default_value_t = 20)]
    pub settle_s: u64,
    /// Stop the tape after this many seconds.
    #[arg(long)]
    pub max_s: Option<u64>,
    #[arg(long, default_value_t = 60)]
    pub guard_ms: i64,
    /// Disable the hold gate (debugging only; not parity evidence).
    #[arg(long)]
    pub no_gate: bool,
    /// Put the `rpicam-hello` shim ("No cameras available!") on PATH; by
    /// default the tool is ABSENT (phase file).
    #[arg(long)]
    pub with_rpicam: bool,
}

pub const DEVICE_A: &str = "00000000-0000-4000-8000-00000000000a";
pub const DEVICE_B: &str = "00000000-0000-4000-8000-00000000000b";
const INTERVAL_MS: i64 = 1000;
const FLIP_GAP_MS: i64 = 800;
/// A hold whose partner tick was not observed ends this long after the
/// partner's PREDICTED tick (connected) …
const HOLD_GRACE_MS: i64 = 500;
/// … or this long while the broker path is cut (nothing is observable then;
/// the prediction alone keeps the pairs consistent).
const HOLD_GRACE_CUT_MS: i64 = 100;
const STALE_MS: i64 = 2500;
const PREFILL_MS: u64 = 1000;

// ------------------------------------------------------------------ gate

#[derive(Debug, Clone)]
struct Hold {
    start: i64,
    /// Predicted tick of the lagging agent.
    lag_due: i64,
    lead: usize,
    lead_tick: Option<i64>,
    lag_tick: Option<i64>,
}

/// The hold gate (module doc). Indices 0 = A, 1 = B.
#[derive(Debug)]
pub struct Gate {
    last: [Option<i64>; 2],
    hold: Option<Hold>,
    open_after: i64,
    skip_until: [i64; 2],
    guard_before: i64,
    guard_after: i64,
    pub pairs: Vec<Value>,
}

impl Gate {
    pub fn new(guard_ms: i64) -> Self {
        Self {
            last: [None, None],
            hold: None,
            open_after: 0,
            skip_until: [i64::MIN, i64::MIN],
            guard_before: guard_ms,
            guard_after: 20,
            pairs: Vec::new(),
        }
    }

    /// A telemetry publish of agent `idx` with payload `ts` arrived at `now`.
    pub fn on_tick(&mut self, idx: usize, ts: i64, now: i64) {
        self.last[idx] = Some(ts);
        let Some(h) = self.hold.as_mut() else { return };
        if ts < h.start {
            return;
        }
        if idx == h.lead {
            h.lead_tick.get_or_insert(ts);
        } else {
            h.lag_tick.get_or_insert(ts);
        }
        if let (Some(l), Some(g)) = (h.lead_tick, h.lag_tick) {
            let (a, b) = if h.lead == 0 { (l, g) } else { (g, l) };
            self.pairs.push(json!({"a": a, "b": b}));
            self.hold = None;
            self.open_after = now + self.guard_after;
        }
    }

    fn next_due(&self, last: i64, now: i64) -> i64 {
        // Smallest last + k·interval (k ≥ 1) not older than the guard: a tick
        // that is due but not yet observed still counts as upcoming.
        let k = ((now - self.guard_before - last) as f64 / INTERVAL_MS as f64).ceil() as i64;
        last + k.max(1) * INTERVAL_MS
    }

    /// May an input change now?
    pub fn is_open(&mut self, now: i64, cut: bool) -> bool {
        let (Some(a), Some(b)) = (self.last[0], self.last[1]) else {
            return true;
        };
        // An agent that stopped publishing while connected (crash, wedge)
        // cannot be paired: no gating until both tick again. During a cut the
        // agents keep ticking unobserved (JS queues QoS 0 offline, Rust skips),
        // so the predictions keep gating.
        if !cut && (now - a > STALE_MS || now - b > STALE_MS) {
            self.hold = None;
            return true;
        }
        if let Some(h) = &self.hold {
            let grace = if cut {
                HOLD_GRACE_CUT_MS
            } else {
                HOLD_GRACE_MS
            };
            if now > h.lag_due + grace {
                self.hold = None;
                self.open_after = now + self.guard_after;
            } else {
                return false;
            }
        }
        if now < self.open_after {
            return false;
        }
        let mut n = [self.next_due(a, now), self.next_due(b, now)];
        for (due, skip) in n.iter_mut().zip(self.skip_until) {
            if *due <= skip {
                *due += INTERVAL_MS;
            }
        }
        let lead = if n[0] <= n[1] { 0 } else { 1 };
        let gap = n[1 - lead] - n[lead];
        if now < n[lead] - self.guard_before {
            return true;
        }
        if gap > FLIP_GAP_MS {
            // Flip: this lead tick has no partner that will see the same inputs.
            self.pairs
                .push(json!({"skip": if lead == 0 { "a" } else { "b" }, "ts": n[lead]}));
            self.skip_until[lead] = n[lead] + self.guard_before;
            return true;
        }
        self.hold = Some(Hold {
            start: n[lead] - self.guard_before,
            lag_due: n[1 - lead],
            lead,
            lead_tick: None,
            lag_tick: None,
        });
        false
    }
}

// ------------------------------------------------------------------ agents

struct Agent {
    label: &'static str,
    kind: AgentKind,
    device_id: &'static str,
    /// Capture output (`<out>/<label>`).
    dir: PathBuf,
    /// Runtime scratch (`<work>/<label>`).
    work: PathBuf,
    home: PathBuf,
    state: PathBuf,
    lte_path: PathBuf,
    pty_master: File,
    _pty_slave: OwnedFd,
    pty_path: PathBuf,
    imu: Arc<tokio::sync::Mutex<Option<tokio::net::unix::OwnedWriteHalf>>>,
    env: Vec<(String, String)>,
    child: Option<Child>,
    pty_drops: u64,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp-rig");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn open_pty() -> Result<(File, OwnedFd, PathBuf)> {
    use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
    let pty = nix::pty::openpty(None, None).context("openpty")?;
    let mut t = tcgetattr(&pty.slave).context("tcgetattr")?;
    cfmakeraw(&mut t);
    tcsetattr(&pty.slave, SetArg::TCSANOW, &t).context("tcsetattr")?;
    let path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
        .context("pty slave path")?;
    // Non-blocking master: a reader that stopped reading must not stall the
    // player (a full tty buffer drops data, like a real UART overrun).
    let flags = nix::fcntl::fcntl(&pty.master, nix::fcntl::FcntlArg::F_GETFL)?;
    let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
    oflags.insert(nix::fcntl::OFlag::O_NONBLOCK);
    nix::fcntl::fcntl(&pty.master, nix::fcntl::FcntlArg::F_SETFL(oflags))?;
    Ok((File::from(pty.master), pty.slave, path))
}

impl Agent {
    fn prepare(
        label: &'static str,
        kind: AgentKind,
        device_id: &'static str,
        args: &PlayArgs,
        tape: &Tape,
        i2c_absent: bool,
    ) -> Result<Self> {
        let dir = args.out.join(label);
        let work = args.work.join(label);
        let _ = fs::remove_dir_all(&work);
        let home = work.join("home");
        let state = work.join("state");
        let bin = work.join("bin");
        let etc = work.join("etc");
        for d in [
            &home,
            &state.join("exec"),
            &bin,
            &etc.join("pipelines"),
            &dir.join("final"),
        ] {
            fs::create_dir_all(d)?;
        }
        // PATH shims (copied so i2cdetect can be absent per agent).
        for entry in
            fs::read_dir(&args.shims).with_context(|| format!("{}", args.shims.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            if (i2c_absent && name == "i2cdetect") || (!args.with_rpicam && name == "rpicam-hello")
            {
                continue;
            }
            let dst = bin.join(&name);
            fs::copy(entry.path(), &dst)?;
            fs::set_permissions(&dst, fs::Permissions::from_mode(0o755))?;
        }
        // Manifests.
        fs::write(
            etc.join("kvs.json"),
            json!({"channels": tape.meta.kvs.channels}).to_string(),
        )?;
        let hw_path = etc.join("hardware.json");
        if !tape.meta.hardware.is_null() {
            fs::write(&hw_path, serde_json::to_vec_pretty(&tape.meta.hardware)?)?;
        }
        // Home seeded from the tape's start state.
        if let Some(files) = &tape.meta.home {
            for (name, text) in files {
                if let Some(t) = text.as_str() {
                    let t = t.replace(&tape.meta.device_id, device_id);
                    fs::write(home.join(name), t)?;
                }
            }
        }
        let (pty_master, pty_slave, pty_path) = open_pty()?;
        let gps_link = work.join("gps-tty");
        let _ = fs::remove_file(&gps_link);
        std::os::unix::fs::symlink(&pty_path, &gps_link)?;
        let lte_path = state.join("telemetry.json");
        let path_env = format!(
            "{}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            bin.display()
        );
        let cert = |n: &str| args.certs.join(n).display().to_string();
        let env: Vec<(String, String)> = vec![
            ("PATH", path_env),
            ("HOME", work.display().to_string()),
            ("TZ", "UTC".into()),
            ("MODE", "real".into()),
            ("RATE_HZ", "1".into()),
            ("DEVICE_ID", device_id.into()),
            ("IOT_DATA_ENDPOINT", args.endpoint.clone()),
            ("CERT_PATH", cert(&format!("{label}.crt"))),
            ("KEY_PATH", cert(&format!("{label}.key"))),
            ("CA_PATH", cert("ca.pem")),
            ("XORGATE_HOME", home.display().to_string()),
            ("GPS_NMEA_PORT", gps_link.display().to_string()),
            ("LTE_TELEMETRY_FILE", lte_path.display().to_string()),
            ("XORGATE_PYTHON_DIR", args.fake_python.display().to_string()),
            ("KVS_MANIFEST", etc.join("kvs.json").display().to_string()),
            ("HARDWARE_MANIFEST", hw_path.display().to_string()),
            (
                "KVS_PIPELINE_DIR",
                etc.join("pipelines").display().to_string(),
            ),
            (
                "KVS_RUNTIME_BIN",
                work.join("no-kvs-runtime").display().to_string(),
            ),
            ("XORGATE_MOCK", "0".into()),
            ("RIG_STATE", state.display().to_string()),
            ("RIG_SUDO_LOG", work.join("sudo.log").display().to_string()),
            ("RIG_IMU_SOCK", work.join("imu.sock").display().to_string()),
            ("NO_COLOR", "1".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        fs::write(
            dir.join("meta.json"),
            serde_json::to_vec_pretty(&json!({
                "label": label, "kind": kind.as_str(), "deviceId": device_id,
                "home": home, "ptyPath": pty_path, "env": env.iter().cloned().collect::<BTreeMap<_, _>>(),
            }))?,
        )?;
        Ok(Self {
            label,
            kind,
            device_id,
            dir,
            work,
            home,
            state,
            lte_path,
            pty_master,
            _pty_slave: pty_slave,
            pty_path,
            imu: Arc::new(tokio::sync::Mutex::new(None)),
            env,
            child: None,
            pty_drops: 0,
        })
    }

    fn command(&self, args: &PlayArgs, argv: &[String], worker: bool) -> Command {
        let mut cmd = match self.kind {
            AgentKind::Js => {
                let mut c = Command::new(&args.node);
                let entry = if worker {
                    "dist/worker.js"
                } else {
                    "dist/cli.js"
                };
                c.arg(args.js_dir.join(entry));
                c
            }
            AgentKind::Rust => {
                let mut c = Command::new(&args.rs_bin);
                if worker {
                    c.arg("run");
                }
                c
            }
        };
        cmd.args(argv);
        cmd.env_clear();
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.current_dir(&self.home);
        cmd
    }

    fn start(&mut self, args: &PlayArgs) -> Result<()> {
        let mut cmd = self.command(args, &[], true);
        cmd.stdout(Stdio::from(File::create(self.dir.join("stdout.log"))?));
        cmd.stderr(Stdio::from(File::create(self.dir.join("stderr.log"))?));
        cmd.stdin(Stdio::null());
        cmd.process_group(0);
        cmd.kill_on_drop(true);
        self.child = Some(
            cmd.spawn()
                .with_context(|| format!("starting agent {}", self.label))?,
        );
        Ok(())
    }

    fn nmea(&mut self, line: &str) {
        let mut buf = line.as_bytes().to_vec();
        buf.push(b'\n');
        match self.pty_master.write(&buf) {
            Ok(n) if n == buf.len() => {}
            _ => self.pty_drops += 1,
        }
    }

    fn lte(&self, bytes: Option<&[u8]>) -> Result<()> {
        match bytes {
            Some(b) => write_atomic(&self.lte_path, b),
            None => {
                let _ = fs::remove_file(&self.lte_path);
                Ok(())
            }
        }
    }

    fn exec(&self, argv: &[String], stdout: &str, code: Option<i32>) -> Result<()> {
        let key = argv.join(" ").replace([' ', '/'], "_");
        let key = match key.split_once('_') {
            Some((tool, rest)) => format!("{tool}__{rest}"),
            None => key,
        };
        let base = self.state.join("exec").join(key);
        write_atomic(&base.with_extension("out"), stdout.as_bytes())?;
        write_atomic(
            &base.with_extension("code"),
            code.unwrap_or(127).to_string().as_bytes(),
        )
    }

    fn i2c(&self, stdout: Option<&str>) -> Result<()> {
        match stdout {
            Some(s) => write_atomic(&self.state.join("i2cdetect.out"), s.as_bytes()),
            None => {
                let _ = fs::remove_file(self.state.join("i2cdetect.out"));
                Ok(())
            }
        }
    }

    async fn imu_line(&self, line: &str) {
        let mut g = self.imu.lock().await;
        if let Some(w) = g.as_mut() {
            let mut b = line.as_bytes().to_vec();
            b.push(b'\n');
            if w.write_all(&b).await.is_err() {
                *g = None;
            }
        }
    }

    async fn imu_exit(&self, code: Option<i32>) {
        let mut g = self.imu.lock().await;
        if let Some(mut w) = g.take() {
            let _ = w
                .write_all(format!("__EXIT__ {}\n", code.unwrap_or(1)).as_bytes())
                .await;
        }
    }

    /// SIGTERM the agent (both agents are signalled together, then awaited).
    fn signal_term(&self) {
        if let Some(pid) = self.child.as_ref().and_then(|c| c.id()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }

    /// Wait up to 30 s for the graceful exit, then SIGKILL the process group
    /// (which also takes a surviving IMU helper).
    async fn wait_stopped(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        let pgid = child.id().map(|p| nix::unistd::Pid::from_raw(p as i32));
        let started = std::time::Instant::now();
        let graceful = tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await
            .is_ok();
        if let Some(pg) = pgid {
            let _ = nix::sys::signal::killpg(pg, nix::sys::signal::Signal::SIGKILL);
        }
        if !graceful {
            let _ = child.wait().await;
        }
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("stderr.log"))
        {
            let _ = writeln!(
                f,
                "[rig] agent {} {} after SIGTERM in {} ms",
                self.label,
                if graceful {
                    "exited"
                } else {
                    "did NOT exit in 30 s; SIGKILLed"
                },
                started.elapsed().as_millis()
            );
        }
    }
}

async fn imu_acceptor(
    sock: PathBuf,
    slot: Arc<tokio::sync::Mutex<Option<tokio::net::unix::OwnedWriteHalf>>>,
    log: Arc<Mutex<File>>,
) -> Result<()> {
    let _ = fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let (_r, w) = stream.into_split();
        if let Ok(mut f) = log.lock() {
            let _ = writeln!(f, "{} imu helper connected", wall_now_ms());
        }
        *slot.lock().await = Some(w);
    }
}

// ------------------------------------------------------------------ broker side

async fn proxy(
    listen: String,
    upstream: String,
    cut: Arc<Mutex<Option<CancellationToken>>>,
) -> Result<()> {
    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("proxy bind {listen}"))?;
    loop {
        let (mut inbound, _) = listener.accept().await?;
        let token = cut.lock().ok().and_then(|g| g.clone());
        let Some(token) = token else {
            drop(inbound); // span closed: refuse
            continue;
        };
        let upstream = upstream.clone();
        tokio::spawn(async move {
            let Ok(mut out) = TcpStream::connect(&upstream).await else {
                return;
            };
            tokio::select! {
                _ = tokio::io::copy_bidirectional(&mut inbound, &mut out) => {}
                _ = token.cancelled() => {}
            }
        });
    }
}

fn tls_config(certs: &Path) -> Result<Arc<rustls::ClientConfig>> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(certs.join("ca.pem"))? {
        roots.add(c?)?;
    }
    let chain: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(certs.join("player.crt"))?.collect::<Result<_, _>>()?;
    let key = PrivateKeyDer::from_pem_file(certs.join("player.key"))?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)?;
    Ok(Arc::new(cfg))
}

struct Captures {
    files: [Mutex<File>; 2],
    inputs: Mutex<File>,
    gate: Mutex<Gate>,
    start_wall: i64,
}

fn mqtt_options(args: &PlayArgs, id: &str) -> Result<rumqttc::v5::MqttOptions> {
    let mut o = rumqttc::v5::MqttOptions::new(id, &args.broker_host, args.broker_port);
    o.set_transport(rumqttc::Transport::Tls(rumqttc::TlsConfiguration::Rustls(
        tls_config(&args.certs)?,
    )));
    o.set_keep_alive(Duration::from_secs(30));
    o.set_clean_start(true);
    o.set_max_packet_size(Some(1_000_000));
    Ok(o)
}

async fn capture_loop(
    mut el: rumqttc::v5::EventLoop,
    client: rumqttc::v5::AsyncClient,
    caps: Arc<Captures>,
) {
    use rumqttc::v5::mqttbytes::v5::Packet;
    use rumqttc::v5::Event;
    loop {
        match el.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                let _ = client
                    .subscribe("xorgate/#", rumqttc::v5::mqttbytes::QoS::AtLeastOnce)
                    .await;
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let now = wall_now_ms();
                let topic = String::from_utf8_lossy(&p.topic).to_string();
                let payload = String::from_utf8_lossy(&p.payload).to_string();
                let idx = if topic.contains(DEVICE_A) {
                    0
                } else if topic.contains(DEVICE_B) {
                    1
                } else {
                    continue;
                };
                let inbound = topic.ends_with("/config") || topic.ends_with("/command");
                let line = json!({
                    "t": now - caps.start_wall, "w": now, "topic": topic,
                    "retain": p.retain, "qos": p.qos as u8, "payload": payload,
                })
                .to_string();
                let file = if inbound {
                    &caps.inputs
                } else {
                    &caps.files[idx]
                };
                if let Ok(mut f) = file.lock() {
                    let _ = writeln!(f, "{line}");
                }
                if topic.ends_with("/telemetry") {
                    if let Some(ts) = serde_json::from_str::<Value>(&payload)
                        .ok()
                        .and_then(|v| v["ts"].as_i64())
                    {
                        if let Ok(mut g) = caps.gate.lock() {
                            g.on_tick(idx, ts, now);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

async fn snapshot_loop(homes: Vec<(PathBuf, PathBuf)>, stop: CancellationToken) {
    let mut last: BTreeMap<(usize, &str), Option<Vec<u8>>> = BTreeMap::new();
    let mut files: Vec<File> = homes
        .iter()
        .map(|(_, dir)| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("files.ndjson"))
                .expect("files.ndjson")
        })
        .collect();
    loop {
        for (i, (home, _)) in homes.iter().enumerate() {
            for name in FILES {
                let cur = fs::read(home.join(name)).ok();
                let prev = last.get(&(i, *name));
                if prev != Some(&cur) {
                    let text = cur.as_ref().map(|b| String::from_utf8_lossy(b).to_string());
                    let _ = writeln!(
                        files[i],
                        "{}",
                        json!({"w": wall_now_ms(), "name": name, "text": text})
                    );
                    last.insert((i, name), cur);
                }
            }
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(1000)) => {}
        }
    }
}

fn gga_quality(line: &str) -> Option<String> {
    let body = line.trim().strip_prefix('$')?;
    let f: Vec<&str> = body.split(['*', ',']).collect();
    (f.first()?.ends_with("GGA")).then(|| f.get(6).unwrap_or(&"").to_string())
}

// ------------------------------------------------------------------ main loop

pub async fn run(args: PlayArgs) -> Result<()> {
    if (args.speed - 1.0).abs() > f64::EPSILON {
        eprintln!(
            "WARNING: --speed {} — not valid parity evidence (timers differ)",
            args.speed
        );
    }
    let tape =
        Tape::read(&args.tape).with_context(|| format!("reading {}", args.tape.display()))?;
    fs::create_dir_all(&args.out)?;
    let calibration = args.a == args.b;
    let first_i2c_absent = tape.records.iter().find_map(|r| match &r.ev {
        TapeEvent::I2cdetect { not_found, .. } => Some(*not_found),
        _ => None,
    }) == Some(true);
    let mut agents = vec![
        Agent::prepare("a", args.a, DEVICE_A, &args, &tape, first_i2c_absent)?,
        Agent::prepare("b", args.b, DEVICE_B, &args, &tape, first_i2c_absent)?,
    ];
    let log = Arc::new(Mutex::new(File::create(args.out.join("player.log"))?));
    let plog = |msg: String| {
        if let Ok(mut f) = log.lock() {
            let _ = writeln!(f, "{} {msg}", wall_now_ms());
        }
    };

    let start_wall = wall_now_ms();
    let shift_s = if args.no_shift {
        0
    } else {
        ((start_wall - tape.meta.start_wall) as f64 / 1000.0).round() as i64
    };
    plog(format!(
        "tape {} ({} records), shift {shift_s} s, A={} B={}",
        args.tape.display(),
        tape.records.len(),
        args.a.as_str(),
        args.b.as_str()
    ));

    // Broker side: capture client, input publisher, proxy.
    let caps = Arc::new(Captures {
        files: [
            Mutex::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(agents[0].dir.join("mqtt.ndjson"))?,
            ),
            Mutex::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(agents[1].dir.join("mqtt.ndjson"))?,
            ),
        ],
        inputs: Mutex::new(File::create(args.out.join("inputs-mqtt.ndjson"))?),
        gate: Mutex::new(Gate::new(args.guard_ms)),
        start_wall,
    });
    let (cap_client, cap_el) =
        rumqttc::v5::AsyncClient::new(mqtt_options(&args, "rig-capture")?, 1024);
    tokio::spawn(capture_loop(cap_el, cap_client.clone(), caps.clone()));
    let (pub_client, mut pub_el) =
        rumqttc::v5::AsyncClient::new(mqtt_options(&args, "rig-player")?, 1024);
    tokio::spawn(async move {
        loop {
            if pub_el.poll().await.is_err() {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    });
    let cut: Arc<Mutex<Option<CancellationToken>>> =
        Arc::new(Mutex::new(Some(CancellationToken::new())));
    let upstream = format!("{}:{}", args.broker_host, args.broker_port);
    tokio::spawn(proxy(args.proxy_listen.clone(), upstream, cut.clone()));

    // Clear any retained config from a previous run.
    for a in &agents {
        let topic = format!("xorgate/devices/{}/config", a.device_id);
        let _ = pub_client
            .publish(
                topic,
                rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
                true,
                Vec::<u8>::new(),
            )
            .await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // IMU sockets.
    for a in &agents {
        tokio::spawn(imu_acceptor(
            a.work.join("imu.sock"),
            a.imu.clone(),
            log.clone(),
        ));
    }

    // Prefill the state-like inputs the agents read at boot (the files and
    // tools existed before the recording agent started).
    let mut transitions: Vec<i64> = Vec::new();
    let mut prefilled = vec![false; tape.records.len()];
    for (i, r) in tape.records.iter().enumerate() {
        if r.t >= PREFILL_MS {
            break;
        }
        if matches!(
            r.ev,
            TapeEvent::LteFile { .. }
                | TapeEvent::Exec { .. }
                | TapeEvent::I2cdetect { .. }
                | TapeEvent::Dt { .. }
        ) {
            apply_state(&agents, &r.ev, shift_s)?;
            prefilled[i] = true;
        }
    }
    let idx = prefilled.iter().filter(|p| **p).count();
    let snap_stop = CancellationToken::new();
    tokio::spawn(snapshot_loop(
        agents
            .iter()
            .map(|a| (a.home.clone(), a.dir.clone()))
            .collect(),
        snap_stop.clone(),
    ));

    // Start both agents together.
    let t0 = tokio::time::Instant::now();
    let play_start_wall = wall_now_ms();
    for a in agents.iter_mut() {
        a.start(&args)?;
    }
    plog(format!("agents started; prefilled {idx} records"));

    let mut closed_spans: Vec<(i64, i64)> = Vec::new();
    let mut closed_since: Option<i64> = None;
    let mut retained: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut last_nmea: Option<(u64, i64)> = None;
    let mut last_quality: Option<String> = None;
    let mut last_imu: Option<u64> = None;
    let mut wall_steps: Vec<(i64, i64)> = Vec::new();
    let mut prev_tw: Option<(u64, i64)> = None;
    let mut released = 0usize;
    let cutoff = args.max_s.map(|s| s * 1000).unwrap_or(u64::MAX);

    for (i, r) in tape.records.iter().enumerate() {
        if prefilled[i] {
            continue;
        }
        if r.t > cutoff {
            break;
        }
        let due = t0 + Duration::from_millis((r.t as f64 / args.speed) as u64);
        tokio::time::sleep_until(due).await;
        if !args.no_gate {
            let cut_active = closed_since.is_some();
            loop {
                let open = caps
                    .gate
                    .lock()
                    .map(|mut g| g.is_open(wall_now_ms(), cut_active))
                    .unwrap_or(true);
                if open {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        let now = wall_now_ms();
        if let Some((pt, pw)) = prev_tw {
            let skew = (r.w - pw) - (r.t as i64 - pt as i64);
            if skew.abs() > crate::inspect::STEP_THRESHOLD_MS {
                wall_steps.push((now, skew));
            }
        }
        prev_tw = Some((r.t, r.w));
        released += 1;
        match &r.ev {
            TapeEvent::Nmea { line } => {
                let line = rewrite::shift_nmea_line(line, shift_s);
                if let Some((lt, lw)) = last_nmea {
                    if r.t - lt > 2_000 {
                        transitions.push(lw);
                        transitions.push(now);
                    }
                } else {
                    transitions.push(now);
                }
                if let Some(q) = gga_quality(&line) {
                    if last_quality.as_ref() != Some(&q) {
                        transitions.push(now);
                        last_quality = Some(q);
                    }
                }
                last_nmea = Some((r.t, now));
                for a in agents.iter_mut() {
                    a.nmea(&line);
                }
            }
            TapeEvent::Imu { line } => {
                if last_imu.is_none_or(|lt| r.t - lt > 2_000) {
                    transitions.push(now);
                }
                last_imu = Some(r.t);
                for a in &agents {
                    a.imu_line(line).await;
                }
            }
            TapeEvent::ImuSpawn { .. } => {
                transitions.push(now);
                plog(format!("tape imu_spawn at t={}", r.t));
            }
            TapeEvent::ImuExit { code } => {
                transitions.push(now);
                for a in &agents {
                    a.imu_exit(*code).await;
                }
            }
            TapeEvent::LteFile { .. }
            | TapeEvent::Exec { .. }
            | TapeEvent::I2cdetect { .. }
            | TapeEvent::Dt { .. } => {
                transitions.push(now);
                apply_state(&agents, &r.ev, shift_s)?;
            }
            TapeEvent::MqttIn {
                topic, retain, qos, ..
            } => {
                transitions.push(now);
                let payload = r.ev.mqtt_payload().unwrap_or_default();
                let is_config = topic.ends_with("/config");
                let is_command = topic.ends_with("/command");
                for a in &agents {
                    let t = topic.replace(&tape.meta.device_id, a.device_id);
                    let mut p = rewrite::substitute_id(&payload, &tape.meta.device_id, a.device_id);
                    if is_command {
                        p = rewrite::shift_command_payload(&p, shift_s * 1000);
                    }
                    if is_config && *retain && retained.get(&t) == Some(&p) {
                        // A retained REDELIVERY: the broker redelivers on the
                        // agent's resubscribe by itself.
                        continue;
                    }
                    let q = if *qos >= 1 {
                        rumqttc::v5::mqttbytes::QoS::AtLeastOnce
                    } else {
                        rumqttc::v5::mqttbytes::QoS::AtMostOnce
                    };
                    // The cloud publishes config retained; commands are not.
                    let ret = is_config;
                    if is_config {
                        retained.insert(t.clone(), p.clone());
                    }
                    let _ = pub_client.publish(t, q, ret, p).await;
                }
            }
            TapeEvent::Conn { state } => {
                transitions.push(now);
                match state {
                    ConnState::Closed => {
                        if closed_since.is_none() {
                            closed_since = Some(now);
                            if let Ok(mut g) = cut.lock() {
                                if let Some(tok) = g.take() {
                                    tok.cancel();
                                }
                            }
                            plog(format!("conn closed (proxy cut) at t={}", r.t));
                        }
                    }
                    ConnState::Connected => {
                        if let Some(s) = closed_since.take() {
                            closed_spans.push((s, now));
                            if let Ok(mut g) = cut.lock() {
                                *g = Some(CancellationToken::new());
                            }
                            plog(format!("conn restored at t={}", r.t));
                        }
                    }
                }
            }
            TapeEvent::Cli { argv } => {
                transitions.push(now);
                for a in &agents {
                    let mut cmd = a.command(&args, argv, false);
                    let out = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(a.dir.join("cli.log"))?;
                    let mut header = out.try_clone()?;
                    let _ = writeln!(header, "=== {} $ {}", now, argv.join(" "));
                    cmd.stdout(Stdio::from(out.try_clone()?));
                    cmd.stderr(Stdio::from(out));
                    cmd.stdin(Stdio::null());
                    match cmd.spawn() {
                        Ok(mut child) => {
                            tokio::spawn(async move {
                                let st =
                                    tokio::time::timeout(Duration::from_secs(60), child.wait())
                                        .await;
                                let _ = writeln!(
                                    header,
                                    "=== exit {:?}",
                                    st.ok().and_then(|r| r.ok()).and_then(|s| s.code())
                                );
                            });
                        }
                        Err(e) => {
                            let _ = writeln!(header, "=== spawn failed: {e}");
                        }
                    }
                }
            }
            TapeEvent::Clock { .. } | TapeEvent::Proc { .. } => {}
        }
    }
    if let Some(s) = closed_since.take() {
        closed_spans.push((s, wall_now_ms()));
        if let Ok(mut g) = cut.lock() {
            *g = Some(CancellationToken::new());
        }
    }
    plog(format!(
        "tape done ({released} released); settling {} s",
        args.settle_s
    ));
    tokio::time::sleep(Duration::from_secs(args.settle_s)).await;

    let end_wall = wall_now_ms();
    snap_stop.cancel();
    for a in agents.iter() {
        a.signal_term();
    }
    let (first, second) = agents.split_at_mut(1);
    tokio::join!(first[0].wait_stopped(), second[0].wait_stopped());
    // Final file copies.
    for a in &agents {
        for name in FILES {
            if let Ok(b) = fs::read(a.home.join(name)) {
                fs::write(a.dir.join("final").join(name), b)?;
            }
        }
        if let Ok(b) = fs::read(a.work.join("sudo.log")) {
            fs::write(a.dir.join("sudo.log"), b)?;
        }
        if a.pty_drops > 0 {
            plog(format!(
                "agent {}: {} pty writes dropped (reader not reading)",
                a.label, a.pty_drops
            ));
        }
        let _ = &a.pty_path;
    }
    let pairs = caps
        .gate
        .lock()
        .map(|g| g.pairs.clone())
        .unwrap_or_default();
    let mut pf = File::create(args.out.join("pairs.ndjson"))?;
    for p in &pairs {
        writeln!(pf, "{p}")?;
    }
    transitions.sort();
    transitions.dedup();
    let info = PlayInfo {
        tape: args.tape.display().to_string(),
        start_wall: play_start_wall,
        end_wall,
        shift_s,
        closed_spans,
        transitions,
        wall_steps,
        calibration,
    };
    fs::write(
        args.out.join("play.json"),
        serde_json::to_vec_pretty(&info)?,
    )?;
    plog(format!("done: {} pairs", pairs.len()));
    if pairs.is_empty() && !args.no_gate {
        bail!(
            "no telemetry pairs observed — did both agents connect? see {}/*/stderr.log",
            args.out.display()
        );
    }
    Ok(())
}

fn apply_state(agents: &[Agent], ev: &TapeEvent, shift_s: i64) -> Result<()> {
    for a in agents {
        match ev {
            TapeEvent::LteFile { .. } => {
                let bytes = ev.lte_bytes().map(|b| match String::from_utf8(b) {
                    Ok(s) => rewrite::shift_lte_text(&s, shift_s).into_bytes(),
                    Err(e) => e.into_bytes(),
                });
                a.lte(bytes.as_deref())?;
            }
            TapeEvent::Exec { argv, stdout, code } => a.exec(argv, stdout, *code)?,
            TapeEvent::I2cdetect { stdout, .. } => a.i2c(stdout.as_deref())?,
            TapeEvent::Dt { .. } => {
                // /proc/device-tree is a fixed path in both agents and cannot
                // be redirected in a container: psu_max_current is absent on
                // both sides of the rig (README "Limits").
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_holds_between_paired_ticks() {
        let mut g = Gate::new(60);
        assert!(g.is_open(0, false));
        g.on_tick(0, 1_000, 1_005); // A phase 0
        g.on_tick(1, 1_300, 1_305); // B phase 300
        assert!(g.is_open(1_400, false));
        assert!(g.is_open(1_939, false));
        assert!(!g.is_open(1_941, false)); // A due at 2000
        g.on_tick(0, 2_000, 2_004);
        assert!(!g.is_open(2_100, false)); // waiting for B
        g.on_tick(1, 2_300, 2_306);
        assert!(!g.is_open(2_310, false)); // guard after
        assert!(g.is_open(2_330, false));
        assert_eq!(g.pairs, vec![json!({"a": 2_000, "b": 2_300})]);
    }

    #[test]
    fn gate_lead_is_whoever_ticks_first_and_times_out() {
        let mut g = Gate::new(60);
        g.on_tick(0, 1_700, 1_700);
        g.on_tick(1, 1_200, 1_200);
        assert!(!g.is_open(2_150, false)); // B due at 2200
        assert!(!g.is_open(2_500, false));
        // B's tick at 2200 and A's at 2700 → pair.
        g.on_tick(1, 2_200, 2_205);
        g.on_tick(0, 2_700, 2_702);
        assert_eq!(g.pairs, vec![json!({"a": 2_700, "b": 2_200})]);
        // Nothing observed: the hold expires; a silent agent is not gated.
        assert!(!g.is_open(3_150, false));
        assert!(!g.is_open(3_150 + 1_400, false));
        assert!(g.is_open(4_750, false));
    }

    #[test]
    fn gate_keeps_predicting_through_a_cut() {
        let mut g = Gate::new(60);
        g.on_tick(0, 1_000, 1_000);
        g.on_tick(1, 1_300, 1_300);
        // 10 s later nothing is observable (cut), but A still ticks at …000
        // and B at …300: hold [A − 60, B + 100].
        assert!(g.is_open(10_900, true));
        assert!(!g.is_open(10_950, true));
        assert!(!g.is_open(11_350, true));
        assert!(!g.is_open(11_401, true));
        assert!(g.is_open(11_421, true));
        // Connected again but still silent → not gated.
        assert!(g.is_open(11_900, false));
    }

    #[test]
    fn gate_flips_when_the_gap_exceeds_800() {
        let mut g = Gate::new(60);
        g.on_tick(0, 1_000, 1_000);
        g.on_tick(1, 1_850, 1_850);
        // At 1940: A due 2000, B due 2850 → gap 850 → skip A's 2000 tick.
        assert!(g.is_open(1_945, false));
        assert_eq!(g.pairs, vec![json!({"skip": "a", "ts": 2_000})]);
        // Now B (2850) leads A (3000).
        assert!(g.is_open(2_700, false));
        assert!(!g.is_open(2_800, false));
    }

    #[test]
    fn gga_quality_parses() {
        assert_eq!(
            gga_quality("$GPGGA,1,2,N,3,E,1,08,0.9*00\r").as_deref(),
            Some("1")
        );
        assert_eq!(
            gga_quality("$GPGGA,083745.000,,,,,0,00,,,M,,M,,*75").as_deref(),
            Some("0")
        );
        assert_eq!(gga_quality("$GPRMC,1"), None);
    }
}
