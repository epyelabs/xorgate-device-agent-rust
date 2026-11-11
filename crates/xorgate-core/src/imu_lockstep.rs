//! The lockstep runner (phase 7c WP2; plan §WP2 "the lockstep runner, both
//! directions", gate rows C4/C5/C10b): runs [`crate::bno08x_client`] against
//! an `*.imutx` transcript (imutx/1 JSON records, `tools/imu/imutx.py`).
//!
//! - **Bus:** every client op must be the transcript's NEXT op: an `open` of
//!   the same path; a `read(n)` = an `ioctl(I2C_SLAVE, addr & 0x7F)` record
//!   then a `read` record of length `n`; a `write(b)` = the ioctl then a
//!   `write` record with exactly `b`. The recorded result is returned (bytes,
//!   a short count, `EAGAIN` → `WouldBlock`, an errno → `I2cError` with
//!   glibc's text from the oracle-printed table). The first difference is a
//!   MISMATCH; the runner then goes "dead" so the client ends quickly.
//! - **Clock, two modes:**
//!   - **strict** (transcripts written by the oracle's model backend, meta
//!     `clock: virtual`): each `time.monotonic()` / `time.sleep()` of the
//!     Python helper is a `clock` record; the client must make the same call
//!     at the same point and gets the SAME IEEE double (`bits`); a sleep with
//!     `sigint: true` is where a real SIGINT landed (interval wait →
//!     `Interrupted`; an init sleep → death by SIGINT);
//!   - **next-op** (real `strace` transcripts: W0, B″): `monotonic()` returns
//!     the `t_us` of the next op record / 1e6, sleeps take no time. The
//!     oracle's replay backend uses the identical rule (`oracle.py
//!     ReplayBackend._monotonic`).
//! - **Signals:** a `signal` record reached before the next op/clock call
//!   kills the client there (default action); its stdout then ends at its
//!   last flush, as CPython's would.
//! - **stdout:** the client's chunks are kept with their position (the
//!   number of op/clock records consumed before them) and compared with the
//!   transcript's `stdout` records after the D80 filter
//!   ([`strip_d80`]).
//! - **`close`:** a `close` record on the device fd (Python's `SMBus.__del__`
//!   at interpreter teardown) is accepted only AFTER the client returned (a
//!   teardown close, reported); a `close` naming another fd is ignored.
//!
//! Pure (no I/O): callers parse the files and pass the records.

use std::cell::RefCell;
use std::collections::BTreeMap;

use serde_json::Value;

use crate::bno08x_client::{self, ImuSink, ReaderArgs, ReaderExit, StdoutClosed};
use crate::imu_io::{
    I2cError, I2cOps, I2cStage, ImuClock, IntervalWait, ReadOutcome, WriteOutcome, I2C_SLAVE,
};

/// errno name → (number, glibc `strerror`), as the oracle image printed it
/// (`fixtures/imu_oracle/errno.json`, `oracle.py errno-table`).
#[derive(Debug, Clone, Default)]
pub struct ErrnoTable(pub BTreeMap<String, (i32, String)>);

impl ErrnoTable {
    /// From the `errno.json` document (`{"errno": {"EIO": [5, "Input/output error"], …}}`).
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let mut t = BTreeMap::new();
        let obj = v
            .get("errno")
            .and_then(Value::as_object)
            .ok_or("errno table: no `errno` object")?;
        for (name, pair) in obj {
            let num = pair.get(0).and_then(Value::as_i64).ok_or("errno number")?;
            let text = pair.get(1).and_then(Value::as_str).ok_or("errno text")?;
            t.insert(name.clone(), (num as i32, text.to_string()));
        }
        Ok(Self(t))
    }

    fn get(&self, name: &str) -> Option<(i32, String)> {
        self.0.get(name).cloned()
    }
}

