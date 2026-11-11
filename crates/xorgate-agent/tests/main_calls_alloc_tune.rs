//! F-P3-MEM review S1: `alloc_tune.rs` proves `alloc::tune()` works, but not
//! that the binary calls it. Pin that the first statement of `fn main` is the
//! call, so deleting or moving it (after a thread could exist) fails a test.
//! Phase 7a: the one allowed predecessor is `capture_host_or_continue();`;
//! phase 7c adds `imu_reader_or_continue();` (the same kind of dispatch).

#[test]
fn main_calls_alloc_tune_first() {
    let src = include_str!("../src/main.rs");
    let body = src
        .split_once("fn main() {")
        .expect("main.rs has `fn main() {`")
        .1;
    let mut stmts = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//"));
    // Phase 7a: the capture-host dispatch may precede it (it spawns no
    // thread and never returns on the host path); phase 7c: the IMU reader
    // dispatch likewise, in that order; nothing else may.
    let mut first = stmts.next().expect("fn main has a body");
    for pre in ["capture_host_or_continue();", "imu_reader_or_continue();"] {
        if first == pre {
            first = stmts.next().expect("fn main continues");
        }
    }
    assert_eq!(first, "xorgate_agent::alloc::tune();");
}
