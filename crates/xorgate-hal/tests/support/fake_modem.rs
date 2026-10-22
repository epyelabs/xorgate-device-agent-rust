//! The scripted SIM7600 of `test/sms-modem.test.ts:20-103` @ 89b5c64 (the
//! TS `FakeModem`), as a pure engine: one command in → the reply lines out,
//! the store state and the command log kept. Shared by the `at_port` unit
//! tests (an in-memory `AtTransport`) and `tests/at_port_pty.rs` (the far
//! end of a pty pair). Plus the dump drivers' marked extensions:
//! `answers` (a scripted reply for an exact command), and for the Phase 5b
//! clear (`test/sms-modem.test.ts` @ 09da38d): `cmgdError` (answer the
//! delete-all on that store with this final instead of deleting), the
//! `deletes` record, `writeError` (a write of that command fails) and the
//! RIG RULE `allowClear` (phase-5b §6.7): without it a delete is a THROW —
//! the command is logged, nothing is deleted and the transport write fails —
//! so a read path that ever deletes fails its test.
//!
//! Semantics (phase-1/evidence/wp5.md §AT-transcript shapes): defaults
//! `cmgf 0`, `mem1 "SR"`; `AT+CPMS?` → `+CPMS: "<mem1>",u,t,"ME",u,t,"ME",u,t`
//! (SR total 0); `AT+CPMS="X"` → `+CPMS: u,t,23,23,23,23`; `AT+CMGRO=i` →
//! `+CMGRO: <stat>,"",<len>` + hex + `OK`, or `ERROR` for an empty slot (and
//! an error if sent in text mode); `urcs` before every reply; `silent` /
//! `hang` → no reply at all.

#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Slot {
    pub stat: i64,
    pub length: i64,
    pub pdu: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Store {
    pub total: i64,
    pub slots: BTreeMap<String, Slot>,
}

/// The serialisable `FakeOpts` the dump driver records (`fake` of a case).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FakeSpec {
    #[serde(default)]
    pub stores: BTreeMap<String, Store>,
    pub cmgf: Option<u8>,
    pub mem1: Option<String>,
    #[serde(default)]
    pub silent: bool,
    pub cpms_error: Option<String>,
    #[serde(default)]
    pub urcs: Vec<String>,
    #[serde(default)]
    pub hang: Vec<String>,
    #[serde(default)]
    pub answers: BTreeMap<String, Vec<String>>,
    pub tick_ms: Option<i64>,
    pub t0: Option<i64>,
    #[serde(default)]
    pub cmgd_error: BTreeMap<String, String>,
    #[serde(default)]
    pub allow_clear: bool,
    #[serde(default)]
    pub write_error: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct FakeModem {
    pub spec: FakeSpec,
    pub log: Vec<String>,
    pub cmgf: u8,
    pub mem1: String,
    /// Store selected (mem1) at each delete, in order.
    pub deletes: Vec<String>,
}

/// What one command produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Nothing at all (silent / hang).
    None,
    Lines(Vec<String>),
    /// The TS fake throws from `write` (`CMGRO sent in text mode`).
    Throw(String),
}

impl FakeModem {
    pub fn new(spec: FakeSpec) -> Self {
        Self {
            cmgf: spec.cmgf.unwrap_or(0),
            mem1: spec.mem1.clone().unwrap_or_else(|| "SR".to_string()),
            spec,
            log: Vec::new(),
            deletes: Vec::new(),
        }
    }

    fn store(&self, mem: &str) -> Option<&Store> {
        self.spec.stores.get(mem)
    }
    fn used(&self, mem: &str) -> usize {
        self.store(mem).map(|s| s.slots.len()).unwrap_or(0)
    }
    fn total(&self, mem: &str) -> i64 {
        if mem == "SR" {
            0
        } else {
            self.store(mem).map(|s| s.total).unwrap_or(0)
        }
    }
    fn triple(&self, mem: &str) -> String {
        format!("\"{mem}\",{},{}", self.used(mem), self.total(mem))
    }

