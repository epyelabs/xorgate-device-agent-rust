//! `xorgate-rs logs [-n N] [-f] [--clear]` — port of `src/commands/logs.ts:1-65`
//! + `readLogTail` (`src/daemon/manager.ts:92-102`), under decision O9:
//!
//! - `$XORGATE_HOME/agent.log` exists → exactly the TS: print the last N
//!   lines, `-f` follows appended bytes (reset on truncation) until Ctrl-C
//!   (exit 0), `--clear` truncates;
//! - otherwise (a systemd agent has no agent.log; it logs to the journal) →
//!   exec `journalctl -u xorgate-agent-rs -n N [-f]` (README §3.3).
//!
//! `--clear` always takes the TS path (there is nothing to clear in the
//! journal): with no agent.log it fails like `truncateSync` (`✗ ENOENT: …`).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use xorgate_core::system::js_number;

/// The journald unit the fallback reads.
pub const UNIT: &str = "xorgate-agent-rs";

/// `Number(argv[nIdx + 1]) || 200` (`logs.ts:13-14`).
pub fn line_count(args: &[String]) -> f64 {
    let Some(i) = args.iter().position(|a| a == "-n" || a == "--lines") else {
        return 200.0;
    };
    let n = args.get(i + 1).map_or(f64::NAN, |s| js_number(s));
    if n == 0.0 || n.is_nan() {
        200.0
    } else {
        n
    }
}

/// `lines.slice(-maxLines)`'s start index (`ToIntegerOrInfinity` + clamp).
fn slice_start(len: usize, max_lines: f64) -> usize {
    let start = -max_lines.trunc();
    let len_f = len as f64;
    let s = if start < 0.0 {
        (len_f + start).max(0.0)
    } else {
        start.min(len_f)
    };
    s as usize
}

/// `readLogTail(maxLines, path)` (`manager.ts:92-102`): `[]` when unreadable.
pub fn read_log_tail(max_lines: f64, path: &Path) -> Vec<String> {
    let Some(text) = xorgate_hal::read_lossy::read_text(path).ok() else {
        return Vec::new();
    };
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let start = slice_start(lines.len(), max_lines);
    lines.split_off(start)
}

/// The `journalctl` argv of the O9 fallback.
pub fn journal_args(n: f64, follow: bool) -> Vec<String> {
    let count = if n.is_finite() && n >= 1.0 {
        format!("{}", n.trunc() as u64)
    } else {
        // `slice(-Infinity)` / a negative count: everything the journal has.
        "all".to_string()
    };
    let mut a = vec!["-u".to_string(), UNIT.to_string(), "-n".to_string(), count];
    if follow {
        a.push("-f".to_string());
    }
    a
}

/// `xorgate-rs logs …` (frozen signature).
pub fn main(args: &[String]) -> i32 {
    let path = crate::env::runtime_home().join("agent.log");
    let follow = args.iter().any(|a| a == "-f" || a == "--follow");
    let clear = args.iter().any(|a| a == "--clear");
    let n = line_count(args);

    if clear {
        return match std::fs::OpenOptions::new().write(true).open(&path) {
            Ok(f) => match f.set_len(0) {
                Ok(()) => {
                    println!("✓ cleared {}", path.display());
                    0
                }
                Err(e) => {
                    eprintln!(
                        "✗ {}",
                        xorgate_hal::io::node_fs_message(&e, "ftruncate", &path)
                    );
                    1
                }
            },
            Err(e) => {
                eprintln!("✗ {}", xorgate_hal::io::node_fs_message(&e, "open", &path));
                1
            }
        };
    }

    if !path.exists() {
        return exec_journal(n, follow);
    }

    for line in read_log_tail(n, &path) {
        println!("{line}");
    }
    if !follow {
        return 0;
    }
    follow_file(&path)
}

/// O9 fallback: replace this process with `journalctl` (found on `PATH`).
fn exec_journal(n: f64, follow: bool) -> i32 {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("journalctl")
        .args(journal_args(n, follow))
        .exec();
    eprintln!("✗ journalctl: {err}");
    1
}

