//! Lines to stdout / stderr that can NEVER panic.
//!
//! `println!` and `eprintln!` panic when the write fails, and a write
//! fails as soon as the reader is gone: a terminal that hung up (EIO), a
//! pipe whose other end closed (EPIPE; Rust ignores SIGPIPE). For most
//! commands that is a fair end. For `xorgate-rs camera requirements
//! --install` and for `xorgate-rs update` it is not: they may be in the
//! middle of an apt transaction (or about to hold packages and restart the
//! agent) when the SSH session that started them drops, and a panic there
//! would unwind through the cleanup and abandon the work half-done.
//!
//! So these two commands print through here: a line that cannot be
//! delivered is dropped, and the command goes on.

use std::io::Write;

/// One line to stdout; a write error is ignored.
pub fn out(line: &str) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{line}");
    let _ = o.flush();
}

/// One line to stderr; a write error is ignored.
pub fn err(line: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}
