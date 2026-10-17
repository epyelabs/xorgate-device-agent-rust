//! A directory watcher with Node `fs.watch(dir, cb)` semantics on Linux —
//! the capture supervisor's sweep trigger (`src/recording/video-capture.ts`
//! `watchSession :902-911` @ 89b5c64).
//!
//! libuv implements `fs.watch` with inotify, mask `IN_ATTRIB | IN_CREATE |
//! IN_MODIFY | IN_DELETE | IN_DELETE_SELF | IN_MOVE_SELF | IN_MOVED_FROM |
//! IN_MOVED_TO`, and invokes the callback ONCE PER EVENT read; the same mask
//! is used here and one `()` is delivered per event, so the supervisor sweeps
//! exactly as often as the TS does. No crate: `libc` inotify on a small
//! dedicated thread that `poll(2)`s the fd (200 ms ticks, so dropping the
//! watcher ends the thread promptly). If the watch cannot be set up the
//! caller runs on its 10 s poll sweep alone (`run.watcher = null; // poll
//! still covers us`), which is authoritative either way.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

/// libuv's `uv_fs_event_start` inotify mask (`src/unix/linux.c`).
const UV_MASK: u32 = libc::IN_ATTRIB
    | libc::IN_CREATE
    | libc::IN_MODIFY
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO;

/// A live watch; dropping it stops the watcher thread and closes the fd.
#[derive(Debug)]
pub struct DirWatcher {
    stop: Arc<AtomicBool>,
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// `fs.watch(dir)`: one `()` per inotify event on `dir`. Errors (missing dir,
/// inotify limits) are returned so the caller can fall back to polling.
pub fn watch_dir(dir: &Path) -> io::Result<(DirWatcher, mpsc::UnboundedReceiver<()>)> {
    // SAFETY: plain syscalls on an fd we own; every error path closes it.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut cpath = dir.as_os_str().as_bytes().to_vec();
    cpath.push(0);
    // SAFETY: `cpath` is NUL-terminated and outlives the call.
    let wd = unsafe { libc::inotify_add_watch(fd, cpath.as_ptr().cast(), UV_MASK) };
    if wd < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: closing the fd we just opened.
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let spawned = std::thread::Builder::new()
        .name("fs-watch".into())
        .spawn(move || {
            pump(fd, wd, &flag, &tx);
            // SAFETY: the thread owns the fd from here on.
            unsafe { libc::close(fd) };
        });
    if let Err(e) = spawned {
        // SAFETY: the thread never started, so the fd is still ours.
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok((DirWatcher { stop }, rx))
}

/// Read events until stopped (or the receiver is gone); one send per event
/// on our watch descriptor.
fn pump(fd: i32, wd: i32, stop: &AtomicBool, tx: &mpsc::UnboundedSender<()>) {
    let mut buf = vec![0u8; 64 * 1024];
    let header = std::mem::size_of::<libc::inotify_event>();
    while !stop.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd we own.
        let rc = unsafe { libc::poll(&mut pfd, 1, 200) };
        if rc <= 0 {
            continue; // timeout or EINTR: re-check the stop flag
        }
        // SAFETY: reading into our own buffer of the stated length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            continue; // EAGAIN (spurious wakeup) or EINTR
        }
        let n = n as usize;
        let mut off = 0;
        while off + header <= n {
            // SAFETY: the kernel wrote a whole inotify_event header at `off`;
            // read_unaligned because the buffer is a byte array.
            let ev: libc::inotify_event =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off).cast()) };
            off += header + ev.len as usize;
            if ev.wd == wd && tx.send(()).is_err() {
                return; // nobody listens any more
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn create_and_modify_are_events_and_drop_stops() {
        let dir = std::env::temp_dir().join(format!("xg-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (w, mut rx) = watch_dir(&dir).unwrap();
        std::fs::write(dir.join("00000.mp4"), b"abc").unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert_eq!(got.unwrap(), Some(()));
        drop(w);
        // The thread exits within a poll tick and drops the sender.
        let end = tokio::time::timeout(Duration::from_secs(5), async {
            while rx.recv().await.is_some() {}
        })
        .await;
        assert!(end.is_ok(), "watcher thread did not stop");
        assert!(watch_dir(&dir.join("absent")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