/// What the run produced.
#[derive(Debug, Clone, PartialEq)]
pub struct LockstepRun {
    /// How the client returned (`None` when it was killed).
    pub exit: Option<ReaderExit>,
    /// The signal that killed it (`SIGTERM`, `SIGINT`, …).
    pub killed: Option<String>,
    /// The first divergence from the transcript.
    pub mismatch: Option<String>,
    /// Every stdout chunk the client wrote, with its position.
    pub chunks: Vec<(usize, String)>,
    /// op/clock records consumed.
    pub consumed: usize,
    /// A device `close` record followed the client's return.
    pub teardown_close: bool,
}

impl LockstepRun {
    /// All stdout bytes.
    pub fn stdout(&self) -> String {
        self.chunks.iter().map(|(_, c)| c.as_str()).collect()
    }

    /// stdout as (position, line) pairs.
    pub fn lines(&self) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for (pos, chunk) in &self.chunks {
            for line in chunk.split_terminator('\n') {
                out.push((*pos, line.to_string()));
            }
        }
        out
    }
}

/// Kinds of transcript records that are client events.
fn is_bus(op: &str) -> bool {
    matches!(op, "open" | "ioctl" | "read" | "write")
}

struct State<'r> {
    recs: &'r [Value],
    errno: &'r ErrnoTable,
    strict: bool,
    dev_fd: Option<i64>,
    i: usize,
    consumed: usize,
    end_us: i64,
    past_end: u32,
    dead: bool,
    dead_clock: f64,
    killed: Option<String>,
    mismatch: Option<String>,
    chunks: Vec<(usize, String)>,
}

fn op_of(r: &Value) -> &str {
    r.get("op").and_then(Value::as_str).unwrap_or("")
}

