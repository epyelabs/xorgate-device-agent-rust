//! F-P3-MEM review S1: `alloc_tune.rs` proves `alloc::tune()` works, but not
//! that the binary calls it. Pin that the first statement of `fn main` is the
//! call, so deleting or moving it (after a thread could exist) fails a test.

#[test]
fn main_calls_alloc_tune_first() {
    let src = include_str!("../src/main.rs");
    let body = src
        .split_once("fn main() {")
        .expect("main.rs has `fn main() {`")
        .1;
    let first = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("//"))
        .expect("fn main has a body");
    assert_eq!(first, "xorgate_agent::alloc::tune();");
}
