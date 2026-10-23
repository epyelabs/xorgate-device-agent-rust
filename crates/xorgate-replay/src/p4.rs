//! Phase 4 rig inputs (WP6, `phase-4-operations.md` §8): what the player adds
//! so both agents exercise the LED, SMS, clock stepper and camera mount on the
//! same inputs.
//!
//! | Input | Rig value |
//! |---|---|
//! | Raspberry Pi identity + boot config + device tree | `meta.rig.pi` → each agent runs in its OWN private mount namespace (`unshare -m`, `tools/replay/pi-ns.sh`): `/boot/firmware` = `<work>/pi/boot` (its own `config.txt`), `/sys/firmware/devicetree/base` (= `/proc/device-tree`) = `<work>/pi/dt` (`model`, camera `rotation` bytes). `meta.rig.pi.dtWrites` rewrites DT files at tape time (the simulated reboot of `p4-cam-mount`) |
//! | `pinctrl`, `gpioinfo`, `dtoverlay` (PATH) | shims, only with `meta.rig.pi`: `pinctrl` logs to `<run>/<label>/led.log` (the `led` diff stream); `gpioinfo`/`dtoverlay` answer from `exec` records |
//! | `XORGATE_LED_WRITER` | `pinctrl` for both (the JS agent always drives pinctrl; Q6) |
//! | SMS port | `meta.rig.modem` (FakeModem semantics of `test/sms-modem.test.ts` @89b5c64) or the tape's `at_tx`/`at_rx` transcript → a pty per agent, `XORGATE_SMS_PORT=<work>/sms-tty`; every command the agent sends → `<run>/<label>/at.ndjson` |
//! | `timedatectl` NTPSynchronized | the tape's `ntp_probe` timeline (`<state>/ntp`, default `yes`) |
//! | `sudo` answers | the tape's `sudo` records (`<state>/sudo/<helper>.{code,out,err}`, default exit 0); `xorgate-camrotate` EXECUTES the real vendored helper (`RIG_CAMROTATE`) inside the agent's namespace |

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use xorgate_hal::tape::{hex_decode, wall_now_ms, Tape, TapeEvent};

/// `/proc/device-tree/model` on the bench CM5 (Argus carrier).
pub const PI_MODEL: &str = "Raspberry Pi Compute Module 5 Rev 1.0";

/// One `meta.rig.pi.dtWrites` entry: at tape time `t`, write `hex` to
/// `<dt>/<rel>` (`hex: null` removes it).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DtWrite {
    pub t: u64,
    pub rel: String,
    #[serde(default)]
    pub hex: Option<String>,
}

/// `meta.rig.pi`.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PiPlan {
    #[serde(default)]
    pub model: Option<String>,
    /// `/boot/firmware/config.txt` at start (absent = no config.txt).
    #[serde(default)]
    pub config_txt: Option<String>,
    /// Device-tree files at start, `rel → hex` (`model` is added).
    #[serde(default)]
    pub dt: BTreeMap<String, String>,
    #[serde(default)]
    pub dt_writes: Vec<DtWrite>,
}

/// One SIM slot (`test/sms-modem.test.ts` `Slot`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ModemSlot {
    pub stat: u32,
    pub length: u32,
    pub pdu: String,
}

/// One storage (`{ total, slots }`).
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct ModemStore {
    pub total: u32,
    #[serde(default)]
    pub slots: BTreeMap<u32, ModemSlot>,
}