impl State<'_> {
    fn is_foreign_close(&self, r: &Value) -> bool {
        op_of(r) == "close"
            && match (r.get("fd").and_then(Value::as_i64), self.dev_fd) {
                (Some(fd), Some(dev)) => fd != dev,
                _ => false,
            }
    }

    /// The next record that is not stdout/meta/a foreign close.
    fn peek(&mut self) -> Option<&Value> {
        while let Some(r) = self.recs.get(self.i) {
            let op = op_of(r);
            if op == "stdout" || op == "meta" || self.is_foreign_close(r) {
                self.i += 1;
                continue;
            }
            return Some(r);
        }
        None
    }

    fn take(&mut self) -> &Value {
        let r = &self.recs[self.i];
        self.i += 1;
        if op_of(r) != "signal" {
            self.consumed += 1;
        }
        if let Some(t) = r.get("t_us").and_then(Value::as_i64) {
            self.end_us = t + r.get("dur_us").and_then(Value::as_i64).unwrap_or(0);
        }
        r
    }

    fn diverge(&mut self, why: String) {
        if self.mismatch.is_none() && self.killed.is_none() {
            self.mismatch = Some(format!("record {}: {why}", self.i));
        }
        self.dead = true;
    }

    fn kill(&mut self, sig: &str) {
        self.killed = Some(sig.to_string());
        self.dead = true;
    }

    /// Before any event: a `signal` record kills here (SIGINT is handled by
    /// the sleep paths). Returns false when the client is (now) dead.
    fn alive_before_event(&mut self) -> bool {
        if self.dead {
            return false;
        }
        let sig = match self.peek() {
            Some(r) if op_of(r) == "signal" => r
                .get("sig")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            _ => return true,
        };
        if sig == "SIGINT" && !self.strict {
            // next-op mode: a SIGINT lands in whatever call comes; an interval
            // wait takes it (see interval_wait), any other call dies (D83).
            return true;
        }
        self.take();
        self.kill(&sig);
        false
    }

    fn dead_err() -> I2cError {
        I2cError {
            stage: I2cStage::Transfer,
            errno: 5,
            strerror: "Input/output error".into(),
        }
    }

    fn errno_of(&mut self, r: &Value, stage: I2cStage) -> Option<Result<I2cError, ()>> {
        let name = r.get("errno").and_then(Value::as_str)?;
        if name == "EAGAIN" && stage == I2cStage::Transfer {
            return Some(Err(()));
        }
        match self.errno.get(name) {
            Some((errno, strerror)) => Some(Ok(I2cError {
                stage,
                errno,
                strerror,
            })),
            None => {
                self.diverge(format!("errno {name} is not in the errno table"));
                Some(Ok(Self::dead_err()))
            }
        }
    }

    /// The `ioctl(I2C_SLAVE, addr)` half of a read/write.
    fn select(&mut self, address: u8) -> Result<(), I2cError> {
        if !self.alive_before_event() {
            return Err(Self::dead_err());
        }
        let ok = matches!(self.peek(), Some(r) if op_of(r) == "ioctl"
            && r.get("req").and_then(Value::as_u64) == Some(u64::from(I2C_SLAVE))
            && r.get("arg").and_then(Value::as_u64) == Some(u64::from(address)));
        if !ok {
            let got = self.peek().map(|r| r.to_string()).unwrap_or("<end>".into());
            self.diverge(format!(
                "client: ioctl(I2C_SLAVE, {address:#x}); transcript: {got}"
            ));
            return Err(Self::dead_err());
        }
        let r = self.take().clone();
        match self.errno_of(&r, I2cStage::SelectDevice) {
            Some(Ok(e)) => Err(e),
            _ => Ok(()),
        }
    }

    fn transfer(&mut self, op: &str, want: &dyn Fn(&Value) -> bool, what: String) -> Option<Value> {
        if !self.alive_before_event() {
            return None;
        }
        let ok = matches!(self.peek(), Some(r) if op_of(r) == op && want(r));
        if !ok {
            let got = self.peek().map(|r| r.to_string()).unwrap_or("<end>".into());
            self.diverge(format!("client: {what}; transcript: {got}"));
            return None;
        }
        let r = self.take().clone();
        if r.get("trunc").and_then(Value::as_bool) == Some(true) {
            self.diverge("the transcript truncated this op (strace -s)".into());
            return None;
        }
        Some(r)
    }

    fn next_op_time(&mut self) -> f64 {
        let next = self.recs[self.i..]
            .iter()
            .find(|r| {
                let op = op_of(r);
                (is_bus(op) || op == "close" || op == "signal" || op == "exit")
                    && !self.is_foreign_close(r)
            })
            .cloned();
        match next {
            Some(r) if is_bus(op_of(&r)) || op_of(&r) == "close" => {
                r.get("t_us").and_then(Value::as_i64).unwrap_or(0) as f64 / 1e6
            }
            _ => {
                self.past_end += 1;
                self.end_us as f64 / 1e6 + 1000.0 * f64::from(self.past_end)
            }
        }
    }

    /// A strict `clock` record of kind `fn_`; returns it or diverges.
    fn clock_rec(&mut self, fn_: &str, what: String) -> Option<Value> {
        let ok = matches!(self.peek(), Some(r) if op_of(r) == "clock"
            && r.get("fn").and_then(Value::as_str) == Some(fn_));
        if !ok {
            let got = self.peek().map(|r| r.to_string()).unwrap_or("<end>".into());
            self.diverge(format!("client: {what}; transcript: {got}"));
            return None;
        }
        Some(self.take().clone())
    }

    fn sleep_common(&mut self, secs: f64) -> Option<bool> {
        if !self.alive_before_event() {
            return None;
        }
        if !self.strict {
            if matches!(self.peek(), Some(r) if op_of(r) == "signal"
                && r.get("sig").and_then(Value::as_str) == Some("SIGINT"))
            {
                self.take();
                return Some(true);
            }
            return Some(false);
        }
        let r = self.clock_rec("sleep", format!("time.sleep({secs:?})"))?;
        if r.get("bits").and_then(Value::as_u64) != Some(secs.to_bits()) {
            self.diverge(format!("client slept {secs:?}; transcript: {r}"));
            return None;
        }
        let sigint = r.get("sigint").and_then(Value::as_bool) == Some(true);
        if sigint && matches!(self.peek(), Some(s) if op_of(s) == "signal") {
            self.take();
        }
        Some(sigint)
    }
}

