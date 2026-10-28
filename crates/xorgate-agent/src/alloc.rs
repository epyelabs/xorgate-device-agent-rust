//! glibc malloc tuning, applied once at process start (finding F-P3-MEM,
//! `plans/device-agent-js-to-rust/phase-3/evidence/f-p3-mem.md`).
//!
//! The uploader reads each closed ~7 MB video fragment into one buffer
//! (`tokio::fs::read` = an exact-size `Vec`, allocated on a blocking-pool
//! thread, so in that thread's malloc arena) and frees it after the PUT.
//! glibc's *dynamic* mmap threshold makes this retain memory: the first
//! such buffer is mmapped, its `free` raises `M_MMAP_THRESHOLD` to ~7 MB and
//! `M_TRIM_THRESHOLD` to twice that, so every later fragment buffer comes
//! from a per-thread arena and, once freed, stays resident (a non-main
//! arena's top is only trimmed above the raised trim threshold, and
//! `malloc_trim` does not trim non-main tops). The live heap stays ~1 MB,
//! but RSS ratchets up one fragment at a time (bench soak: 30 → 80 MiB in
//! ~7 MiB steps over an hour).
//!
//! Setting `M_MMAP_THRESHOLD` explicitly turns the dynamic adjustment off
//! (glibc `mallopt`: "disables dynamic adjustment"), keeping the trim
//! threshold at its 128 KiB default: buffers ≥ 1 MiB are mmapped and
//! returned to the kernel on free. No behaviour change, one mmap/munmap per
//! fragment.

/// Allocations at or above this size are served by mmap (and unmapped on
/// free). Well below the smallest video fragment, above the agent's hot
/// small/medium buffers (64 KiB body chunks, TLS records, JSON payloads).
pub const MMAP_THRESHOLD_BYTES: i32 = 1024 * 1024;

/// Apply the tuning; `true` when glibc accepted it. A no-op (`false`) on
/// non-glibc targets. Call before any thread is spawned.
#[allow(unsafe_code)]
pub fn tune() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallopt only updates glibc's malloc parameters; it takes
        // the arena locks itself and has no memory-safety preconditions.
        unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD_BYTES) == 1 }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        false
    }
}