/// `meta.rig.modem` — the `FakeOpts` of the TS test's FakeModem.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModemSpec {
    #[serde(default)]
    pub cmgf: u32,
    #[serde(default = "default_mem1")]
    pub mem1: String,
    #[serde(default)]
    pub stores: BTreeMap<String, ModemStore>,
    /// Answer `AT+CPMS?` with this line (no SIM).
    #[serde(default)]
    pub cpms_error: Option<String>,
    /// Unsolicited lines injected before every response.
    #[serde(default)]
    pub urcs: Vec<String>,
    /// Phase 5b (`p5b-sms-clear` only): answer `AT+CMGD=0,4` (the TS
    /// FakeModem of `test/sms-modem.test.ts:106-114` @09da38d: record the
    /// store, empty it, `OK`). Without it a CMGD is answered `ERROR` and
    /// flagged `forbidden` (the diff FAILS the run): every other tape keeps
    /// the read-only contract.
    #[serde(default)]
    pub allow_clear: bool,
    /// `cmgdError`: answer `AT+CMGD=0,4` on this store with this final.
    #[serde(default)]
    pub cmgd_error: BTreeMap<String, String>,
    /// Rig: `AT+CMGD=0,4` on these stores gets no answer at all (the TS
    /// FakeModem's `hang`, as a per-store delete timeout).
    #[serde(default)]
    pub cmgd_hang: Vec<String>,
    /// Rig: answer a command starting with the key only after this many ms
    /// (a slow delete / a slow sweep, so a second request lands mid-op).
    #[serde(default)]
    pub delay_ms: BTreeMap<String, u64>,
    /// Rig: modem state changes at tape time (a refill, a SIM pulled).
    #[serde(default)]
    pub changes: Vec<ModemChange>,
}

/// One `meta.rig.modem.changes` entry: from tape time `t` (ms after the
/// agents' start), the listed fields replace the spec's.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModemChange {
    pub t: u64,
    #[serde(default)]
    pub stores: Option<BTreeMap<String, ModemStore>>,
    /// `Some("")` clears a previous `cpmsError`.
    #[serde(default)]
    pub cpms_error: Option<String>,
    #[serde(default)]
    pub cmgd_hang: Option<Vec<String>>,
    #[serde(default)]
    pub cmgd_error: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub delay_ms: Option<BTreeMap<String, u64>>,
}

fn default_mem1() -> String {
    "SR".to_string()
}

/// How the fake modem answers.
#[derive(Debug, Clone, PartialEq)]
pub enum ModemMode {
    /// Stateful FakeModem semantics.
    Fake(ModemSpec),
    /// The tape's recorded transcript: each `at_tx` line answered with the
    /// `at_rx` bytes recorded after it (matched in order, by text).
    Transcript(Vec<(String, Vec<u8>)>),
}

/// Everything Phase 4 the player needs from a tape.
#[derive(Debug, Clone, Default)]
pub struct P4Plan {
    pub pi: Option<PiPlan>,
    pub modem: Option<ModemMode>,
}

impl P4Plan {
    pub fn of(tape: &Tape) -> Self {
        let rig = tape.meta.rig.as_ref();
        let pi = rig
            .and_then(|r| r.get("pi"))
            .and_then(|v| serde_json::from_value::<PiPlan>(v.clone()).ok());
        let modem = rig
            .and_then(|r| r.get("modem"))
            .and_then(|v| serde_json::from_value::<ModemSpec>(v.clone()).ok())
            .map(ModemMode::Fake)
            .or_else(|| transcript_of(tape).map(ModemMode::Transcript));
        Self { pi, modem }
    }

    /// The shims only a Pi-namespaced run puts on PATH.
    pub fn skip_shim(&self, name: &str) -> bool {
        self.pi.is_none() && matches!(name, "pinctrl" | "gpioinfo" | "dtoverlay")
    }
}

