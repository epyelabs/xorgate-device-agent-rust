//! The IMU reader's process plumbing: its name, the SIGINT mask and the one
//! timed signal wait. The only `unsafe` of the reader is [`wait_sigint`]
//! (plan §Design 1, fix-check R6: nix 0.31 has no `sigtimedwait`).

use std::ffi::CStr;
use std::time::Duration;

use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};

/// `PR_SET_NAME`: `comm` (and so `ps`, `pgrep -x`) reads `xorgate-imu`.
pub fn set_process_name(name: &CStr) {
    let _ = nix::sys::prctl::set_name(name);
}

fn sigint_set() -> SigSet {
    let mut set = SigSet::empty();
    set.add(Signal::SIGINT);
    set
}

/// Block SIGINT for the (single) reader thread from the loop start on: a
/// SIGINT then stays pending until [`wait_sigint`] takes it.
pub fn block_sigint() {
    let _ = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&sigint_set()), None);
}

/// How one timed wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigWait {
    /// A SIGINT was (or already was) pending: taken.
    Sigint,
    /// The timeout passed (`EAGAIN`).
    Timeout,
    /// Another signal interrupted the wait (`EINTR`): wait out the rest.
    Eintr,
}

/// `sigtimedwait({SIGINT}, timeout)`: the loop's interval sleep. With SIGINT
/// blocked it returns at once when one is pending (also for a zero timeout).
#[allow(unsafe_code)]
pub fn wait_sigint(timeout: Duration) -> SigWait {
    let set = sigint_set();
    let ts = libc::timespec {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_nsec: timeout.subsec_nanos() as _,
    };
    // SAFETY: pointers to a live stack `sigset_t` (borrowed from `set`) and
    // `timespec`; a null `siginfo` (the kernel then writes nothing back).
    let rc = unsafe { libc::sigtimedwait(set.as_ref(), std::ptr::null_mut(), &ts) };
    if rc == libc::SIGINT {
        return SigWait::Sigint;
    }
    match nix::errno::Errno::last() {
        nix::errno::Errno::EINTR if rc < 0 => SigWait::Eintr,
        // EAGAIN (the timeout) — or EINVAL, which a timespec built from a
        // Duration cannot cause: never spin, end the wait.
        _ => SigWait::Timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Runs on its own thread: the mask and the pending SIGINT are this
    /// thread's (raise), never the test harness's other threads'.
    #[test]
    fn a_blocked_sigint_is_taken_by_the_wait_and_a_timeout_elapses() {
        std::thread::spawn(|| {
            block_sigint();
            let t = Instant::now();
            assert_eq!(wait_sigint(Duration::from_millis(30)), SigWait::Timeout);
            assert!(t.elapsed() >= Duration::from_millis(25));
            // raise(3) = pthread_kill(pthread_self()): pending on THIS thread
            nix::sys::signal::raise(Signal::SIGINT).unwrap();
            // pending before the wait: taken at once, even with a zero timeout
            assert_eq!(wait_sigint(Duration::ZERO), SigWait::Sigint);
            assert_eq!(wait_sigint(Duration::ZERO), SigWait::Timeout);
        })
        .join()
        .unwrap();
    }
}