/// Copy the bytes appended since `offset` (up to `size`) to `out` — the
/// `pump` of `logs.ts:41-54`. A read hiccup just ends this round (as
/// `readSync <= 0` does); only a WRITE/flush failure is an `Err`.
fn pump(
    f: &mut std::fs::File,
    offset: &mut u64,
    size: u64,
    buf: &mut [u8],
    out: &mut impl Write,
) -> std::io::Result<()> {
    while *offset < size {
        let want = (buf.len() as u64).min(size - *offset) as usize;
        if f.seek(SeekFrom::Start(*offset)).is_err() {
            break;
        }
        let read = match f.read(&mut buf[..want]) {
            Ok(r) if r > 0 => r,
            _ => break,
        };
        *offset += read as u64;
        out.write_all(&buf[..read])?;
    }
    out.flush()
}

/// `logs.ts:30-64`: re-read appended bytes on change; reset on truncation;
/// Ctrl-C exits 0. Polls every 200 ms in place of `fs.watch`. A stdout
/// write error (EPIPE once the reader of `logs -f | grep -m1 X` is gone)
/// ends the follow with exit 1: Node's `process.stdout.write` (`:54`) emits
/// an unhandled EPIPE `error` there and the process dies non-zero.
fn follow_file(path: &Path) -> i32 {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("(no log yet at {} — waiting…)", path.display());
            return 0;
        }
    };
    let mut offset = f.metadata().map(|m| m.len()).unwrap_or(0);
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("✗ {e}");
            return 1;
        }
    };
    rt.block_on(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
        let mut buf = vec![0u8; 64 * 1024];
        // ONE listener for the whole loop (the TS installs one SIGINT handler,
        // `:59`): a fresh `ctrl_c()` per `select!` would drop a SIGINT that
        // lands while the tick arm runs.
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        loop {
            tokio::select! {
                _ = &mut ctrl_c => return 0,
                _ = tick.tick() => {
                    let Ok(size) = f.metadata().map(|m| m.len()) else { continue };
                    if size < offset {
                        offset = 0; // truncated
                    }
                    let mut stdout = std::io::stdout().lock();
                    if pump(&mut f, &mut offset, size, &mut buf, &mut stdout).is_err() {
                        return 1;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn pump_copies_appended_bytes_and_surfaces_write_errors() {
        let d = std::env::temp_dir().join(format!("xg-logs-pump-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("agent.log");
        std::fs::write(&p, "old\nnew 1\nnew 2\n").unwrap();
        let mut f = std::fs::File::open(&p).unwrap();
        let mut buf = vec![0u8; 4]; // several reads per round
        let mut offset = 4;
        let mut out = Vec::new();
        pump(&mut f, &mut offset, 16, &mut buf, &mut out).unwrap();
        assert_eq!(out, b"new 1\nnew 2\n");
        assert_eq!(offset, 16);
        let mut offset = 0;
        assert!(pump(&mut f, &mut offset, 16, &mut buf, &mut Broken).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn count_is_number_or_200() {
        assert_eq!(line_count(&s(&[])), 200.0);
        assert_eq!(line_count(&s(&["-n", "5"])), 5.0);
        assert_eq!(line_count(&s(&["--lines", "0x10"])), 16.0);
        assert_eq!(line_count(&s(&["-n", "abc"])), 200.0);
        assert_eq!(line_count(&s(&["-n", "0"])), 200.0);
        assert_eq!(line_count(&s(&["-n"])), 200.0);
    }

    #[test]
    fn tail_follows_slice_semantics() {
        let d = std::env::temp_dir().join(format!("xg-logs-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("agent.log");
        std::fs::write(&p, "a\nb\nc\nd\n").unwrap();
        assert_eq!(read_log_tail(2.0, &p), s(&["c", "d"]));
        assert_eq!(read_log_tail(2.5, &p), s(&["c", "d"]));
        assert_eq!(read_log_tail(200.0, &p), s(&["a", "b", "c", "d"]));
        assert_eq!(read_log_tail(-1.0, &p), s(&["b", "c", "d"]));
        assert_eq!(read_log_tail(f64::INFINITY, &p), s(&["a", "b", "c", "d"]));
        assert!(read_log_tail(5.0, &d.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn journal_fallback_argv() {
        assert_eq!(
            journal_args(200.0, false),
            s(&["-u", "xorgate-agent-rs", "-n", "200"])
        );
        assert_eq!(
            journal_args(5.0, true),
            s(&["-u", "xorgate-agent-rs", "-n", "5", "-f"])
        );
        assert_eq!(journal_args(-3.0, false)[3], "all");
    }
}