/// One handle per trait; all three share the run's state.
struct Handle<'s, 'r>(&'s RefCell<State<'r>>);

impl I2cOps for Handle<'_, '_> {
    fn open(&mut self, path: &str) -> Result<(), I2cError> {
        let mut s = self.0.borrow_mut();
        let p = path.to_string();
        let Some(r) = s.transfer(
            "open",
            &|r| r.get("path").and_then(Value::as_str) == Some(p.as_str()),
            format!("open({path:?})"),
        ) else {
            return Err(State::dead_err());
        };
        if r.get("errno").and_then(Value::as_str).is_some() {
            if let Some(Ok(e)) = s.errno_of(&r, I2cStage::Open) {
                return Err(e);
            }
            return Err(State::dead_err());
        }
        if let Some(fd) = r.get("ret").and_then(Value::as_i64) {
            s.dev_fd = Some(fd);
        }
        Ok(())
    }

    fn write(&mut self, address: u8, bytes: &[u8]) -> Result<WriteOutcome, I2cError> {
        let mut s = self.0.borrow_mut();
        s.select(address)?;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let len = bytes.len() as u64;
        let h = hex.clone();
        let Some(r) = s.transfer(
            "write",
            &|r| {
                r.get("hex").and_then(Value::as_str).unwrap_or("") == h
                    && r.get("len").and_then(Value::as_u64) == Some(len)
            },
            format!("write({hex})"),
        ) else {
            return Err(State::dead_err());
        };
        match s.errno_of(&r, I2cStage::Transfer) {
            Some(Err(())) => Ok(WriteOutcome::WouldBlock),
            Some(Ok(e)) => Err(e),
            None => Ok(WriteOutcome::Written(
                r.get("ret").and_then(Value::as_u64).unwrap_or(len) as usize,
            )),
        }
    }

    fn read(&mut self, address: u8, len: usize) -> Result<ReadOutcome, I2cError> {
        let mut s = self.0.borrow_mut();
        s.select(address)?;
        let n = len as u64;
        let Some(r) = s.transfer(
            "read",
            &|r| r.get("len").and_then(Value::as_u64) == Some(n),
            format!("read({len})"),
        ) else {
            return Err(State::dead_err());
        };
        match s.errno_of(&r, I2cStage::Transfer) {
            Some(Err(())) => Ok(ReadOutcome::WouldBlock),
            Some(Ok(e)) => Err(e),
            None => {
                let hex = r.get("hex").and_then(Value::as_str).unwrap_or("");
                match decode_hex(hex) {
                    Some(b) => Ok(ReadOutcome::Data(b)),
                    None => {
                        s.diverge(format!("bad hex in {r}"));
                        Err(State::dead_err())
                    }
                }
            }
        }
    }
}

impl ImuClock for Handle<'_, '_> {
    fn monotonic(&mut self) -> f64 {
        let mut s = self.0.borrow_mut();
        if !s.alive_before_event() {
            s.dead_clock += 1000.0;
            return s.dead_clock;
        }
        if !s.strict {
            return s.next_op_time();
        }
        match s.clock_rec("monotonic", "time.monotonic()".into()) {
            Some(r) => match r.get("bits").and_then(Value::as_u64) {
                Some(bits) => f64::from_bits(bits),
                None => {
                    s.diverge(format!("monotonic record without bits: {r}"));
                    s.dead_clock
                }
            },
            None => {
                s.dead_clock += 1000.0;
                s.dead_clock
            }
        }
    }