/// The recorded AT transcript (`at_tx` + the `at_rx` chunks up to the next
/// `at_tx`), or `None` when the tape has none.
fn transcript_of(tape: &Tape) -> Option<Vec<(String, Vec<u8>)>> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for r in &tape.records {
        match &r.ev {
            TapeEvent::AtTx { line, .. } => out.push((line.clone(), Vec::new())),
            TapeEvent::AtRx { .. } => {
                if let (Some(last), Some(b)) = (out.last_mut(), r.ev.at_rx_bytes()) {
                    last.1.extend_from_slice(&b);
                }
            }
            _ => {}
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Per-agent Phase 4 state the player keeps alive for the run.
pub struct AgentP4 {
    /// `<work>/pi` (namespace sources), when the tape is Pi-namespaced.
    pub pi_dir: Option<PathBuf>,
    /// The SMS pty slave the player holds open (so the pty never hangs up
    /// between the agent's opens), when the tape has a modem.
    _sms_slave: Option<OwnedFd>,
}

/// Prepare one agent: namespace dirs, SMS pty + fake modem thread, env.
pub fn prepare_agent(
    plan: &P4Plan,
    work: &Path,
    dir: &Path,
    camrotate: Option<&Path>,
) -> Result<(AgentP4, Vec<(String, String)>)> {
    let mut env: Vec<(String, String)> = vec![
        // Q6: both agents hit the same pinctrl shim (the JS agent ignores it).
        ("XORGATE_LED_WRITER".into(), "pinctrl".into()),
        (
            "RIG_LED_LOG".into(),
            dir.join("led.log").display().to_string(),
        ),
    ];
    let mut pi_dir = None;
    if let Some(pi) = &plan.pi {
        let root = work.join("pi");
        let boot = root.join("boot");
        let dt = root.join("dt");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&boot)?;
        fs::create_dir_all(&dt)?;
        if let Some(text) = &pi.config_txt {
            fs::write(boot.join("config.txt"), text)?;
        }
        let model = pi.model.clone().unwrap_or_else(|| PI_MODEL.to_string());
        let mut m = model.into_bytes();
        m.push(0);
        fs::write(dt.join("model"), m)?;
        for (rel, hex) in &pi.dt {
            write_dt(&dt, rel, Some(hex))?;
        }
        if let Some(h) = camrotate {
            env.push(("RIG_CAMROTATE".into(), h.display().to_string()));
        }
        pi_dir = Some(root);
    }
    let mut sms_slave = None;
    if let Some(mode) = &plan.modem {
        let (master, slave, path) = open_modem_pty()?;
        let link = work.join("sms-tty");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&path, &link)?;
        env.push(("XORGATE_SMS_PORT".into(), link.display().to_string()));
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("at.ndjson"))?;
        spawn_fake_modem(master, mode.clone(), log);
        sms_slave = Some(slave);
    }
    Ok((
        AgentP4 {
            pi_dir,
            _sms_slave: sms_slave,
        },
        env,
    ))
}

