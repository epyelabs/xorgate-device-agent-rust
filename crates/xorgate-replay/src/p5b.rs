//! Phase 5b rig inputs (WP4, `phase-5b-js-catchup.md` §8 step B): the OBD
//! peripheral. What the player adds so both agents run the REAL OBD helper
//! against the same ELM327 behaviour without any real Bluetooth:
//!
//! | Input | Rig value |
//! |---|---|
//! | `meta.rig.obd.scenario` | ONE ELM327-emulator 4.0.0 per agent (an ELM327 link is single-client): `python3 /rig/tools/obd-emulator.py <python> <scenario> <work>/obd-pty`, started before the agent, SIGTERMed with the run; `XORGATE_OBD_SERIAL` = its pty (the TS SERIAL seam, ts:src/hardware/obd.ts `buildObdHelperArgs`) |
//! | `XORGATE_PYTHON_DIR` | `<work>/python/`: the rig's `bno085_read.py` relay + the side's REAL helper (`obd_read.py`, `obd_scan.py`, `obd/`): the JS build's `python/` for a JS side, the Rust repo's vendored `python/` (`/rig/python`) for the Rust side once it carries the helper (byte-identical, I9), else the JS build's |
//! | `bluetoothctl` (PATH, OBD tapes only) | `tools/replay/shims/bluetoothctl`: prints `meta.rig.obd.bluetoothctl.{scan,devices,remove}` from `<state>/bt/`, logs to `<run>/<label>/bluetoothctl.log`, refuses pair/trust/connect (logged REFUSED) — never BlueZ |
//! | `meta.rig.obd.env` | extra agent env (the `XORGATE_OBD_*` seam a tape exercises) |
//! | the helper's stdout (gated runs) | `obd_read.py` = `tools/replay/obd-gate.py`, which runs the REAL helper (`obd_read_real.py`) and relays its lines only while the player's hold gate is open (`$RIG_OBD_GATE`, written by `play.rs` [`write_obd_gate`]) — helper edges reach both agents outside a tick pair, like every other line input |

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use xorgate_hal::tape::Tape;

/// The relay's barrier-wait log in `<run>/<label>/` (obd-gate.py `log_wait`).
pub const OBD_GATE_LOG: &str = "obd-gate.ndjson";

/// `meta.rig.obd`.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ObdPlan {
    /// The emulator scenario (`taos-2022`, `taos-2022-crank`,
    /// `taos-2022-badproto`, …); absent = no emulator (no serial seam).
    #[serde(default)]
    pub scenario: Option<String>,
    /// Extra agent env (`XORGATE_OBD_ADAPTER`, …).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub bluetoothctl: BtPlan,
}

/// `meta.rig.obd.bluetoothctl`: what the shim prints.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct BtPlan {
    /// Lines after `scan on` (`[NEW] Device <MAC> <name>`, `[CHG] …`).
    #[serde(default)]
    pub scan: Vec<String>,
    /// Lines for `devices` (`Device <MAC> <name>`).
    #[serde(default)]
    pub devices: Vec<String>,
    /// `remove <MAC>`: output + exit code.
    #[serde(default)]
    pub remove_out: Option<String>,
    #[serde(default)]
    pub remove_code: Option<i32>,
}

impl ObdPlan {
    pub fn of(tape: &Tape) -> Option<Self> {
        let v = tape.meta.rig.as_ref()?.get("obd")?;
        serde_json::from_value(v.clone()).ok()
    }
}

/// The agent env additions (`(key, value)`, later entries win).
pub type Env = Vec<(String, String)>;

/// The per-agent emulator, SIGTERMed (then killed) when dropped.
pub struct Emu(Child);

