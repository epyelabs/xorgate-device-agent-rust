//! Probe for `clippy.toml`'s `disallowed-methods` (F-P6-ENV, review R-P6F-3).
//!
//! NOT part of any crate: `scripts/tests/clippy-ban-probe.sh` copies the tree,
//! mounts this file into the scratch copy's `xorgate-hal` lib, runs clippy and
//! checks that EVERY `// PROBE` line below is flagged as a disallowed method,
//! and that the `// CONTROL` line (the sanctioned helper) is not. It is never
//! executed; it only has to type-check.
#![allow(dead_code, deprecated, unsafe_code, unused_must_use, unused_variables, unused_imports)]

use std::ffi::CStr;
use std::os::fd::AsFd;

fn std_and_tokio() {
    std::process::Command::new("true"); // PROBE std::process::Command::new
    tokio::process::Command::new("true"); // PROBE tokio::process::Command::new
    crate::child::std_command("true"); // CONTROL xorgate_hal::child::std_command
}

fn raw_libc() {
    let p: &CStr = c"/bin/true";
    let argv: [*const libc::c_char; 2] = [p.as_ptr(), std::ptr::null()];
    let margv: [*mut libc::c_char; 1] = [std::ptr::null_mut()];
    let envp: [*const libc::c_char; 1] = [std::ptr::null()];
    let mut pid: libc::pid_t = 0;
    unsafe {
        libc::fork(); // PROBE libc::fork
        libc::vfork(); // PROBE libc::vfork
        libc::execv(p.as_ptr(), argv.as_ptr()); // PROBE libc::execv
        libc::execve(p.as_ptr(), argv.as_ptr(), envp.as_ptr()); // PROBE libc::execve
        libc::execvp(p.as_ptr(), argv.as_ptr()); // PROBE libc::execvp
        libc::execvpe(p.as_ptr(), argv.as_ptr(), envp.as_ptr()); // PROBE libc::execvpe
        libc::fexecve(3, argv.as_ptr(), envp.as_ptr()); // PROBE libc::fexecve
        libc::posix_spawn(&mut pid, p.as_ptr(), std::ptr::null(), std::ptr::null(), margv.as_ptr(), margv.as_ptr()); // PROBE libc::posix_spawn
        libc::posix_spawnp(&mut pid, p.as_ptr(), std::ptr::null(), std::ptr::null(), margv.as_ptr(), margv.as_ptr()); // PROBE libc::posix_spawnp
        libc::system(p.as_ptr()); // PROBE libc::system
        libc::popen(p.as_ptr(), c"r".as_ptr()); // PROBE libc::popen
    }
}

fn raw_nix(fd: std::fs::File) {
    let p: &CStr = c"/bin/true";
    unsafe {
        nix::unistd::fork(); // PROBE nix::unistd::fork
    }
    nix::unistd::execv(p, &[p]); // PROBE nix::unistd::execv
    nix::unistd::execve(p, &[p], &[p]); // PROBE nix::unistd::execve
    nix::unistd::execvp(p, &[p]); // PROBE nix::unistd::execvp
    nix::unistd::execvpe(p, &[p], &[p]); // PROBE nix::unistd::execvpe
    nix::unistd::fexecve(fd.as_fd(), &[p], &[p]); // PROBE nix::unistd::fexecve
    nix::unistd::execveat(fd.as_fd(), p, &[p], &[p], nix::fcntl::AtFlags::empty()); // PROBE nix::unistd::execveat
}