    fn sleep(&mut self, secs: f64) {
        let mut s = self.0.borrow_mut();
        if s.sleep_common(secs) == Some(true) {
            // A SIGINT in an init-phase sleep: default disposition (D83).
            s.kill("SIGINT");
        }
    }

    fn interval_wait(&mut self, secs: f64) -> IntervalWait {
        let mut s = self.0.borrow_mut();
        match s.sleep_common(secs) {
            Some(true) | None => IntervalWait::Interrupted,
            Some(false) => IntervalWait::Elapsed,
        }
    }
}

impl ImuSink for Handle<'_, '_> {
    fn write_chunk(&mut self, chunk: &str) -> Result<(), StdoutClosed> {
        let mut s = self.0.borrow_mut();
        if !s.dead {
            let pos = s.consumed;
            s.chunks.push((pos, chunk.to_string()));
        }
        Ok(())
    }
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Whether a transcript uses the strict clock (it has `clock` records).
pub fn is_strict(recs: &[Value]) -> bool {
    recs.iter().any(|r| op_of(r) == "clock")
}

/// Run the client in lockstep with `recs`.
pub fn run(recs: &[Value], errno: &ErrnoTable, args: &ReaderArgs) -> LockstepRun {
    let state = RefCell::new(State {
        recs,
        errno,
        strict: is_strict(recs),
        dev_fd: None,
        i: 0,
        consumed: 0,
        end_us: 0,
        past_end: 0,
        dead: false,
        dead_clock: 0.0,
        killed: None,
        mismatch: None,
        chunks: Vec::new(),
    });
    let (mut bus, mut clock, mut sink) = (Handle(&state), Handle(&state), Handle(&state));
    let exit = bno08x_client::run(&mut bus, &mut clock, &mut sink, args);
    let mut s = state.into_inner();
    let exit = if s.killed.is_some() || s.mismatch.is_some() {
        None
    } else {
        Some(exit)
    };
    let mut teardown_close = false;
    if exit.is_some() {
        if let Some(r) = s.peek() {
            if op_of(r) == "close" {
                s.take();
                teardown_close = true;
            }
        }
    }
    // Whatever is left must be the exit record (or nothing).
    if s.mismatch.is_none() {
        let rest: Vec<String> = s.recs[s.i..]
            .iter()
            .filter(|r| {
                let op = op_of(r);
                op != "stdout" && op != "meta" && op != "exit" && !s.is_foreign_close(r)
            })
            .map(|r| op_of(r).to_string())
            .collect();
        if !rest.is_empty() {
            s.mismatch = Some(format!(
                "the client ended with {} transcript records left (first: {})",
                rest.len(),
                rest[0]
            ));
        }
    }
    LockstepRun {
        exit,
        killed: s.killed,
        mismatch: s.mismatch,
        chunks: s.chunks,
        consumed: s.consumed,
        teardown_close,
    }
}

const DUMP_HEAD: &str = "\t\t********** Packet *************";
const DUMP_TAIL: &str = "\t\t*******************************";

/// Drop the `print(packet)` dumps (`__init__.py:836`, D80) from stdout
/// lines: `""`, the head line, …, the tail line, `""` (as `oracle.py strip_d80`).
pub fn strip_d80<T: Clone>(lines: &[(T, String)]) -> Vec<(T, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].1.is_empty() && lines.get(i + 1).is_some_and(|l| l.1 == DUMP_HEAD) {
            let mut j = i + 2;
            while j < lines.len() && lines[j].1 != DUMP_TAIL {
                j += 1;
            }
            if lines.get(j + 1).is_some_and(|l| l.1.is_empty()) {
                i = j + 2;
                continue;
            }
        }
        out.push(lines[i].clone());
        i += 1;
    }
    out
}