impl Drop for Emu {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.0.id() as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.0.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The python dir a side runs the helper from: `side_src` when it carries
/// `obd_read.py`, else `fallback` (the JS build's, byte-identical by I9).
pub fn helper_source(side_src: &Path, fallback: &Path) -> PathBuf {
    if side_src.join("obd_read.py").is_file() {
        side_src.to_path_buf()
    } else {
        fallback.to_path_buf()
    }
}

/// Prepare one agent's OBD inputs: its python dir, the shim's answers, the
/// emulator. Returns the emulator (kept alive by the agent) + env.
#[allow(clippy::too_many_arguments)] // the per-agent inputs, as play.rs has them
pub fn prepare_agent(
    plan: &ObdPlan,
    work: &Path,
    dir: &Path,
    state: &Path,
    fake_python: &Path,
    helper_src: &Path,
    rig_tools: &Path,
    gate: Option<&Path>,
) -> Result<(Option<Emu>, Env)> {
    let py = work.join("python");
    let _ = fs::remove_dir_all(&py);
    fs::create_dir_all(&py)?;
    symlink(
        fake_python.join("bno085_read.py"),
        py.join("bno085_read.py"),
    )?;
    for name in ["obd_read.py", "obd_scan.py", "obd"] {
        let src = helper_src.join(name);
        if !src.exists() {
            bail!("OBD tape: {} has no {name}", helper_src.display());
        }
        // The helper runs behind the hold-gate relay (`obd-gate.py`, the
        // orchestrator ruling 2026-10-23): `obd_read.py` = the relay,
        // `obd_read_real.py` = the side's REAL helper it runs.
        let dst = if name == "obd_read.py" && gate.is_some() {
            "obd_read_real.py"
        } else {
            name
        };
        symlink(&src, py.join(dst))?;
    }
    if gate.is_some() {
        fs::copy(rig_tools.join("obd-gate.py"), py.join("obd_read.py"))
            .context("install the OBD hold-gate relay")?;
    }
    let bt = state.join("bt");
    fs::create_dir_all(&bt)?;
    let lines = |v: &[String]| v.iter().map(|l| format!("{l}\n")).collect::<String>();
    fs::write(bt.join("scan.txt"), lines(&plan.bluetoothctl.scan))?;
    fs::write(bt.join("devices.txt"), lines(&plan.bluetoothctl.devices))?;
    fs::write(
        bt.join("remove.out"),
        plan.bluetoothctl
            .remove_out
            .clone()
            .unwrap_or_else(|| "Device has been removed\n".into()),
    )?;
    fs::write(
        bt.join("remove.code"),
        plan.bluetoothctl.remove_code.unwrap_or(0).to_string(),
    )?;
    let mut env: Env = vec![
        ("XORGATE_PYTHON_DIR".into(), py.display().to_string()),
        (
            "RIG_BT_LOG".into(),
            dir.join("bluetoothctl.log").display().to_string(),
        ),
        ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
    ];
    if let Some(g) = gate {
        env.push(("RIG_OBD_GATE".into(), g.display().to_string()));
        // The cross-side barrier (obd-gate.py `barrier`): one dir for both
        // agents, this side's counters reset (a fresh run).
        let side = work
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bar = g.with_file_name("obd-barrier");
        fs::create_dir_all(&bar)?;
        for e in fs::read_dir(&bar)?.flatten() {
            if e.file_name()
                .to_string_lossy()
                .starts_with(&format!("{side}."))
            {
                let _ = fs::remove_file(e.path());
            }
        }
        env.push(("RIG_OBD_BARRIER".into(), bar.display().to_string()));
        env.push(("RIG_OBD_SIDE".into(), side));
        env.push(("RIG_OBD_PEERS".into(), "a,b".into()));
        // The relay's rig-owned barrier-wait log (review-wp4 R4-4).
        let log = dir.join(OBD_GATE_LOG);
        let _ = fs::remove_file(&log);
        env.push(("RIG_OBD_WAITLOG".into(), log.display().to_string()));
    }
    env.extend(plan.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let mut emu = None;
    if let Some(sc) = &plan.scenario {
        let pty_file = work.join("obd-pty");
        let _ = fs::remove_file(&pty_file);
        let log = fs::File::create(dir.join("obd-emulator.log"))?;
        let child = Command::new("python3")
            .arg(rig_tools.join("obd-emulator.py"))
            .arg(helper_src)
            .arg(sc)
            .arg(&pty_file)
            .env("TMPDIR", work)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("spawn the ELM327 emulator")?;
        let e = Emu(child);
        let deadline = Instant::now() + Duration::from_secs(30);
        let pty = loop {
            if let Ok(p) = fs::read_to_string(&pty_file) {
                if p.starts_with("/dev/") {
                    break p.trim().to_string();
                }
            }
            if Instant::now() > deadline {
                bail!(
                    "ELM327 emulator ({sc}) printed no pty in 30 s: see {}",
                    dir.join("obd-emulator.log").display()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        env.push(("XORGATE_OBD_SERIAL".into(), pty));
        emu = Some(e);
    }
    Ok((emu, env))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obd_plan_parses_the_rig_meta() {
        let p: ObdPlan = serde_json::from_value(serde_json::json!({
            "scenario": "taos-2022",
            "env": {"XORGATE_OBD_POLL_HZ": "2"},
            "bluetoothctl": {"scan": ["[NEW] Device 00:1D:A5:68:98:8B OBDII"], "removeCode": 1},
        }))
        .unwrap();
        assert_eq!(p.scenario.as_deref(), Some("taos-2022"));
        assert_eq!(p.env["XORGATE_OBD_POLL_HZ"], "2");
        assert_eq!(p.bluetoothctl.remove_code, Some(1));
        let d = std::env::temp_dir().join(format!("p5b-src-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        assert_eq!(
            helper_source(&d, Path::new("/js/python")),
            Path::new("/js/python")
        );
        fs::write(d.join("obd_read.py"), "").unwrap();
        assert_eq!(helper_source(&d, Path::new("/js/python")), d);
        let _ = fs::remove_dir_all(&d);
    }
}
