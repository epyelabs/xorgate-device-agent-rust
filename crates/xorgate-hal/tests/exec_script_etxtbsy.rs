//! The "write a script, then exec it" race of a multi-threaded test binary
//! (the `obd_io::tests::unbond_spawns_at_call_time` flake): a fork on another
//! thread while the script's write fd is open hands the child that fd until
//! the child execs, and an `execve` of the script in that window fails with
//! ETXTBSY. Fork load here = threads spawning `/bin/true` in a loop.
//!
//! The pinning test proves `test_support::write_executable` is immune; the
//! `#[ignore]`d control (`cargo test --test exec_script_etxtbsy -- --ignored`)
//! demonstrates the hazard of the in-process `fs::write` + `chmod` pattern
//! the tests used before.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use xorgate_hal::test_support::write_executable;

const FORKERS: usize = 8;
const WRITERS: usize = 4;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-etxtbsy-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `WRITERS` threads × `per_writer` scripts, each written with `write` then
/// exec'd at once, under `FORKERS` threads of fork load. Returns
/// (successful execs, ETXTBSY failures); any other failure panics.
fn race(tag: &str, per_writer: usize, write: fn(&Path, &str)) -> (usize, usize) {
    let dir = scratch(tag);
    let stop = Arc::new(AtomicBool::new(false));
    let forkers: Vec<_> = (0..FORKERS)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("/bin/true").status();
                }
            })
        })
        .collect();
    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let (mut ok, mut busy) = (0, 0);
                for i in 0..per_writer {
                    let p = dir.join(format!("s{w}-{i}"));
                    write(&p, "#!/bin/sh\nexit 0\n");
                    match Command::new(&p).status() {
                        Ok(s) if s.success() => ok += 1,
                        Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => busy += 1,
                        other => panic!("{}: {other:?}", p.display()),
                    }
                }
                (ok, busy)
            })
        })
        .collect();
    let (mut ok, mut busy) = (0, 0);
    for w in writers {
        let (o, b) = w.join().unwrap();
        ok += o;
        busy += b;
    }
    stop.store(true, Ordering::Relaxed);
    for f in forkers {
        f.join().unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
    (ok, busy)
}

#[test]
fn write_executable_never_hits_etxtbsy_under_fork_load() {
    let (ok, busy) = race("helper", 150, |p, body| write_executable(p, body).unwrap());
    assert_eq!((ok, busy), (WRITERS * 150, 0));
}

/// Control (not part of the gate: it asserts the hazard, which is timing
/// dependent): the old in-process pattern loses some execs to ETXTBSY.
#[test]
#[ignore = "demonstrates the race; timing dependent"]
fn in_process_write_then_exec_hits_etxtbsy_under_fork_load() {
    let (ok, busy) = race("inproc", 2000, |p, body| {
        std::fs::write(p, body).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    });
    eprintln!("in-process pattern: {ok} ok, {busy} ETXTBSY");
    assert!(busy > 0, "no ETXTBSY in {ok} execs");
}
