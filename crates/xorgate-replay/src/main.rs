//! `xorgate-replay` — the Phase 2 record/replay harness (plan
//! `phase-2-cloud-plane.md` §Record/replay; operator guide in
//! `tools/replay/README.md`).
//!
//! ```text
//! xorgate-replay inspect <tape> [--dump [--kind K]…]
//! xorgate-replay synth <scenario> -o <tape> [--duration-s N] [--fixtures DIR]
//! xorgate-replay play  --tape T --out RUN --a js|rust --b js|rust [rig flags]   (inside the rig)
//! xorgate-replay diff  RUN [--tolerances tools/replay/tolerances.toml] [-o RUN/diff.md]
//! ```

mod diff;
mod inspect;
mod play;
mod record;
mod rewrite;
mod synth;
mod tolerances;

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
}

fn main() -> Result<()> {
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
            let base = match base {
                Some(p) => Some(Tape::read(&p).with_context(|| format!("reading {}", p.display()))?),
                None => None,
            };
            let tape = synth::build_with(&scenario, duration_s, &fixtures, base.as_ref())?;
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
            let rep = diff::diff(&a, &b, &info, &pairs, &table);
            let md = diff::render(&rep, &a, &b, &info, &table, &run);
            let out = out.unwrap_or_else(|| run.join("diff.md"));
            std::fs::write(&out, &md)?;
            std::fs::write(out.with_extension("json"), serde_json::to_vec_pretty(&rep)?)?;
            let unt = rep.untolerated();
            eprintln!(
                "{}: {} untolerated, {} tolerated → {}",
                if unt == 0 { "PASS" } else { "FAIL" },
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
