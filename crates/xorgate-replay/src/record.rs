//! `xorgate-replay record-shared` — a SHARED-inputs-only bench recorder.
//!
//! Records what both agents may read concurrently (decision O6 "shared"
//! profile): the modem daemon's LTE telemetry file, `vcgencmd get_throttled` /
//! `pmic_read_adc`, the device-tree PSU limit, plus the 10 s clock samples. It
//! never opens the GNSS tty, the IMU, or the agent's MQTT identity, so it can
//! run next to the JS prod agent (no exclusive window). Its tape has no NMEA,
//! IMU or MQTT input: `synth <scenario> --base <tape>` splices those in
//! (labelled synthetic) to make a rig tape whose LTE/power inputs are real.
//!
//! Run it on the bench from a COPIED binary (never build on the bench):
//! `scp tools/replay/.cache/bin/xorgate-replay pi@bench:/tmp/ && ssh pi@bench
//! /tmp/xorgate-replay record-shared --out /tmp/shared.ndjson.gz --duration-s 300`.
//! Same cadence as the agents' 1 Hz reads; same de-duplication as the agent's
//! tape writer (only changes are written). The tape holds the SIM identity
//! (ICCID/MSISDN in the LTE file): keep it local, never commit it (O8).

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use xorgate_hal::tape::{wall_now_ms, ConnState, FileTape, TapeEvent, TapeMeta};

#[derive(Debug, Clone, clap::Args)]
pub struct RecordArgs {
    #[arg(long)]
    pub out: PathBuf,
    #[arg(long, default_value_t = 300)]
    pub duration_s: u64,
    #[arg(long, default_value = "/run/sim7600-lte/telemetry.json")]
    pub lte_file: PathBuf,
    #[arg(long, default_value = "/proc/device-tree/chosen/power/max_current")]
    pub dt_path: PathBuf,
    /// Label for meta.deviceId (the rig substitutes its own ids anyway).
    #[arg(long, default_value = "bench-shared")]
    pub device_id: String,
}

fn vcgencmd(arg: &str) -> (String, Option<i32>) {
    match Command::new("vcgencmd").arg(arg).output() {
        Ok(o) => (String::from_utf8_lossy(&o.stdout).to_string(), o.status.code()),
        Err(_) => (String::new(), None),
    }
}

pub fn run(args: RecordArgs) -> Result<()> {
    let mut meta = TapeMeta::new(&args.device_id, concat!("xorgate-replay record-shared ", env!("CARGO_PKG_VERSION")), wall_now_ms());
    meta.source = Some("record-shared".into());
    meta.env = TapeMeta::redacted_env(&|k| std::env::var(k).ok());
    let tape = FileTape::create(&args.out, &meta).with_context(|| format!("creating {}", args.out.display()))?;
    tape.record(TapeEvent::Conn { state: ConnState::Connected });
    tape.dt(std::fs::read(&args.dt_path).ok().as_deref());
    let start = Instant::now();
    let mut next = start;
    let mut last_clock = start;
    let thr = vec!["vcgencmd".to_string(), "get_throttled".to_string()];
    let pmic = vec!["vcgencmd".to_string(), "pmic_read_adc".to_string()];
    while start.elapsed() < Duration::from_secs(args.duration_s) {
        tape.lte_file(std::fs::read(&args.lte_file).ok().as_deref());
        let (o, c) = vcgencmd("get_throttled");
        tape.exec(&thr, &o, c);
        let (o, c) = vcgencmd("pmic_read_adc");
        tape.exec(&pmic, &o, c);
        if last_clock.elapsed() >= Duration::from_secs(10) {
            tape.clock();
            last_clock = Instant::now();
        }
        next += Duration::from_secs(1);
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }
    tape.clock();
    let n = tape.records();
    tape.close()?;
    eprintln!("recorded {} records in {:.0} s to {}", n, start.elapsed().as_secs_f64(), args.out.display());
    Ok(())
}
