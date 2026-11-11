//! The Rust BNO085 reader (phase 7c WP3, `phase-7c-rust-imu.md` §Design 1-2,
//! P2/P5, R1): `xorgate-rs __imu-reader [--address A] [--interval S]`, run by
//! the IMU collector as a supervised child with `argv[0] = xorgate-imu` when
//! `XORGATE_IMU_BACKEND=rust`. It replaces `python3 python/bno085_read.py`
//! with the same (stdout, stderr, exit) triple per failure class; everything
//! downstream (the collector, the NDJSON consumer, the log lines) is
//! unchanged.
//!
//! It is a pure device reader: `main.rs` dispatches here before ANY agent
//! init (no `alloc::tune`, no logging, no env, settings or `/etc/xorgate`
//! read). Single-threaded, blocking, no tokio. Startup order:
//! 1. `PR_SET_NAME("xorgate-imu")` (`comm`; `pgrep -x xorgate-imu` counts
//!    SHTP clients in WP4, even under the B″ strace wrapper);
//! 2. argparse-compatible flags (`bno085_read.py:26-30`);
//! 3. [`bno08x_client::run`] over the real i2c-dev transport
//!    ([`xorgate_hal::i2c_dev::I2cDev`]), CPython's clock ([`RealClock`]) and
//!    stdout ([`StdoutSink`]).
//!
//! **Signals (plan §Design 1, fix-check R6; D83).** No handler is ever
//! installed. SIGINT keeps its default disposition through init (a SIGINT
//! there kills the process by SIGINT, as CPython's uncaught
//! `KeyboardInterrupt` does). Right after `ready` is written
//! ([`ImuSink::loop_starts`], the helper's `try:` at `:76`) SIGINT is
//! BLOCKED, and the loop's interval wait is `sigtimedwait({SIGINT},
//! interval)`: a SIGINT → exit 0; the timeout → the next iteration; `EINTR`
//! → wait out the rest of the interval. A SIGINT that lands mid-I/O stays
//! pending until that wait (D83(b)). SIGTERM keeps its default action
//! throughout.
//!
//! **stdout:** each CPython flush is ONE `write(2)` on fd 1 (never `writev`,
//! never Rust's buffered `Stdout`; review-wp2 F2: C10b's transcript sees
//! `write(1, …)` only). EPIPE (Rust ignores SIGPIPE) → exit 1 (D83: Python
//! 120).
//!
//! **Teardown on a normal exit (0/3/4, and 1)** — orchestrator ruling on
//! review-wp2 F4/X6: close the bus fd (ONE `close(2)`, CPython's
//! `SMBus.__del__`), THEN write [`bno08x_client::ReaderEnd::unflushed`] in
//! ONE write, THEN exit with [`bno08x_client::ReaderExit::code`].
//! `std::process::exit` skips `Drop`, so the close is explicit. A death by
//! signal needs no close (the kernel closes the fd).

mod sys;
#[cfg(feature = "imu-test-bus")]
mod test_bus;

use std::ffi::{CStr, OsString};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use xorgate_core::bno08x_client::{self, ImuSink, ReaderArgs, StdoutClosed};
use xorgate_core::imu_io::{I2cOps, ImuClock, IntervalWait};
use xorgate_core::py_fmt::str_repr;
use xorgate_hal::i2c_dev::I2cDev;

/// The hidden subcommand `main.rs` dispatches before anything else.
pub const SUBCOMMAND: &str = "__imu-reader";
/// `argv[0]` (the collector's `ChildSpec::arg0`), `comm` (PR_SET_NAME) and
/// the tape's `imu_spawn` argv[0].
pub const PROCESS_NAME: &str = "xorgate-imu";
const PROCESS_NAME_C: &CStr = c"xorgate-imu";
/// argparse's `prog` for the usage/help text. Python prints
/// `bno085_read.py` (its `argv[0]` basename); only an operator running the
/// reader by hand with bad flags sees it (D81: process identity).
const PROG: &str = "xorgate-imu";

/// The bus the reader drives, plus the close CPython's teardown performs.
pub trait ReaderBus: I2cOps {
    /// Close the device fd (one `close(2)`; nothing when never opened).
    fn close_bus(&mut self);
}

