//! `xorgate-replay imu-lockstep` — phase 7c C5(a) / C10b helper: replay
//! `*.imutx` transcripts (plain or `.gz`) through the Rust BNO085 client in
//! lockstep (`xorgate_core::imu_lockstep`) and report a verdict per file.
//!
//! The output is COUNTS-ONLY by default (file name, PASS/FAIL, the failure
//! class and its record/line index), so it is safe to paste into evidence
//! even for a raw bench transcript. `--verbose` prints the full mismatch text,
//! which can quote transcript bytes and sample values: private use only.
//! The C5(b) half (the same transcript through the Python oracle) is
//! `tools/imu/oracle/oracle.py replay`; `tools/imu/oracle/c5.sh` runs both.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use xorgate_core::bno08x_client::ReaderArgs;
use xorgate_core::imu_lockstep::{self, ErrnoTable};

#[derive(clap::Args)]
pub struct ImuLockstepArgs {
    /// Transcripts (`*.imutx`, optionally gzipped).
    #[arg(required = true)]
    pub files: Vec<PathBuf>,
    /// The oracle-printed errno table.
    #[arg(long, default_value = "fixtures/imu_oracle/errno.json")]
    pub errno: PathBuf,
    /// Print the full mismatch text (may quote transcript bytes: private only).
    #[arg(long)]
    pub verbose: bool,
}

fn read_records(path: &Path) -> Result<Vec<Value>> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = if path.extension().is_some_and(|e| e == "gz") {
        let mut s = String::new();
        flate2::read::GzDecoder::new(raw.as_slice()).read_to_string(&mut s)?;
        s
    } else {
        String::from_utf8(raw)?
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("line {}", i + 1)))
        .collect()
}

/// The failure class and index only: no bytes, no values.
fn redact(problem: &str) -> String {
    if let Some(rest) = problem.strip_prefix("op mismatch: ") {
        let idx = rest
            .strip_prefix("record ")
            .and_then(|r| r.split(':').next())
            .unwrap_or("?");
        if rest.starts_with("the client ended") {
            return "op mismatch: the client ended early".into();
        }
        return format!("op mismatch at record {idx}");
    }
    problem.split(':').next().unwrap_or(problem).to_string()
}

/// Returns true when every file PASSES.
pub fn run(args: &ImuLockstepArgs) -> Result<bool> {
    let table: Value = serde_json::from_str(
        &std::fs::read_to_string(&args.errno)
            .with_context(|| format!("reading {}", args.errno.display()))?,
    )?;
    let errno = ErrnoTable::from_json(&table).map_err(anyhow::Error::msg)?;
    let mut all_ok = true;
    for f in &args.files {
        let recs = read_records(f)?;
        let mode = if imu_lockstep::is_strict(&recs) {
            "strict"
        } else {
            "next-op"
        };
        let run = imu_lockstep::run(&recs, &errno, &ReaderArgs::default());
        let problems = imu_lockstep::verdict(&recs, &run);
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        let stdout_lines = run.lines().len();
        if problems.is_empty() {
            println!(
                "PASS {name} mode={mode} records={} stdout_lines={stdout_lines} exit={}{}",
                run.consumed,
                match (&run.exit, &run.killed) {
                    (Some(e), _) => format!("code:{}", e.code()),
                    (None, Some(s)) => format!("signal:{s}"),
                    _ => "none".into(),
                },
                if run.teardown_close {
                    " teardown_close"
                } else if run.python_only_close {
                    " python_only_close"
                } else {
                    ""
                }
            );
        } else {
            all_ok = false;
            for p in &problems {
                if args.verbose {
                    println!("FAIL {name} mode={mode}: {p}");
                } else {
                    println!("FAIL {name} mode={mode}: {}", redact(p));
                }
            }
        }
    }
    Ok(all_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_keeps_class_and_index_only() {
        assert_eq!(
            redact("op mismatch: record 12: client: read(4); transcript: {\"hex\":\"0102\"}"),
            "op mismatch at record 12"
        );
        assert_eq!(
            redact("stdout differs at line 3: oracle Some(\"{\\\"quat\\\": 0.5}\")"),
            "stdout differs at line 3"
        );
        assert_eq!(redact("exit: oracle code Some(0)"), "exit");
    }
}
