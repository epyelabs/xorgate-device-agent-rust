//! Phase 7c WP3 (`phase-7c-rust-imu.md` §Design 6, fix-check R1): the rig's
//! Rust-backend IMU relay, `xorgate-replay __imu-reader …`.
//!
//! A side run with `XORGATE_IMU_BACKEND=rust` spawns `XORGATE_IMU_READER_BIN`
//! with `argv[0] = xorgate-imu` and args `__imu-reader --address 0x4a`; the
//! player points that BIN at ITS OWN binary (`rig_side_env`), so this relay
//! runs in place of the Rust reader, exactly as `tools/replay/fake-python/
//! bno085_read.py` runs in place of the Python helper: it connects to the
//! player's per-agent socket `$RIG_IMU_SOCK` and relays the tape's IMU lines
//! verbatim; `__EXIT__ <code>` ends it with that code; SIGINT ends it with 0;
//! SIGTERM keeps its default action.
//!
//! It ASSERTS its own argv first: `["xorgate-imu", "__imu-reader",
//! "--address", "0x4a"]`. On any mismatch it prints `{"type": "error",
//! "error": "init failed: rig argv [...]"}` and exits 3, so the collector
//! parks and the run FAILS (C7's negative). A relay SCRIPT could not do this:
//! the kernel's `#!` handling replaces `argv[0]` with the script path, so a
//! Python relay never sees `xorgate-imu` (the plan's `tools/replay/
//! fake-imu-reader` is therefore this ELF subcommand; wp3.md).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
use xorgate_core::py_fmt::{error_line, str_repr};

/// The hidden subcommand (the Rust reader's).
pub const SUBCOMMAND: &str = "__imu-reader";

/// The argv the agent's `rust` backend must give its reader.
pub const WANT_ARGV: [&str; 4] = ["xorgate-imu", "__imu-reader", "--address", "0x4a"];

/// Python's `repr(list_of_str)`.
pub fn py_list_repr(argv: &[String]) -> String {
    let items: Vec<String> = argv.iter().map(|a| str_repr(a)).collect();
    format!("[{}]", items.join(", "))
}

/// The relay's verdict on its own argv: `None` = as the agent must spawn it,
/// else the error line it prints before exiting 3.
pub fn argv_check(argv: &[String]) -> Option<String> {
    if argv.iter().map(String::as_str).eq(WANT_ARGV) {
        None
    } else {
        Some(error_line(&format!(
            "init failed: rig argv {}",
            py_list_repr(argv)
        )))
    }
}

fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// `xorgate-replay __imu-reader …`: returns the exit code.
pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if let Some(line) = argv_check(&argv) {
        emit(&line);
        return 3;
    }
    // SIGINT → exit 0 (the fake helper's KeyboardInterrupt), from a waiter
    // thread; blocked here first so no thread takes the default action.
    let mut set = SigSet::empty();
    set.add(Signal::SIGINT);
    let _ = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None);
    std::thread::spawn(move || {
        // Every relayed line is already flushed (the main thread holds the
        // stdout lock while it reads, so taking it here would deadlock).
        if set.wait().is_ok() {
            std::process::exit(0);
        }
    });
    let Some(path) = std::env::var_os("RIG_IMU_SOCK").filter(|p| !p.is_empty()) else {
        emit(&error_line("init failed: RIG_IMU_SOCK unset (rig fake)"));
        return 3;
    };
    let sock = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            emit(&error_line(&format!("init failed: rig socket {e}")));
            return 3;
        }
    };
    let mut r = BufReader::new(sock);
    let mut out = std::io::stdout().lock();
    loop {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => return 0,
            Ok(_) => {}
        }
        if line.starts_with("__EXIT__") {
            return line
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse::<i32>().ok())
                .unwrap_or(1);
        }
        if out.write_all(line.as_bytes()).is_err() || out.flush().is_err() {
            return 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_the_agents_argv_passes() {
        assert_eq!(argv_check(&v(&WANT_ARGV)), None);
        // arg0 dropped: argv[0] is the program path
        assert_eq!(
            argv_check(&v(&[
                "/rig/cache/bin/xorgate-replay",
                "__imu-reader",
                "--address",
                "0x4a"
            ]))
            .unwrap(),
            "{\"type\": \"error\", \"error\": \"init failed: rig argv \
             ['/rig/cache/bin/xorgate-replay', '__imu-reader', '--address', '0x4a']\"}"
        );
        for bad in [
            v(&["xorgate-imu", "__imu-reader", "--address", "0x4b"]),
            v(&["xorgate-imu", "__imu-reader"]),
            v(&["xorgate-imu", "__imu-reader", "--address", "0x4a", "-x"]),
            v(&["python3", "__imu-reader", "--address", "0x4a"]),
        ] {
            assert!(argv_check(&bad).is_some(), "{bad:?}");
        }
    }
}