impl ReaderBus for I2cDev {
    fn close_bus(&mut self) {
        self.close();
    }
}

/// `xorgate-rs __imu-reader <args…>` (the args AFTER the subcommand). Returns
/// the process exit code.
pub fn main(raw: &[OsString]) -> i32 {
    sys::set_process_name(PROCESS_NAME_C);
    let argv: Vec<String> = raw
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let args = match parse_args(&argv) {
        Ok(Parsed::Run(a)) => a,
        Ok(Parsed::Help) => {
            let _ = StdoutSink.write_chunk(&help_text());
            return 0;
        }
        Err(ArgError::Usage(msg)) => {
            write_stderr(&format!("{}{PROG}: error: {msg}\n", usage_line()));
            return 2;
        }
        Err(ArgError::Value(msg)) => {
            // Python: an uncaught ValueError from parse_address → traceback,
            // exit 1. The last traceback line is the message.
            write_stderr(&format!("ValueError: {msg}\n"));
            return 1;
        }
    };
    #[cfg(feature = "imu-test-bus")]
    if let Some(path) = std::env::var_os(test_bus::ENV) {
        return test_bus::run(&path, &args);
    }
    run_reader(&mut I2cDev::new(), &mut RealClock, &args)
}

/// The helper's `main()` after argument parsing, then CPython's teardown
/// order: close the bus, write the unflushed text in one write, return the
/// exit code.
pub fn run_reader<B: ReaderBus, C: ImuClock>(bus: &mut B, clock: &mut C, args: &ReaderArgs) -> i32 {
    let mut sink = StdoutSink;
    let end = bno08x_client::run(bus, clock, &mut sink, args);
    bus.close_bus();
    if !end.unflushed.is_empty() {
        // A failed final flush changes nothing here (Python: exit 120 on a
        // broken pipe at teardown; unobservable, the agent is gone — D83).
        let _ = sink.write_chunk(&end.unflushed);
    }
    end.exit.code()
}

/// The reader's stdout: every chunk is one `write(2)` on fd 1 (looped only
/// for a partial write or `EINTR`, as CPython's `BufferedWriter.flush` does;
/// a pipe write of a line is never partial in practice).
#[derive(Debug, Default)]
pub struct StdoutSink;

impl ImuSink for StdoutSink {
    fn write_chunk(&mut self, chunk: &str) -> Result<(), StdoutClosed> {
        let out = std::io::stdout();
        let mut b = chunk.as_bytes();
        while !b.is_empty() {
            match nix::unistd::write(out.as_fd(), b) {
                Ok(0) => return Err(StdoutClosed),
                Ok(n) => b = &b[n..],
                Err(nix::errno::Errno::EINTR) => continue,
                // EPIPE (the agent is gone) or any other OSError: Python's
                // uncaught exception → exit 1.
                Err(_) => return Err(StdoutClosed),
            }
        }
        Ok(())
    }

    /// The helper's `try:` (`bno085_read.py:76`): from here a SIGINT means
    /// `return 0`, taken at the interval wait.
    fn loop_starts(&mut self) {
        sys::block_sigint();
    }
}

fn write_stderr(text: &str) {
    let err = std::io::stderr();
    let _ = nix::unistd::write(err.as_fd(), text.as_bytes());
}

/// CPython 3.11's `time` module on Linux.
#[derive(Debug, Default)]
pub struct RealClock;

/// `_PyTime_AsSecondsDouble` (CPython 3.11): integer division when the
/// nanoseconds are a whole number of seconds, else `ns as f64 / 1e9` — the
/// same doubles `time.monotonic()` returns for the same `CLOCK_MONOTONIC`
/// reading (review-wp1 §5 note (d)).
pub fn py_monotonic(ns: u128) -> f64 {
    if ns.is_multiple_of(1_000_000_000) {
        (ns / 1_000_000_000) as f64
    } else {
        ns as f64 / 1e9
    }
}

/// A sleep length as a `Duration`. The client only ever passes the init
/// back-offs (0.5 s) and the interval (0.05 s by default); a negative or NaN
/// length (Python: `ValueError`) is a zero wait and an infinite one is capped
/// (Python: `OverflowError`) — unreachable with the agent's argv (D81 class).
fn duration(secs: f64) -> Duration {
    if secs.is_nan() || secs <= 0.0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(secs.min(1e9))
    }
}

