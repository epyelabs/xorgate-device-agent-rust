//! F-P3-MEM review S1: `alloc_tune.rs` proves `alloc::tune()` works, but not
//! that the binary calls it. Pin that the first statement of `fn main` is the
//! call, so deleting or moving it (after a thread could exist) fails a test.
//! Phase 7c: the one allowed predecessor is `imu_reader_or_continue();`.

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
    // Phase 7c: the IMU reader dispatch may precede it (it spawns no
    // thread and never returns on the reader path); nothing else may.
    let mut first = stmts.next().expect("fn main has a body");
    if first == "imu_reader_or_continue();" {
        first = stmts.next().expect("fn main continues");
    }
    assert_eq!(first, "xorgate_agent::alloc::tune();");
}
