//! TEST-ONLY transport (cargo feature `imu-test-bus`, enabled by the agent's
//! own dev-dependency, so ONLY `cargo test` builds carry it; a `cargo build`
//! / release binary has no such code): when `XORGATE_IMU_TEST_BUS=<unix
//! socket path>` is set, the reader's bus ops, its `time.monotonic()` and its
//! INIT sleeps go to a test server over that socket instead of `/dev/i2c-1`
//! and `CLOCK_MONOTONIC` (no BNO085 and no `i2c-stub` in a container). The
//! loop's interval wait stays REAL (`sigtimedwait`), and so do the argument
//! parsing, `PR_SET_NAME`, the SIGINT scheme, the stdout sink, the teardown
//! order and the exit codes: `tests/imu_reader.rs` drives the real
//! `xorgate-rs __imu-reader` process through every C4 transcript.
//!
//! One text line per request and per reply:
//! `open <path>` → `ok` | `err open <errno>`;
//! `write <addr> <hex|->` → `ok <n>` | `eagain` | `err sel|xfer <errno>`;
//! `read <addr> <len>` → `data <hex|->` | `eagain` | `err sel|xfer <errno>`;
//! `close` → `ok`; `mono` → `mono <f64 bits>`; `sleep <f64 bits>` → `ok`.
//! errno texts come from the real [`xorgate_hal::i2c_dev::strerror`].

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;

use xorgate_core::bno08x_client::ReaderArgs;
use xorgate_core::imu_io::{
    I2cError, I2cOps, I2cStage, ImuClock, IntervalWait, ReadOutcome, WriteOutcome,
};
use xorgate_hal::i2c_dev::strerror;

use super::{run_reader, ReaderBus, RealClock};

/// The socket path the test sets.
pub const ENV: &str = "XORGATE_IMU_TEST_BUS";

struct Conn {
    r: BufReader<UnixStream>,
    w: UnixStream,
}

impl Conn {
    fn call(&mut self, req: &str) -> String {
        writeln!(self.w, "{req}").expect("test bus: request");
        let mut line = String::new();
        let n = self.r.read_line(&mut line).expect("test bus: reply");
        assert!(
            n > 0,
            "test bus: the server closed the socket after {req:?}"
        );
        line.trim_end().to_string()
    }
}

type Shared = Rc<RefCell<Conn>>;

fn hex(b: &[u8]) -> String {
    if b.is_empty() {
        return "-".into();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("test bus: hex"))
        .collect()
}

fn error(stage: &str, errno: &str) -> I2cError {
    let errno: i32 = errno.parse().expect("test bus: errno");
    I2cError {
        stage: match stage {
            "open" => I2cStage::Open,
            "sel" => I2cStage::SelectDevice,
            _ => I2cStage::Transfer,
        },
        errno,
        strerror: strerror(errno),
    }
}

struct TestBus {
    conn: Shared,
    open: bool,
}

impl I2cOps for TestBus {
    fn open(&mut self, path: &str) -> Result<(), I2cError> {
        let r = self.conn.borrow_mut().call(&format!("open {path}"));
        match r.split(' ').collect::<Vec<_>>().as_slice() {
            ["ok"] => {
                self.open = true;
                Ok(())
            }
            ["err", st, e] => Err(error(st, e)),
            _ => panic!("test bus: bad reply {r:?}"),
        }
    }

    fn write(&mut self, address: u8, bytes: &[u8]) -> Result<WriteOutcome, I2cError> {
        let r = self
            .conn
            .borrow_mut()
            .call(&format!("write {address} {}", hex(bytes)));
        match r.split(' ').collect::<Vec<_>>().as_slice() {
            ["ok", n] => Ok(WriteOutcome::Written(n.parse().expect("count"))),
            ["eagain"] => Ok(WriteOutcome::WouldBlock),
            ["err", st, e] => Err(error(st, e)),
            _ => panic!("test bus: bad reply {r:?}"),
        }
    }

    fn read(&mut self, address: u8, len: usize) -> Result<ReadOutcome, I2cError> {
        let r = self
            .conn
            .borrow_mut()
            .call(&format!("read {address} {len}"));
        match r.split(' ').collect::<Vec<_>>().as_slice() {
            ["data", h] => Ok(ReadOutcome::Data(unhex(h))),
            ["eagain"] => Ok(ReadOutcome::WouldBlock),
            ["err", st, e] => Err(error(st, e)),
            _ => panic!("test bus: bad reply {r:?}"),
        }
    }
}

impl ReaderBus for TestBus {
    fn close_bus(&mut self) {
        if std::mem::take(&mut self.open) {
            let r = self.conn.borrow_mut().call("close");
            assert_eq!(r, "ok", "test bus: close");
        }
    }
}

struct TestClock {
    conn: Shared,
    real: RealClock,
}

impl ImuClock for TestClock {
    fn monotonic(&mut self) -> f64 {
        let r = self.conn.borrow_mut().call("mono");
        let bits = r
            .strip_prefix("mono ")
            .and_then(|b| b.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("test bus: bad reply {r:?}"));
        f64::from_bits(bits)
    }

    fn sleep(&mut self, secs: f64) {
        let r = self
            .conn
            .borrow_mut()
            .call(&format!("sleep {}", secs.to_bits()));
        assert_eq!(r, "ok", "test bus: sleep");
    }

    /// REAL: the SIGINT scheme under test.
    fn interval_wait(&mut self, secs: f64) -> IntervalWait {
        self.real.interval_wait(secs)
    }
}

/// The reader over the test server at `path`.
pub fn run(path: &OsStr, args: &ReaderArgs) -> i32 {
    let w = UnixStream::connect(path).expect("test bus: connect");
    let r = BufReader::new(w.try_clone().expect("test bus: clone"));
    let conn: Shared = Rc::new(RefCell::new(Conn { r, w }));
    let mut bus = TestBus {
        conn: conn.clone(),
        open: false,
    };
    let mut clock = TestClock {
        conn,
        real: RealClock,
    };
    run_reader(&mut bus, &mut clock, args)
}