/// Write (or remove) one device-tree file under `dt`.
pub fn write_dt(dt: &Path, rel: &str, hex: Option<&String>) -> Result<()> {
    if rel.contains("..") || rel.starts_with('/') {
        anyhow::bail!("bad dt path {rel:?}");
    }
    let path = dt.join(rel);
    match hex {
        Some(h) => {
            let bytes = hex_decode(h).with_context(|| format!("dt {rel}: bad hex"))?;
            if let Some(p) = path.parent() {
                fs::create_dir_all(p)?;
            }
            let tmp = path.with_extension("rig-tmp");
            fs::write(&tmp, bytes)?;
            fs::rename(&tmp, &path)?;
        }
        None => {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

/// The `unshare` wrapper for a Pi-namespaced agent: `(program, args)`.
pub fn wrap_command(
    pi_dir: Option<&Path>,
    ns_script: &Path,
    program: &str,
    args: &[String],
) -> (String, Vec<String>) {
    match pi_dir {
        None => (program.to_string(), args.to_vec()),
        Some(dir) => {
            let mut a = vec![
                "-m".to_string(),
                "--propagation".to_string(),
                "private".to_string(),
                "--".to_string(),
                ns_script.display().to_string(),
                dir.display().to_string(),
                program.to_string(),
            ];
            a.extend(args.iter().cloned());
            ("unshare".to_string(), a)
        }
    }
}

/// `meta.rig.pi.dtWrites` at their tape times (relative to `t0`).
pub async fn run_dt_writes(
    plan: P4Plan,
    dts: Vec<PathBuf>,
    t0: tokio::time::Instant,
    log: impl Fn(String) + Send + 'static,
) {
    let Some(pi) = plan.pi else { return };
    let mut writes = pi.dt_writes.clone();
    writes.sort_by_key(|w| w.t);
    for w in writes {
        tokio::time::sleep_until(t0 + Duration::from_millis(w.t)).await;
        for dt in &dts {
            if let Err(e) = write_dt(dt, &w.rel, w.hex.as_ref()) {
                log(format!("dt write {} failed: {e:#}", w.rel));
            }
        }
        log(format!(
            "dt write t={} {} = {}",
            w.t,
            w.rel,
            w.hex.as_deref().unwrap_or("(removed)")
        ));
    }
}

/// A blocking-master pty for the fake modem (the NMEA pty's master is
/// non-blocking; this one is read by a thread).
fn open_modem_pty() -> Result<(File, OwnedFd, PathBuf)> {
    use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
    let pty = nix::pty::openpty(None, None).context("openpty")?;
    for fd in [&pty.master, &pty.slave] {
        nix::fcntl::fcntl(
            fd,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )?;
    }
    let mut t = tcgetattr(&pty.slave).context("tcgetattr")?;
    cfmakeraw(&mut t);
    tcsetattr(&pty.slave, SetArg::TCSANOW, &t).context("tcsetattr")?;
    let path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
        .context("pty slave path")?;
    Ok((File::from(pty.master), pty.slave, path))
}

/// The scripted SIM7600 (`class FakeModem`, `test/sms-modem.test.ts:46-111`
/// @89b5c64), answering whole lines.
#[derive(Debug, Clone)]
pub struct FakeModem {
    spec: ModemSpec,
    pub cmgf: u32,
    pub mem1: String,
    /// The store selected at each `AT+CMGD` (the TS FakeModem's `deletes`).
    pub deletes: Vec<String>,
    /// How many `changes` are applied.
    applied: usize,
}

impl FakeModem {
    pub fn new(spec: ModemSpec) -> Self {
        Self {
            cmgf: spec.cmgf,
            mem1: spec.mem1.clone(),
            spec,
            deletes: Vec::new(),
            applied: 0,
        }
    }

    /// Apply every `changes` entry due at `elapsed_ms` (in order, once).
    pub fn apply_due(&mut self, elapsed_ms: u64) {
        while let Some(c) = self.spec.changes.get(self.applied).cloned() {
            if c.t > elapsed_ms {
                break;
            }
            self.applied += 1;
            if let Some(st) = c.stores {
                self.spec.stores = st;
            }
            if let Some(e) = c.cpms_error {
                self.spec.cpms_error = (!e.is_empty()).then_some(e);
            }
            if let Some(h) = c.cmgd_hang {
                self.spec.cmgd_hang = h;
            }
            if let Some(e) = c.cmgd_error {
                self.spec.cmgd_error = e;
            }
            if let Some(d) = c.delay_ms {
                self.spec.delay_ms = d;
            }
        }
    }

    /// The answer delay for `cmd` (the longest matching prefix wins).
    pub fn delay_for(&self, cmd: &str) -> u64 {
        self.spec
            .delay_ms
            .iter()
            .filter(|(k, _)| cmd.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, v)| *v)
            .unwrap_or(0)
    }

    /// `{SM: used, ME: used}` for the at.ndjson state (a clear's effect).
    pub fn used_map(&self) -> Value {
        Value::Object(
            self.spec
                .stores
                .iter()
                .map(|(k, v)| (k.clone(), json!(v.slots.len())))
                .collect(),
        )
    }

    pub fn allow_clear(&self) -> bool {
        self.spec.allow_clear
    }

    fn used(&self, mem: &str) -> usize {
        self.spec
            .stores
            .get(mem)
            .map(|s| s.slots.len())
            .unwrap_or(0)
    }

    fn total(&self, mem: &str) -> u32 {
        if mem == "SR" {
            0
        } else {
            self.spec.stores.get(mem).map(|s| s.total).unwrap_or(0)
        }
    }

    fn triple(&self, mem: &str) -> String {
        format!("\"{mem}\",{},{}", self.used(mem), self.total(mem))
    }

    /// The lines answering `cmd` (no trailing `\r`). `Err` = the TS test would
    /// have thrown (CMGRO in text mode): answered `ERROR`, flagged by the diff.
    pub fn answer(&mut self, cmd: &str) -> (Vec<String>, Option<String>) {
        if cmd == CMGD_DELETE_ALL
            && self.spec.allow_clear
            && self.spec.cmgd_hang.contains(&self.mem1)
        {
            // `hang`: no answer at all (the agent's per-call delete timeout).
            self.deletes.push(self.mem1.clone());
            return (Vec::new(), None);
        }
        let mut out: Vec<String> = self.spec.urcs.clone();
        let mut fault = None;
        if cmd == CMGD_DELETE_ALL && self.spec.allow_clear {
            self.deletes.push(self.mem1.clone());
            match self.spec.cmgd_error.get(&self.mem1) {
                Some(e) => out.push(e.clone()),
                None => {
                    if let Some(st) = self.spec.stores.get_mut(&self.mem1) {
                        st.slots.clear();
                    }
                    out.push("OK".to_string());
                }
            }
        } else if cmd == "ATE0" {
            out.extend(["ATE0".to_string(), "OK".to_string()]);
        } else if cmd == "AT+CMGF?" {
            out.extend([format!("+CMGF: {}", self.cmgf), "OK".to_string()]);
        } else if let Some(n) = cmd
            .strip_prefix("AT+CMGF=")
            .filter(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
        {
            self.cmgf = n.parse().unwrap_or(0);
            out.push("OK".to_string());
        } else if cmd == "AT+CPMS?" {
            match &self.spec.cpms_error {
                Some(e) => out.push(e.clone()),
                None => out.extend([
                    format!(
                        "+CPMS: {},{},{}",
                        self.triple(&self.mem1),
                        self.triple("ME"),
                        self.triple("ME")
                    ),
                    "OK".to_string(),
                ]),
            }
        } else if let Some(mem) = cmd
            .strip_prefix("AT+CPMS=\"")
            .and_then(|r| r.strip_suffix('"'))
            .filter(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            self.mem1 = mem.to_string();
            out.extend([
                format!(
                    "+CPMS: {},{},23,23,23,23",
                    self.used(&self.mem1),
                    self.total(&self.mem1)
                ),
                "OK".to_string(),
            ]);
        } else if let Some(i) = cmd
            .strip_prefix("AT+CMGRO=")
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            if self.cmgf != 0 {
                fault = Some("CMGRO sent in text mode".to_string());
                out.push("ERROR".to_string());
            } else {
                let slot = i.parse::<u32>().ok().and_then(|i| {
                    self.spec
                        .stores
                        .get(&self.mem1)
                        .and_then(|s| s.slots.get(&i))
                        .cloned()
                });
                match slot {
                    Some(s) => out.extend([
                        format!("+CMGRO: {},\"\",{}", s.stat, s.length),
                        s.pdu,
                        "OK".to_string(),
                    ]),
                    None => out.push("ERROR".to_string()),
                }
            }
        } else {
            out.push("ERROR".to_string());
        }
        (out, fault)
    }
}

/// The SIM7600's own framing: every line `\r\n<line>\r\n`.
fn frame(lines: &[String]) -> Vec<u8> {
    let mut b = Vec::new();
    for l in lines {
        b.extend_from_slice(b"\r\n");
        b.extend_from_slice(l.as_bytes());
        b.extend_from_slice(b"\r\n");
    }
    b
}

/// The one delete the clear may send (`xorgate_hal::at_clear`, I7;
/// ts:src/hardware/sms-modem.ts:532 @09da38d).
pub const CMGD_DELETE_ALL: &str = "AT+CMGD=0,4";

/// The commands the read-only contract forbids (`phase-4-operations.md` §1.3,
/// E5): any write/list/delete/send, and a 2-arg `CPMS`. Phase 5b: on a tape
/// with `allowClear`, exactly `AT+CMGD=0,4` is permitted (§6 of the phase
/// file); any other CMGD form stays forbidden there too.
pub fn forbidden_at(cmd: &str, allow_clear: bool) -> bool {
    if allow_clear && cmd == CMGD_DELETE_ALL {
        return false;
    }
    let c = cmd.to_ascii_uppercase();
    c.starts_with("AT+CMGR=")
        || c.starts_with("AT+CMGL")
        || c.starts_with("AT+CMGD")
        || c.starts_with("AT+CMGW")
        || c.starts_with("AT+CMSS")
        || c.starts_with("AT+CMGS")
        || (c.starts_with("AT+CPMS=") && c.contains(','))
}

fn spawn_fake_modem(mut master: File, mode: ModemMode, mut log: File) {
    let t0 = std::time::Instant::now();
    std::thread::spawn(move || {
        let mut fake = match &mode {
            ModemMode::Fake(s) => Some(FakeModem::new(s.clone())),
            ModemMode::Transcript(_) => None,
        };
        let mut transcript = match mode {
            ModemMode::Transcript(t) => t,
            ModemMode::Fake(_) => Vec::new(),
        };
        let mut pending: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = match master.read(&mut buf) {
                Ok(0) => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    // EIO while no slave is open (the player holds one, so
                    // this is the end of the run).
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            pending.extend_from_slice(&buf[..n]);
            while let Some(pos) = pending.iter().position(|b| *b == b'\r' || *b == b'\n') {
                let raw: Vec<u8> = pending.drain(..=pos).collect();
                let cmd = String::from_utf8_lossy(&raw[..raw.len() - 1])
                    .trim()
                    .to_string();
                if cmd.is_empty() {
                    continue;
                }
                let mut delay = 0;
                let mut allow_clear = false;
                let (reply, fault, state) = match fake.as_mut() {
                    Some(f) => {
                        f.apply_due(t0.elapsed().as_millis() as u64);
                        delay = f.delay_for(&cmd);
                        allow_clear = f.allow_clear();
                        let (lines, fault) = f.answer(&cmd);
                        let mut st = json!({"cmgf": f.cmgf, "mem1": f.mem1});
                        if allow_clear {
                            // Phase 5b: the clear's effect + the delete order.
                            st["used"] = f.used_map();
                            st["deletes"] = json!(f.deletes);
                        }
                        (frame(&lines), fault, st)
                    }
                    None => {
                        let hit = transcript.iter().position(|(c, _)| *c == cmd);
                        let reply = match hit {
                            Some(i) => transcript.remove(i).1,
                            None => frame(&["ERROR".to_string()]),
                        };
                        (reply, None, Value::Null)
                    }
                };
                let mut rec = json!({"w": wall_now_ms(), "cmd": cmd, "state": state});
                if forbidden_at(&cmd, allow_clear) {
                    rec["forbidden"] = json!(true);
                }
                if let Some(f) = fault {
                    rec["fault"] = json!(f);
                }
                if reply.is_empty() {
                    rec["hang"] = json!(true);
                }
                let _ = writeln!(log, "{rec}");
                if delay > 0 {
                    std::thread::sleep(Duration::from_millis(delay));
                }
                let _ = master.write_all(&reply);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ModemSpec {
        let mut sm = ModemStore {
            total: 40,
            slots: BTreeMap::new(),
        };
        sm.slots.insert(
            0,
            ModemSlot {
                stat: 0,
                length: 24,
                pdu: "0791".into(),
            },
        );
        let me = ModemStore {
            total: 23,
            slots: BTreeMap::new(),
        };
        ModemSpec {
            cmgf: 0,
            mem1: "SR".into(),
            stores: [("SM".to_string(), sm), ("ME".to_string(), me)]
                .into_iter()
                .collect(),
            cpms_error: None,
            urcs: vec![],
            allow_clear: false,
            cmgd_error: BTreeMap::new(),
            cmgd_hang: vec![],
            delay_ms: BTreeMap::new(),
            changes: vec![],
        }
    }

    /// Phase 5b: `AT+CMGD=0,4` only with `allowClear` (else ERROR +
    /// forbidden), the TS FakeModem's delete (ts test/sms-modem.test.ts:106-114
    /// @09da38d), `cmgdError`, the rig's per-store hang, and `changes`.
    #[test]
    fn fake_modem_clear_is_gated_by_allow_clear() {
        let mut m = FakeModem::new(spec());
        m.answer(r#"AT+CPMS="SM""#);
        assert_eq!(m.answer("AT+CMGD=0,4").0, ["ERROR"]);
        assert!(forbidden_at("AT+CMGD=0,4", false));
        assert_eq!(m.used_map()["SM"], 1, "nothing deleted without allowClear");

        let mut sp = spec();
        sp.allow_clear = true;
        sp.cmgd_error.insert("ME".into(), "+CMS ERROR: 321".into());
        let mut m = FakeModem::new(sp);
        assert!(!forbidden_at("AT+CMGD=0,4", true));
        assert!(
            forbidden_at("AT+CMGD=1,4", true),
            "only the delete-all form"
        );
        assert!(forbidden_at("AT+CMGD=0,1", true));
        m.answer(r#"AT+CPMS="SM""#);
        assert_eq!(m.answer("AT+CMGD=0,4").0, ["OK"]);
        assert_eq!(m.used_map()["SM"], 0);
        m.answer(r#"AT+CPMS="ME""#);
        assert_eq!(m.answer("AT+CMGD=0,4").0, ["+CMS ERROR: 321"]);
        assert_eq!(m.deletes, ["SM", "ME"]);

        let mut sp = spec();
        sp.allow_clear = true;
        sp.cmgd_hang = vec!["SM".into()];
        sp.delay_ms.insert("AT+CMGD".into(), 3000);
        sp.changes = vec![ModemChange {
            t: 10_000,
            stores: Some(BTreeMap::new()),
            cpms_error: Some("+CME ERROR: 10".into()),
            cmgd_hang: Some(vec![]),
            cmgd_error: None,
            delay_ms: None,
        }];
        let mut m = FakeModem::new(sp);
        assert_eq!(m.delay_for("AT+CMGD=0,4"), 3000);
        assert_eq!(m.delay_for("AT+CMGRO=1"), 0);
        m.answer(r#"AT+CPMS="SM""#);
        assert!(m.answer("AT+CMGD=0,4").0.is_empty(), "hang: no answer");
        m.apply_due(9_999);
        assert_eq!(m.answer("AT+CPMS?").0.len(), 2);
        m.apply_due(10_000);
        assert_eq!(m.answer("AT+CPMS?").0, ["+CME ERROR: 10"]);
        assert_eq!(m.answer("AT+CMGD=0,4").0, ["OK"], "hang lifted");
    }

    /// The FakeModem answers of `test/sms-modem.test.ts:76-110`.
    #[test]
    fn fake_modem_answers_like_the_ts_fixture() {
        let mut m = FakeModem::new(spec());
        assert_eq!(m.answer("ATE0").0, ["ATE0", "OK"]);
        assert_eq!(m.answer("AT+CMGF?").0, ["+CMGF: 0", "OK"]);
        assert_eq!(
            m.answer("AT+CPMS?").0,
            [r#"+CPMS: "SR",0,0,"ME",0,23,"ME",0,23"#, "OK"]
        );
        assert_eq!(
            m.answer(r#"AT+CPMS="SM""#).0,
            ["+CPMS: 1,40,23,23,23,23", "OK"]
        );
        assert_eq!(m.mem1, "SM");
        assert_eq!(
            m.answer("AT+CMGRO=0").0,
            [r#"+CMGRO: 0,"",24"#, "0791", "OK"]
        );
        assert_eq!(m.answer("AT+CMGRO=5").0, ["ERROR"]);
        assert_eq!(m.answer("AT+CMGF=1").0, ["OK"]);
        let (l, fault) = m.answer("AT+CMGRO=0");
        assert_eq!(l, ["ERROR"]);
        assert!(fault.is_some(), "CMGRO in text mode is a rig fault");
        assert_eq!(m.answer("AT+CMGL=4").0, ["ERROR"]);
        let mut e = spec();
        e.cpms_error = Some("+CME ERROR: SIM not inserted".into());
        e.urcs = vec!["RDY".into()];
        assert_eq!(
            FakeModem::new(e).answer("AT+CPMS?").0,
            ["RDY", "+CME ERROR: SIM not inserted"]
        );
        assert_eq!(frame(&["OK".to_string()]), b"\r\nOK\r\n");
    }

    #[test]
    fn forbidden_commands() {
        for c in [
            "AT+CMGR=1",
            "AT+CMGL=4",
            "AT+CMGD=1,4",
            "AT+CMGW",
            "AT+CMSS=1",
            "AT+CMGS=20",
            r#"AT+CPMS="SM","SM""#,
        ] {
            assert!(forbidden_at(c, false), "{c}");
        }
        for c in [
            "AT+CMGRO=1",
            r#"AT+CPMS="SM""#,
            "AT+CPMS?",
            "AT+CMGF=0",
            "ATE0",
        ] {
            assert!(!forbidden_at(c, false), "{c}");
        }
    }

    #[test]
    fn wrap_command_adds_the_namespace_only_for_pi_tapes() {
        let (p, a) = wrap_command(None, Path::new("/ns.sh"), "node", &["w.js".into()]);
        assert_eq!((p.as_str(), a), ("node", vec!["w.js".to_string()]));
        let (p, a) = wrap_command(
            Some(Path::new("/w/a/pi")),
            Path::new("/rig/tools/pi-ns.sh"),
            "/bin/xorgate-rs",
            &["run".into()],
        );
        assert_eq!(p, "unshare");
        assert_eq!(
            a,
            [
                "-m",
                "--propagation",
                "private",
                "--",
                "/rig/tools/pi-ns.sh",
                "/w/a/pi",
                "/bin/xorgate-rs",
                "run"
            ]
        );
    }

    #[test]
    fn dt_writes_and_plan_parse() {
        let d = std::env::temp_dir().join(format!("p4-dt-{}", std::process::id()));
        let rel = "axi/pcie@1000120000/rp1/i2c@88000/imx290@1a/rotation".to_string();
        write_dt(&d, &rel, Some(&"000000b4".to_string())).unwrap();
        assert_eq!(fs::read(d.join(&rel)).unwrap(), [0, 0, 0, 0xb4]);
        write_dt(&d, &rel, None).unwrap();
        assert!(!d.join(&rel).exists());
        assert!(write_dt(&d, "../x", Some(&"00".to_string())).is_err());
        let _ = fs::remove_dir_all(&d);

        let mut tape = Tape {
            meta: xorgate_hal::tape::TapeMeta::new("d", "b", 0),
            records: vec![],
            truncated: false,
        };
        assert!(P4Plan::of(&tape).pi.is_none());
        assert!(P4Plan::of(&tape).skip_shim("pinctrl"));
        assert!(!P4Plan::of(&tape).skip_shim("vcgencmd"));
        tape.meta.rig = Some(json!({
            "pi": {"configTxt": "dtoverlay=imx290,cam0\n", "dt": {(rel.clone()): "00000000"},
                   "dtWrites": [{"t": 5000, "rel": rel, "hex": "000000b4"}]},
            "modem": {"stores": {"SM": {"total": 40, "slots": {"0": {"stat": 0, "length": 24, "pdu": "0791"}}}}}
        }));
        let p = P4Plan::of(&tape);
        let pi = p.pi.as_ref().unwrap();
        assert_eq!(pi.config_txt.as_deref(), Some("dtoverlay=imx290,cam0\n"));
        assert_eq!(pi.dt_writes[0].t, 5000);
        assert!(!p.skip_shim("pinctrl"));
        match p.modem.unwrap() {
            ModemMode::Fake(s) => {
                assert_eq!(s.mem1, "SR");
                assert_eq!(s.stores["SM"].slots[&0].length, 24);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn transcript_mode_from_at_records() {
        let mut tape = Tape {
            meta: xorgate_hal::tape::TapeMeta::new("d", "b", 0),
            records: vec![],
            truncated: false,
        };
        let rec = |ev| xorgate_hal::tape::TapeRecord { t: 0, w: 0, ev };
        tape.records = vec![
            rec(TapeEvent::AtTx {
                port: "p".into(),
                line: "ATE0".into(),
            }),
            rec(TapeEvent::at_rx("p", b"\r\nATE0\r\n")),
            rec(TapeEvent::at_rx("p", b"\r\nOK\r\n")),
            rec(TapeEvent::AtTx {
                port: "p".into(),
                line: "AT+CMGF?".into(),
            }),
            rec(TapeEvent::at_rx("p", b"\r\n+CMGF: 0\r\n\r\nOK\r\n")),
        ];
        match P4Plan::of(&tape).modem.unwrap() {
            ModemMode::Transcript(t) => {
                assert_eq!(t.len(), 2);
                assert_eq!(
                    t[0],
                    ("ATE0".to_string(), b"\r\nATE0\r\n\r\nOK\r\n".to_vec())
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