impl ImuClock for RealClock {
    fn monotonic(&mut self) -> f64 {
        py_monotonic(xorgate_hal::fragment_stat::hrtime_ns())
    }

    /// `time.sleep` during init: SIGINT is at its default disposition here,
    /// so a SIGINT kills the process (D83(a)). `std::thread::sleep` resumes
    /// after `EINTR` like CPython's (PEP 475).
    fn sleep(&mut self, secs: f64) {
        std::thread::sleep(duration(secs));
    }

    /// The loop's `time.sleep(args.interval)`: `sigtimedwait({SIGINT})`.
    fn interval_wait(&mut self, secs: f64) -> IntervalWait {
        let deadline = Instant::now() + duration(secs);
        loop {
            let rest = deadline.saturating_duration_since(Instant::now());
            match sys::wait_sigint(rest) {
                sys::SigWait::Sigint => return IntervalWait::Interrupted,
                sys::SigWait::Timeout => return IntervalWait::Elapsed,
                sys::SigWait::Eintr => continue,
            }
        }
    }
}

// ---------------------------------------------------------------- arguments

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Run(ReaderArgs),
    /// `-h` / `--help`: argparse prints the help and exits 0.
    Help,
}

/// argparse's two failure kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgError {
    /// `parser.error(...)`: usage + message on stderr, exit 2.
    Usage(String),
    /// `parse_address` raised `ValueError` (uncaught in the helper): exit 1.
    Value(String),
}

fn usage_line() -> String {
    format!("usage: {PROG} [-h] [--address ADDRESS] [--interval INTERVAL]\n")
}

/// argparse 3.11's help for the helper's parser (`help_position` 23).
pub fn help_text() -> String {
    format!(
        "{}\noptions:\n  -h, --help           show this help message and exit\n  \
         --address ADDRESS    I2C address, e.g. 0x4a or 0x4b\n  \
         --interval INTERVAL  seconds between samples\n",
        usage_line()
    )
}

const LONG_OPTS: [&str; 3] = ["--help", "--address", "--interval"];

/// argparse's `_negative_number_matcher` (`^-\d+$|^-\d*\.\d+$`): such a token
/// is a value, not an option, in a parser with no numeric-looking options.
fn looks_negative(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('-') else {
        return false;
    };
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    match rest.split_once('.') {
        None => digits(rest),
        Some((a, b)) => (a.is_empty() || digits(a)) && digits(b),
    }
}

/// The helper's `argparse.ArgumentParser` (`--address` default `"0x4a"`,
/// `--interval` `type=float` default 0.05; `allow_abbrev`: unique prefixes
/// of the long options), then `parse_address`.
pub fn parse_args(argv: &[String]) -> Result<Parsed, ArgError> {
    let mut address = "0x4a".to_string();
    let mut interval = 0.05_f64;
    let mut unrecognized: Vec<String> = Vec::new();
    let mut i = 0;
    let mut positional_only = false;
    while i < argv.len() {
        let tok = &argv[i];
        i += 1;
        if positional_only || !tok.starts_with('-') || tok == "-" || looks_negative(tok) {
            unrecognized.push(tok.clone());
            continue;
        }
        if tok == "--" {
            positional_only = true;
            continue;
        }
        if tok == "-h" {
            return Ok(Parsed::Help);
        }
        let (name, inline) = match tok.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (tok.as_str(), None),
        };
        let matches: Vec<&str> = if LONG_OPTS.contains(&name) {
            vec![name]
        } else if name.starts_with("--") {
            LONG_OPTS
                .iter()
                .copied()
                .filter(|o| o.starts_with(name))
                .collect()
        } else {
            Vec::new()
        };
        let opt = match matches.as_slice() {
            [one] => *one,
            [] => {
                unrecognized.push(tok.clone());
                continue;
            }
            many => {
                return Err(ArgError::Usage(format!(
                    "ambiguous option: {name} could match {}",
                    many.join(", ")
                )))
            }
        };
        if opt == "--help" {
            if inline.is_some() {
                return Err(ArgError::Usage(format!(
                    "argument -h/--help: ignored explicit argument {}",
                    str_repr(inline.as_deref().unwrap_or(""))
                )));
            }
            return Ok(Parsed::Help);
        }
        let value = match inline {
            Some(v) => v,
            None => match argv.get(i) {
                Some(v) if !v.starts_with('-') || looks_negative(v) => {
                    i += 1;
                    v.clone()
                }
                _ => {
                    return Err(ArgError::Usage(format!(
                        "argument {opt}: expected one argument"
                    )))
                }
            },
        };
        if opt == "--address" {
            address = value;
        } else {
            interval = py_float(&value).ok_or_else(|| {
                ArgError::Usage(format!(
                    "argument --interval: invalid float value: {}",
                    str_repr(&value)
                ))
            })?;
        }
    }
    if !unrecognized.is_empty() {
        return Err(ArgError::Usage(format!(
            "unrecognized arguments: {}",
            unrecognized.join(" ")
        )));
    }
    Ok(Parsed::Run(ReaderArgs {
        address: parse_address(&address)?,
        interval,
    }))
}

