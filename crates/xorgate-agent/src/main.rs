//! `xorgate-rs` — the Rust device agent binary (plan README §3.3): a thin
//! shell over `cli::main` (`src/cli.tsx`'s dispatcher).
//!
//! Phase 2 verbs: `run [flags]` (the worker), `status`, `logs`, `set` and
//! `-v`/`--version` (build identity). Phase 7a: the hidden first argument
//! `__capture-host` runs the capture host (`capture_host`) instead. Phase 7c:
//! `__imu-reader` runs the BNO085 reader (`imu_reader`) instead.

#![deny(unsafe_code)]

fn main() {
    capture_host_or_continue();
    imu_reader_or_continue();
    // F-P3-MEM: glibc keeps freed ~7 MB upload buffers resident under its
    // dynamic mmap threshold; pin the threshold before any thread exists.
    xorgate_agent::alloc::tune();
    // `process.argv.slice(2)`; non-UTF-8 arguments are decoded lossily.
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    std::process::exit(xorgate_agent::cli::main(&argv));
}

/// Phase 7a: `xorgate-rs __capture-host …` is the capture host, a pure
/// pipeline runner dispatched before ANY agent init (no malloc tuning,
/// logging, env or settings); it never returns. Otherwise a no-op that
/// spawns nothing, so `alloc::tune()` still runs before any thread exists.
fn capture_host_or_continue() {
    let mut raw = std::env::args_os().skip(1);
    if raw.next().as_deref()
        == Some(std::ffi::OsStr::new(
            xorgate_agent::capture_host::SUBCOMMAND,
        ))
    {
        let host_args: Vec<std::ffi::OsString> = raw.collect();
        std::process::exit(xorgate_agent::capture_host::main(&host_args));
    }
}

/// Phase 7c: `xorgate-rs __imu-reader …` is the BNO085 reader, dispatched
/// before ANY agent init (no malloc tuning, logging, env or settings); it
/// never returns. Otherwise a no-op.
fn imu_reader_or_continue() {
    let mut raw = std::env::args_os().skip(1);
    if raw.next().as_deref() == Some(std::ffi::OsStr::new(xorgate_agent::imu_reader::SUBCOMMAND)) {
        let reader_args: Vec<std::ffi::OsString> = raw.collect();
        std::process::exit(xorgate_agent::imu_reader::main(&reader_args));
    }
}
