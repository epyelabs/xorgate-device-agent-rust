//! F-P3-MEM regression pin (`plans/device-agent-js-to-rust/phase-3/evidence/
//! f-p3-mem.md`): after `alloc::tune()`, a freed fragment-sized buffer is
//! returned to the kernel instead of being kept in a malloc arena.
//!
//! The uploader's pattern: a fresh blocking-pool thread reads a ~7 MB
//! fragment into one exact-size buffer, a runtime worker drops it after the
//! PUT. Without the tuning, glibc's dynamic mmap threshold rises to the first
//! freed buffer's size, every later (smaller) buffer comes from an arena, and
//! the freed bytes stay there (`fordblks` grows by a fragment) — this test
//! then fails. Its own test binary: the allocator state is process-global.

#![cfg(all(target_os = "linux", target_env = "gnu"))]
#![allow(unsafe_code)]

fn free_bytes_in_arenas() -> usize {
    // SAFETY: mallinfo2 only reads glibc's malloc statistics.
    unsafe { libc::mallinfo2() }.fordblks
}

#[test]
fn freed_fragment_buffers_are_not_retained_in_malloc_arenas() {
    assert!(
        xorgate_agent::alloc::tune(),
        "mallopt(M_MMAP_THRESHOLD) refused"
    );
    let before = free_bytes_in_arenas();
    let mut worst = 0usize;
    // Largest first: the shape that defeats the dynamic threshold.
    for i in 0..12usize {
        let size = 7_300_000 - i * 97_000;
        let buf = std::thread::spawn(move || {
            let mut v = vec![0u8; size];
            for b in v.iter_mut().step_by(4096) {
                *b = 1; // touch every page, as a file read does
            }
            v
        })
        .join()
        .unwrap();
        std::thread::spawn(move || drop(buf)).join().unwrap();
        worst = worst.max(free_bytes_in_arenas().saturating_sub(before));
    }
    assert!(
        worst < 2 * 1024 * 1024,
        "a freed fragment buffer stayed in a malloc arena: +{worst} B free-but-resident"
    );
}