    /// `write(data)` (`sms-modem.test.ts:75-102`), `data` without the `\r`.
    pub fn command(&mut self, cmd: &str) -> Reply {
        self.log.push(cmd.to_string());
        if let Some(e) = self.spec.write_error.get(cmd) {
            return Reply::Throw(e.clone());
        }
        if self.spec.silent || self.spec.hang.iter().any(|h| h == cmd) {
            return Reply::None;
        }
        let mut out: Vec<String> = self.spec.urcs.clone();
        if let Some(a) = self.spec.answers.get(cmd) {
            out.extend(a.iter().cloned());
        } else if cmd == "ATE0" {
            out.extend(["ATE0".to_string(), "OK".to_string()]);
        } else if cmd == "AT+CMGF?" {
            out.extend([format!("+CMGF: {}", self.cmgf), "OK".to_string()]);
        } else if let Some(d) = cmd
            .strip_prefix("AT+CMGF=")
            .filter(|d| d.len() == 1 && d.as_bytes()[0].is_ascii_digit())
        {
            self.cmgf = d.parse().unwrap();
            out.push("OK".to_string());
        } else if cmd == "AT+CPMS?" {
            match &self.spec.cpms_error {
                Some(e) => out.push(e.clone()),
                None => out.extend([
                    format!(
                        "+CPMS: {},{},{}",
                        self.triple(&self.mem1.clone()),
                        self.triple("ME"),
                        self.triple("ME")
                    ),
                    "OK".to_string(),
                ]),
            }
        } else if let Some(mem) = cmd
            .strip_prefix("AT+CPMS=\"")
            .and_then(|r| r.strip_suffix('"'))
            .filter(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            self.mem1 = mem.to_string();
            out.extend([
                format!("+CPMS: {},{},23,23,23,23", self.used(mem), self.total(mem)),
                "OK".to_string(),
            ]);
        } else if let Some(i) = cmd
            .strip_prefix("AT+CMGRO=")
            .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        {
            if self.cmgf != 0 {
                return Reply::Throw("CMGRO sent in text mode".to_string());
            }
            let n: u64 = i.parse().unwrap();
            match self
                .store(&self.mem1)
                .and_then(|s| s.slots.get(&n.to_string()))
            {
                Some(slot) => out.extend([
                    format!("+CMGRO: {},\"\",{}", slot.stat, slot.length),
                    slot.pdu.clone(),
                    "OK".to_string(),
                ]),
                None => out.push("ERROR".to_string()),
            }
        } else if cmd == "AT+CMGD=0,4" {
            if !self.spec.allow_clear {
                return Reply::Throw("rig: delete without allowClear".to_string());
            }
            self.deletes.push(self.mem1.clone());
            if let Some(e) = self.spec.cmgd_error.get(&self.mem1) {
                out.push(e.clone());
            } else {
                if let Some(st) = self.spec.stores.get_mut(&self.mem1) {
                    st.slots.clear();
                }
                out.push("OK".to_string());
            }
        } else {
            out.push("ERROR".to_string());
        }
        Reply::Lines(out)
    }
}

// ---------------------------------------------------------------------------
// In-memory transport + test clock (the TS FakeModem IS the transport)
// ---------------------------------------------------------------------------

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use xorgate_core::clock::Clock;
use xorgate_hal::at_port::{AtTransport, BoxFut};

/// The TS `now: () => t` of the budget test: wall = mono = `t`.
#[derive(Debug, Default)]
pub struct TestClock {
    pub t: AtomicI64,
}

impl TestClock {
    pub fn new(t: i64) -> Arc<Self> {
        Arc::new(Self {
            t: AtomicI64::new(t),
        })
    }
    pub fn advance(&self, ms: i64) {
        self.t.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn wall_ms(&self) -> i64 {
        self.t.load(Ordering::SeqCst)
    }
    fn mono_ms(&self) -> u64 {
        self.t.load(Ordering::SeqCst) as u64
    }
}

/// A [`FakeModem`] as an [`AtTransport`]: each reply is queued as ONE chunk
/// (`queueMicrotask(() => for (l of out) cb(l))` delivers them together);
/// `read_chunk` with nothing queued never resolves (cancel-safe).
pub struct MemTransport {
    pub modem: Arc<Mutex<FakeModem>>,
    pub closed: Arc<AtomicBool>,
    queue: VecDeque<Vec<u8>>,
    /// `onCommand: () => (t += tickMs)`.
    tick: Option<(Arc<TestClock>, i64)>,
}

impl MemTransport {
    pub fn new(modem: Arc<Mutex<FakeModem>>, tick: Option<(Arc<TestClock>, i64)>) -> Self {
        Self {
            modem,
            closed: Arc::new(AtomicBool::new(false)),
            queue: VecDeque::new(),
            tick,
        }
    }
}

impl AtTransport for MemTransport {
    fn write<'a>(&'a mut self, data: &'a [u8]) -> BoxFut<'a, io::Result<()>> {
        Box::pin(async move {
            let s = String::from_utf8_lossy(data);
            let cmd = s.strip_suffix('\r').unwrap_or(&s).to_string();
            let reply = {
                let mut m = self.modem.lock().unwrap();
                // `this.log.push(cmd); this.o.onCommand?.(cmd);` (the log is
                // pushed inside `command`; nothing observes the order).
                if let Some((c, ms)) = &self.tick {
                    c.advance(*ms);
                }
                m.command(&cmd)
            };
            match reply {
                Reply::None => Ok(()),
                Reply::Throw(msg) => Err(io::Error::other(msg)),
                Reply::Lines(lines) => {
                    let mut chunk = Vec::new();
                    for l in lines {
                        chunk.extend_from_slice(l.as_bytes());
                        chunk.extend_from_slice(b"\r\n");
                    }
                    self.queue.push_back(chunk);
                    Ok(())
                }
            }
        })
    }

    fn read_chunk(&mut self) -> BoxFut<'_, io::Result<Vec<u8>>> {
        Box::pin(async move {
            match self.queue.pop_front() {
                Some(c) => Ok(c),
                None => std::future::pending().await,
            }
        })
    }

    fn close(&mut self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            self.closed.store(true, Ordering::SeqCst);
        })
    }
}