/// `parse_address` (`bno085_read.py:21-22`): `int(value, 16)` when the RAW
/// string lower-cases to a `0x` prefix, else `int(value)`. Values beyond
/// `i64` are refused (Python has big ints; the agent passes 0x4a/0x4b only).
pub fn parse_address(value: &str) -> Result<i64, ArgError> {
    let base = if value.to_lowercase().starts_with("0x") {
        16
    } else {
        10
    };
    py_int(value, base).ok_or_else(|| {
        ArgError::Value(format!(
            "invalid literal for int() with base {base}: {}",
            str_repr(value)
        ))
    })
}

/// Digits with single underscores only BETWEEN digits (PEP 515).
fn strip_underscores(s: &str, is_digit: impl Fn(char) -> bool) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    for (k, c) in chars.iter().enumerate() {
        if *c == '_' {
            let ok =
                k > 0 && k + 1 < chars.len() && is_digit(chars[k - 1]) && is_digit(chars[k + 1]);
            if !ok {
                return None;
            }
        } else {
            out.push(*c);
        }
    }
    Some(out)
}

/// CPython `int(s, base)` for base 10 / 16 (whitespace, sign, `0x` prefix
/// for 16 with an optional `_` after it, PEP 515 underscores).
pub fn py_int(s: &str, base: u32) -> Option<i64> {
    let t = s.trim();
    let (neg, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let t = if base == 16 {
        let t = t
            .strip_prefix("0x")
            .or_else(|| t.strip_prefix("0X"))
            .map_or(t, |r| r.strip_prefix('_').unwrap_or(r));
        t
    } else {
        t
    };
    if t.is_empty() {
        return None;
    }
    let digits = strip_underscores(t, |c| c.is_digit(base))?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
        return None;
    }
    let mag = i64::from_str_radix(&digits, base).ok()?;
    Some(if neg { -mag } else { mag })
}

