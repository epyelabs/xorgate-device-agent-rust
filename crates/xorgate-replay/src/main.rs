//! `xorgate-replay` — the Phase 2 record/replay harness (plan
//! `phase-2-cloud-plane.md` §Record/replay; operator guide in
//! `tools/replay/README.md`).
//!
//! ```text
//! xorgate-replay inspect <tape> [--dump [--kind K]…]
//! xorgate-replay synth <scenario> -o <tape> [--duration-s N] [--fixtures DIR]
//! xorgate-replay play  --tape T --out RUN --a js|rust --b js|rust [rig flags]   (inside the rig)
//! xorgate-replay diff  RUN [--tolerances tools/replay/tolerances.toml] [-o RUN/diff.md]
//! xorgate-replay imu-lockstep FILE.imutx… [--errno fixtures/imu_oracle/errno.json] [--verbose]
//! xorgate-replay __imu-reader --address 0x4a   (the rig's IMU relay, run by a `rust`-backend agent)
//! ```
#![allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: NOT an agent spawn site (see clippy.toml)

mod aws;
mod diff;
mod diff_p4;
mod handoff;
mod imu_lockstep_cli;
mod imu_relay;
mod inspect;
mod media;
mod media_diff;
mod p4;
mod p5b;
mod play;
mod record;
mod rewrite;
mod synth;
mod takeover;
mod tolerances;
mod watch;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use xorgate_hal::tape::Tape;

#[derive(Parser)]
#[command(
    name = "xorgate-replay",
    version,
    about = "xorgate record/replay harness"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Summarise a tape (header, kinds, conn spans, wall steps, truncation).
    Inspect {
        tape: PathBuf,
        /// Print every record (NDJSON) after the summary.
        #[arg(long)]
        dump: bool,
        /// With --dump: only these kinds.
        #[arg(long = "kind")]
        kinds: Vec<String>,
    },
    /// Build a deterministic tape from fixtures.
    Synth {
        /// Scenario name (`--list` shows them).
        scenario: Option<String>,
        #[arg(short, long)]
        out: Option<PathBuf>,
        #[arg(long)]
        duration_s: Option<u64>,
        /// The repo's `fixtures/` dir.
        #[arg(long, default_value = "fixtures")]
        fixtures: PathBuf,
        #[arg(long)]
        list: bool,
        /// Splice onto a recorded base tape (`record-shared`): its LTE /
        /// power / clock records replace the synthetic ones.
        #[arg(long)]
        base: Option<PathBuf>,
    },
    /// Record the SHARED inputs only (LTE file, vcgencmd, dt) on the bench.
    RecordShared(record::RecordArgs),
    /// Rig player: run two agents against one broker on a tape (in the rig).
    Play(Box<play::PlayArgs>),
    /// Diff a run dir (`<run>/a`, `<run>/b`) under the tolerance table.
    Diff {
        /// The run dir written by `play`.
        run: PathBuf,
        #[arg(long, default_value = "tools/replay/tolerances.toml")]
        tolerances: PathBuf,
        /// Output markdown (default `<run>/diff.md`; `diff.json` next to it).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Phase 7c C5(a)/C10b: replay `*.imutx` transcripts through the Rust
    /// BNO085 client in lockstep (counts-only verdicts; exit 1 on any FAIL).
    ImuLockstep(imu_lockstep_cli::ImuLockstepArgs),
}

fn main() -> Result<()> {
    // Phase 7c WP3: `xorgate-replay __imu-reader …` is the rig's Rust-backend
    // IMU relay (the player points a `rust` side's XORGATE_IMU_READER_BIN at
    // this binary); dispatched before clap, like the agent's own subcommand.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(imu_relay::SUBCOMMAND)) {
        std::process::exit(imu_relay::main());
    }
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Inspect { tape, dump, kinds } => {
            let t = Tape::read(&tape).with_context(|| format!("reading {}", tape.display()))?;
            let s = inspect::summarize(&t);
            print!("{}", inspect::render(&t, &s));
            if dump {
                print!("{}", inspect::dump(&t, &kinds));
            }
        }
        Cmd::Synth {
            scenario,
            out,
            duration_s,
            fixtures,
            list,
            base,
        } => {
            if list || scenario.is_none() {
                for (name, dur, tracks, desc) in synth::SCENARIOS {
                    println!("{name:<14} {dur:>4} s  {desc}  [{}]", tracks.join(","));
                }
                return Ok(());
            }
            let scenario = scenario.unwrap_or_default();
            let out = out.unwrap_or_else(|| PathBuf::from(format!("{scenario}.ndjson.gz")));
            let tape = match base {
                Some(p) => {
                    let base =
                        Tape::read(&p).with_context(|| format!("reading {}", p.display()))?;
                    synth::build_with(&scenario, duration_s, &fixtures, Some(&base))?
                }
                None => synth::build(&scenario, duration_s, &fixtures)?,
            };
            let gz = out.extension().is_some_and(|e| e == "gz");
            tape.write(&out, gz)
                .with_context(|| format!("writing {}", out.display()))?;
            eprintln!(
                "wrote {} ({} records, {:.0} s)",
                out.display(),
                tape.records.len(),
                tape.duration_ms() as f64 / 1000.0
            );
        }
        Cmd::RecordShared(args) => record::run(args)?,
        Cmd::ImuLockstep(args) => {
            if !imu_lockstep_cli::run(&args)? {
                std::process::exit(1);
            }
        }
        Cmd::Play(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(play::run(*args))?;
        }
        Cmd::Diff {
            run,
            tolerances,
            out,
        } => {
            let table = tolerances::Table::load(&tolerances)?;
            let (a, b, info, pairs) = diff::load_run(&run, None, None)?;
            let mut rep = diff::diff(&a, &b, &info, &pairs, &table);
            // Phase 3 streams (media_diff.rs): children, sessions, sidecars,
            // mp4, telemetry segments, S3, counters, evictions, shm.
            let shm: Vec<serde_json::Value> = std::fs::read_to_string(run.join("shm.ndjson"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            let ma = media_diff::MediaCapture::load(&run.join("a"), &shm);
            let mb = media_diff::MediaCapture::load(&run.join("b"), &shm);
            media_diff::diff_media(&ma, &mb, &info, &table, &mut rep);
            // Phase 6 P3a: the takeover stream of a handoff run (takeover.rs).
            if let Some(h) = &info.handoff {
                takeover::diff_takeover(&run, h, &ma, &mb, &mut rep);
            }
            // Phase 4 streams (diff_p4.rs): led, at.
            let pa = diff_p4::P4Capture::load(&run.join("a"));
            let pb = diff_p4::P4Capture::load(&run.join("b"));
            diff_p4::diff_p4(&pa, &pb, &table, &mut rep);
            diff_p4::retag_clock_iso(&table, &a, &b, &mut rep);
            diff_p4::retag_trust_published(&table, &a, &b, &info, &mut rep);
            diff_p4::diff_obd_gate(&run, &mut rep);
            rep.hits = diff::row_hits(&rep, &table);
            let md = diff::render(&rep, &a, &b, &info, &table, &run);
            let out = out.unwrap_or_else(|| run.join("diff.md"));
            std::fs::write(&out, &md)?;
            std::fs::write(out.with_extension("json"), serde_json::to_vec_pretty(&rep)?)?;
            let unt = rep.untolerated();
            eprintln!(
                "{}: {} untolerated, {} tolerated → {}",
                diff::verdict(&rep),
                unt,
                rep.mismatches.len() - unt,
                out.display()
            );
            if unt > 0 {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