/// The transcript's stdout as (position, line), positions counted like
/// [`LockstepRun::chunks`] (op/clock records before the line).
pub fn expected_lines(recs: &[Value]) -> Vec<(usize, String)> {
    let dev_fd = recs
        .iter()
        .find(|r| op_of(r) == "open")
        .and_then(|r| r.get("ret").and_then(Value::as_i64))
        .filter(|fd| *fd >= 0);
    let mut pos = 0;
    let mut out = Vec::new();
    for r in recs {
        match op_of(r) {
            "stdout" => out.push((
                pos,
                r.get("line")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )),
            "meta" | "exit" | "signal" => {}
            "close"
                if matches!((r.get("fd").and_then(Value::as_i64), dev_fd),
                    (Some(fd), Some(dev)) if fd != dev) => {}
            _ => pos += 1,
        }
    }
    out
}

/// The transcript's `exit` record as (code, signal).
pub fn expected_exit(recs: &[Value]) -> Option<(Option<i64>, Option<String>)> {
    recs.iter().rev().find(|r| op_of(r) == "exit").map(|r| {
        (
            r.get("code").and_then(Value::as_i64),
            r.get("signal").and_then(Value::as_str).map(str::to_string),
        )
    })
}

/// The C4/C5 verdict: every problem with `run` against `recs` (empty = PASS).
/// - no op/clock mismatch, nothing left over;
/// - stdout lines byte-equal to the transcript's, D80 dumps filtered, AND at
///   the same positions in the op stream;
/// - the exit: the transcript's `exit {code}` = the client's code, `exit
///   {signal}` = the signal that killed it. (No exit record: not compared.)
pub fn verdict(recs: &[Value], run: &LockstepRun) -> Vec<String> {
    let mut p = Vec::new();
    if let Some(m) = &run.mismatch {
        p.push(format!("op mismatch: {m}"));
    }
    let want: Vec<(usize, String)> = strip_d80(&expected_lines(recs));
    let got = run.lines();
    let want_text: Vec<&str> = want.iter().map(|(_, l)| l.as_str()).collect();
    let got_text: Vec<&str> = got.iter().map(|(_, l)| l.as_str()).collect();
    if want_text != got_text {
        let i = want_text
            .iter()
            .zip(&got_text)
            .position(|(a, b)| a != b)
            .unwrap_or(want_text.len().min(got_text.len()));
        p.push(format!(
            "stdout differs at line {i}: oracle {:?} vs rust {:?} ({} vs {} lines)",
            want_text.get(i),
            got_text.get(i),
            want_text.len(),
            got_text.len()
        ));
    } else if want != got {
        let i = want.iter().zip(&got).position(|(a, b)| a != b).unwrap_or(0);
        p.push(format!(
            "stdout line {i} at a different op position: oracle {} vs rust {}",
            want[i].0, got[i].0
        ));
    }
    if let Some((code, signal)) = expected_exit(recs) {
        let got = (run.exit.map(|e| i64::from(e.code())), run.killed.clone());
        if (code, signal.clone()) != got {
            p.push(format!(
                "exit: oracle code {code:?} signal {signal:?} vs rust code {:?} signal {:?}",
                got.0, got.1
            ));
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d80_filter_drops_exactly_the_dump_block() {
        let l = |s: &str| (0usize, s.to_string());
        let lines = vec![
            l(""),
            l(DUMP_HEAD),
            l("DBG::\t\t HEADER:"),
            l(""),
            l("DBG::\t\t Data:"),
            l(DUMP_TAIL),
            l(""),
            l("{\"type\": \"sample\"}"),
        ];
        assert_eq!(strip_d80(&lines), vec![l("{\"type\": \"sample\"}")]);
        let keep = vec![l(""), l("x")];
        assert_eq!(strip_d80(&keep), keep);
    }

    #[test]
    fn hex_decodes() {
        assert_eq!(decode_hex("00ff10"), Some(vec![0, 0xff, 0x10]));
        assert_eq!(decode_hex("0"), None);
        assert_eq!(decode_hex(""), Some(vec![]));
    }
}