/// CPython `float(s)`: whitespace, PEP 515 underscores, `inf`/`nan` words.
pub fn py_float(s: &str) -> Option<f64> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let word = lower.trim_start_matches(['+', '-']);
    if matches!(word, "inf" | "infinity" | "nan") {
        return t.parse::<f64>().ok();
    }
    if t.is_empty()
        || !t
            .bytes()
            .all(|b| b.is_ascii_digit() || b"+-.eE_".contains(&b))
    {
        return None;
    }
    let plain = strip_underscores(t, |c| c.is_ascii_digit())?;
    plain.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Parsed, ArgError> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn run(address: i64, interval: f64) -> Result<Parsed, ArgError> {
        Ok(Parsed::Run(ReaderArgs { address, interval }))
    }

    #[test]
    fn the_agents_argv_and_the_defaults() {
        assert_eq!(p(&["--address", "0x4a"]), run(0x4a, 0.05));
        assert_eq!(p(&[]), run(0x4a, 0.05));
        assert_eq!(p(&["--address", "0x4b"]), run(0x4b, 0.05));
    }

    #[test]
    fn argparse_forms() {
        assert_eq!(p(&["--address=0x4b"]), run(0x4b, 0.05));
        assert_eq!(p(&["--addr", "0x4b"]), run(0x4b, 0.05));
        assert_eq!(p(&["--a=74"]), run(74, 0.05));
        assert_eq!(p(&["--interval", "0.1"]), run(0x4a, 0.1));
        assert_eq!(p(&["--int=1e-1", "--address", "0X4B"]), run(0x4b, 0.1));
        assert_eq!(
            p(&["--address", "0x4a", "--address", "0x4b"]),
            run(0x4b, 0.05)
        );
        assert_eq!(p(&["--address", "-74"]), run(-74, 0.05));
        assert_eq!(p(&["--interval", "1_0.5"]), run(0x4a, 10.5));
        assert_eq!(p(&["-h"]), Ok(Parsed::Help));
        assert_eq!(p(&["--he"]), Ok(Parsed::Help));
        assert_eq!(p(&["--address", "0x4a", "--help"]), Ok(Parsed::Help));
    }

    #[test]
    fn argparse_errors_exit_2_and_parse_address_errors_exit_1() {
        let usage = |r: Result<Parsed, ArgError>| match r {
            Err(ArgError::Usage(m)) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            usage(p(&["--address"])),
            "argument --address: expected one argument"
        );
        assert_eq!(
            usage(p(&["--address", "--interval", "1"])),
            "argument --address: expected one argument"
        );
        assert_eq!(
            usage(p(&["--interval", "x"])),
            "argument --interval: invalid float value: 'x'"
        );
        assert_eq!(usage(p(&["foo", "-x"])), "unrecognized arguments: foo -x");
        assert_eq!(
            usage(p(&["--", "--address"])),
            "unrecognized arguments: --address"
        );
        assert_eq!(
            p(&["--address", "0xzz"]),
            Err(ArgError::Value(
                "invalid literal for int() with base 16: '0xzz'".into()
            ))
        );
        assert_eq!(
            p(&["--address", " 0x4a"]),
            Err(ArgError::Value(
                "invalid literal for int() with base 10: ' 0x4a'".into()
            ))
        );
    }

    #[test]
    fn python_int_and_float() {
        assert_eq!(py_int("0x4a", 16), Some(74));
        assert_eq!(py_int("4a", 16), Some(74));
        assert_eq!(py_int("0x_4a", 16), Some(74));
        assert_eq!(py_int(" 74 ", 10), Some(74));
        assert_eq!(py_int("0_74", 10), Some(74));
        assert_eq!(py_int("7__4", 10), None);
        assert_eq!(py_int("_74", 10), None);
        assert_eq!(py_int("0x", 16), None);
        assert_eq!(py_int("", 10), None);
        assert_eq!(py_float(" .5 "), Some(0.5));
        assert_eq!(py_float("5e-2"), Some(0.05));
        assert_eq!(py_float("abc"), None);
        assert!(py_float("nan").is_some_and(f64::is_nan));
        assert_eq!(py_float("-inf"), Some(f64::NEG_INFINITY));
    }

    #[test]
    fn help_is_argparse_311_layout() {
        assert_eq!(
            help_text(),
            "usage: xorgate-imu [-h] [--address ADDRESS] [--interval INTERVAL]\n\
             \n\
             options:\n  \
             -h, --help           show this help message and exit\n  \
             --address ADDRESS    I2C address, e.g. 0x4a or 0x4b\n  \
             --interval INTERVAL  seconds between samples\n"
        );
    }

    #[test]
    fn monotonic_is_cpythons_seconds_double() {
        assert_eq!(py_monotonic(5_000_000_000), 5.0);
        assert_eq!(py_monotonic(1_590_550_000), 1_590_550_000_f64 / 1e9);
        assert_eq!(py_monotonic(1_590_550_000).to_bits(), 4609842019559961054);
        assert_eq!(py_monotonic(0), 0.0);
    }

    #[test]
    fn sleep_lengths_never_panic() {
        assert_eq!(duration(-1.0), Duration::ZERO);
        assert_eq!(duration(f64::NAN), Duration::ZERO);
        assert_eq!(duration(0.05), Duration::from_secs_f64(0.05));
        assert_eq!(duration(f64::INFINITY), Duration::from_secs(1_000_000_000));
    }

    #[test]
    fn names() {
        assert_eq!(SUBCOMMAND, "__imu-reader");
        assert_eq!(PROCESS_NAME, "xorgate-imu");
        assert_eq!(PROCESS_NAME_C.to_str(), Ok(PROCESS_NAME));
    }
}
