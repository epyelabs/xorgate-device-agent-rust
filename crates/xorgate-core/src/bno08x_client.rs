//! The Rust BNO085 reader's client state machine (phase 7c WP2; plan
//! `phase-7c-rust-imu.md` §Design 1, P2, P5): a line-by-line port of what
//! `python/bno085_read.py` (sha256 `8852852b…`) does through the bench's
//! Python stack — Blinka 9.1.0 `busio`/`generic_linux`, PureIO 1.1.11,
//! busdevice 5.2.17 and `adafruit_bno08x` 1.3.3 — down to the i2c-dev ops.
//!
//! Generic over the FROZEN seams of [`crate::imu_io`]: [`I2cOps`] (one
//! `read(2)`/`write(2)` per op, the `I2C_SLAVE` ioctl folded in) and
//! [`ImuClock`] (CPython's float `time.monotonic()` and `time.sleep()`), plus
//! [`ImuSink`] for stdout. Pure: no I/O, no clock, no signal API.
//!
//! Proven op-for-op, byte-for-byte against the UNMODIFIED helper on the real
//! stack (the WP2 oracle, `tools/imu/oracle/`) by C4
//! (`tests/imu_oracle_c4.rs`, 29 scenario rows), through the lockstep
//! transport of [`crate::imu_lockstep`].
//!
//! What it holds — the Python driver's state (plan P2, items 1-10):
//! - ONE persistent [`ShtpBuffer`] shared by send and receive
//!   (`_data_buffer`, replaced by a zeroed `bytearray(count)` above 512);
//! - the library's [`SequenceTable`] (`_sequence_number`), written by every
//!   received header, and since agent 0.2.2 a second one for the host's
//!   writes (the helper's `ShtpI2C._tx_seq`): each channel's writes are
//!   numbered 0, 1, 2… whatever the hub sends (a BNO086 answers a jump with
//!   an SHTP error list on channel 0);
//! - the persistent pending-slice list (`_packet_slices`, #49 carry-over:
//!   slices left by a failed batch are processed LAST after the next packet);
//! - the readings cache (`_readings`, cached samples, `0xFC` zero seeds);
//! - `_id_read`.
//!
//! The control flow, with the helper's line numbers:
//! 1. `busio.I2C(board.SCL, board.SDA)` (`bno085_read.py:56`): PureIO opens
//!    `/dev/i2c-1`; ENOENT becomes the bus-missing `ValueError`
//!    ([`BUS_MISSING_BCM2712_PROVISIONAL`], final once C4 row 22 passes),
//!    any other errno an `OSError` with the path.
//! 2. `BNO08X_I2C(i2c, address)` (`:57`): busdevice's probe (`writeto(addr,
//!    b"")`, on `OSError` a 1-byte read, then `ValueError("No I2C device at
//!    address: 0x4a")`); then `initialize()`: 3 attempts of soft reset
//!    (OUTSIDE the `try`) + `_check_id` (inside), 0.5 s back-off.
//! 3. `enable_feature(0x05)`, `enable_feature(0x04)` (`:58-59`): set-feature
//!    on channel 2, then `_process_available_packets(max_packets=10)` until
//!    the id is in the readings, 2 s budget.
//! 4. Any exception → `{"type": "error", "error": "init failed at 0x4a:
//!    <str(exc)>"}`, exit 3 (`:60-62`); else `ready` (`:64`).
//! 5. The loop (`:76-99`): drain, `quaternion` (captured after the FIRST
//!    drain), drain, `linear_acceleration`; an exception counts toward
//!    `int(2.0 / interval)` (= 40) CONSECUTIVE failures → `read failed Nx:`,
//!    exit 4; a success resets the count and emits a sample from the
//!    CACHED readings; each iteration ends in the interruptible interval
//!    wait (SIGINT → exit 0, D83).
//!
//! stdout is reproduced with CPython's buffering (the helper's stdout is a
//! pipe, so it is block-buffered): `print()`ed non-JSON lines (the two
//! `_data_ready` lines) wait in a buffer and go out in the SAME write as the
//! next `emit` (`print(..., flush=True)`), or at a normal exit, AFTER the bus
//! is closed ([`ReaderEnd::unflushed`], review-wp2 F4); a death by signal
//! loses them. The `print(packet)` dump of a failed batch is NOT
//! reproduced (D80).

use crate::imu_io::{
    select_device_arg, I2cError, I2cOps, ImuClock, IntervalWait, ReadOutcome, I2C_BUS_1_PATH,
};
use crate::py_fmt::{
    error_line, init_failed_text, no_i2c_device_text, read_failed_text, ready_line_with,
    sample_line, ExcKind, FirmwareId, PyExc, BUS_MISSING_BCM2712_PROVISIONAL, BYTEARRAY_INDEX,
    PRINT_END,
};
use crate::sh2::{self, Readings, ReportSlice};
use crate::shtp::{
    self, Packet, ReadBytes, SequenceTable, ShtpBuffer, CHANNEL_CONTROL, CHANNEL_EXE,
    CHANNEL_SHTP_COMMAND, HEADER_LEN,
};

/// `_PACKET_READ_TIMEOUT` (`__init__.py:120`): `_wait_for_packet`'s budget.
pub const PACKET_READ_TIMEOUT: f64 = 2.0;
/// `_FEATURE_ENABLE_TIMEOUT` (`__init__.py:121`).
pub const FEATURE_ENABLE_TIMEOUT: f64 = 2.0;
/// `_wait_for_packet_type`'s default `timeout=5.0` (`__init__.py:786`).
pub const WAIT_FOR_PACKET_TYPE_TIMEOUT: f64 = 5.0;
/// `enable_feature`'s `max_packets=10` (`__init__.py:977`): up to 11
/// packets per call (`processed_count > max_packets`, `:770`).
pub const ENABLE_MAX_PACKETS: usize = 10;
/// `initialize`'s attempts (`__init__.py:505`).
pub const INIT_ATTEMPTS: usize = 3;
/// `soft_reset`'s and `initialize`'s back-off (`__init__.py:512,1054-1062`).
pub const INIT_SLEEP_S: f64 = 0.5;
/// `soft_reset`'s `_read_packet` count (`__init__.py:1059`).
pub const SOFT_RESET_READS: usize = 3;
/// The two features the helper enables, in order (`bno085_read.py:58-59`).
pub const FEATURES: [u8; 2] = [sh2::REPORT_ROTATION_VECTOR, sh2::REPORT_LINEAR_ACCELERATION];
/// `errno.ENOENT`: the one `open` error Blinka turns into the bus-missing text
/// (`generic_linux/i2c.py:34-39` catches `FileNotFoundError` only).
pub const ENOENT: i32 = 2;

/// The helper's arguments (`bno085_read.py:26-30`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReaderArgs {
    /// The parsed `--address` (`parse_address`), e.g. `0x4a`.
    pub address: i64,
    /// `--interval` seconds (default 0.05).
    pub interval: f64,
}

impl Default for ReaderArgs {
    fn default() -> Self {
        Self {
            address: 0x4A,
            interval: 0.05,
        }
    }
}

impl ReaderArgs {
    /// `int(2.0 / args.interval) if args.interval > 0 else 40` (`:73`).
    pub fn max_consecutive_errors(&self) -> u32 {
        if self.interval > 0.0 {
            (2.0 / self.interval) as u32
        } else {
            40
        }
    }
}

/// How the reader ended (the process exit status the agent sees).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderExit {
    /// SIGINT in the loop's interval wait: `except KeyboardInterrupt:
    /// return 0` (`bno085_read.py:98-99`).
    Interrupted,
    /// `init failed at …`, `return 3` (`:60-62`).
    InitFailed,
    /// `read failed 40x: …`, `return 4` (`:84-87`).
    ReadFailed,
    /// stdout gone (EPIPE): the helper's uncaught `BrokenPipeError` (plan P5).
    StdoutClosed,
}

impl ReaderExit {
    pub fn code(self) -> i32 {
        match self {
            ReaderExit::Interrupted => 0,
            ReaderExit::StdoutClosed => 1,
            ReaderExit::InitFailed => 3,
            ReaderExit::ReadFailed => 4,
        }
    }
}

/// stdout could not be written (EPIPE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdoutClosed;

/// The reader's stdout: ONE `write` per call (what CPython's flush does).
pub trait ImuSink {
    fn write_chunk(&mut self, chunk: &str) -> Result<(), StdoutClosed>;

    /// Called ONCE, right after the `ready` line was written and before the
    /// first loop iteration: the helper's `try:` around its loop
    /// (`bno085_read.py:76`), from where a SIGINT means `return 0` instead of
    /// a death by SIGINT. The WP3 reader blocks SIGINT here (plan §Design 1:
    /// default disposition through init, taken at the interval wait from the
    /// loop start on). Default: nothing (the lockstep runner, tests).
    fn loop_starts(&mut self) {}
}

impl<T: ImuSink + ?Sized> ImuSink for &mut T {
    fn write_chunk(&mut self, chunk: &str) -> Result<(), StdoutClosed> {
        (**self).write_chunk(chunk)
    }
    fn loop_starts(&mut self) {
        (**self).loop_starts()
    }
}

/// CPython's block-buffered stdout, reduced to what the helper does with it:
/// `print(text)` buffers, `print(json, flush=True)` writes buffer + line in one
/// write; what is left at a normal exit is handed to the caller
/// ([`ReaderEnd::unflushed`]); a death by signal loses it.
struct Stdout<'a, S: ImuSink> {
    sink: &'a mut S,
    buffered: String,
}

impl<S: ImuSink> Stdout<'_, S> {
    /// `print(text)` (no flush).
    fn print(&mut self, text: &str) {
        self.buffered.push_str(text);
        self.buffered.push_str(PRINT_END);
    }

    /// `emit(obj)` = `print(json.dumps(obj), flush=True)` (`bno085_read.py:17-18`).
    fn emit(&mut self, line: &str) -> Result<(), StdoutClosed> {
        self.print(line);
        let chunk = std::mem::take(&mut self.buffered);
        self.sink.write_chunk(&chunk)
    }
}

/// `adafruit_bno08x.BNO08X_I2C`'s state (`__init__.py:481-501`, `i2c.py`).
struct Driver {
    /// `addr & 0x7F`, PureIO's ioctl argument (`smbus.py:154`).
    addr7: u8,
    buffer: ShtpBuffer,
    /// What received headers store (the library's `_sequence_number`); a
    /// channel > 5 raises there. Not used for sends since agent 0.2.2.
    seq: SequenceTable,
    /// The host's own write numbering per channel (agent 0.2.2).
    tx_seq: SequenceTable,
    pending: Vec<ReportSlice>,
    readings: Readings,
    id_read: bool,
    /// The main firmware's identity from the accepted product-ID response.
    firmware: Option<FirmwareId>,
}

fn os_exc(e: I2cError) -> PyExc {
    e.to_py(I2C_BUS_1_PATH)
}

fn is_packet_error(e: &PyExc) -> bool {
    e.kind() == Some(ExcKind::PacketError)
}

struct Io<'a, B: I2cOps, C: ImuClock, S: ImuSink> {
    bus: &'a mut B,
    clock: &'a mut C,
    out: Stdout<'a, S>,
}

impl Driver {
    fn new(address: i64) -> Self {
        Self {
            addr7: select_device_arg(address),
            buffer: ShtpBuffer::new(),
            seq: SequenceTable::new(),
            tx_seq: SequenceTable::new(),
            pending: Vec::new(),
            readings: Readings::new(),
            id_read: false,
            firmware: None,
        }
    }

    /// `I2CDevice.readinto(self._data_buffer, end=end)` → busio's
    /// `memoryview[0:end]` → PureIO's ioctl + ONE `read(n)` → Blinka's copy
    /// loop (`i2c_device.py:64-79`, `busio.py:207-213`,
    /// `generic_linux/i2c.py:70-77`).
    fn readinto<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
        end: usize,
    ) -> Result<(), PyExc> {
        let n = self.buffer.read_len(end);
        match io.bus.read(self.addr7, n).map_err(os_exc)? {
            ReadOutcome::Data(b) => self.buffer.fill(end, ReadBytes::Data(&b)),
            ReadOutcome::WouldBlock => self.buffer.fill(end, ReadBytes::WouldBlock),
        }
    }

    /// `_send_packet` (`i2c.py:30-46`): staged into the shared buffer, ONE
    /// write of `buffer[0:len]`; a write `EAGAIN` (`None`) counts as sent;
    /// the sequence byte advances either way. The byte is the host's OWN
    /// count for the channel (agent 0.2.2, the helper's `ShtpI2C`), not the
    /// last one received; a write that fails does not advance it.
    fn send_packet<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
        channel: u8,
        data: &[u8],
    ) -> Result<(), PyExc> {
        let seq = self.tx_seq.get(channel).unwrap_or(0);
        let bytes = self
            .buffer
            .stage_send(channel, seq, data)
            .expect("the driver's sends fit its 512-byte buffer");
        io.bus.write(self.addr7, &bytes).map_err(os_exc)?;
        self.tx_seq.advance_after_send(channel);
        Ok(())
    }

    /// `_data_ready` (`i2c.py:107-122`): one header read; the `0x7FFF` lines
    /// are `print()`ed (buffered).
    fn data_ready<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
    ) -> Result<bool, PyExc> {
        self.readinto(io, HEADER_LEN)?;
        let r = shtp::data_ready(self.buffer.header_bytes());
        for line in r.stdout {
            io.out.print(line);
        }
        Ok(r.ready)
    }

    /// `_read_packet` (`i2c.py:59-91`): the 2nd header read sizes the read
    /// and stores the sequence (BEFORE the count-0 `PacketError`); the full
    /// read; the Packet re-parsed from the BUFFER (the 3rd header), whose
    /// sequence is stored again (`_update_sequence_number`).
    fn read_packet<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
    ) -> Result<Packet, PyExc> {
        self.readinto(io, HEADER_LEN)?;
        let h = self.buffer.header();
        self.seq.store_received(h.channel, h.sequence)?;
        if h.packet_byte_count == 0 {
            return Err(PyExc::msg(ExcKind::PacketError, "No packet available"));
        }
        let end = self.buffer.prepare_full_read(h.packet_byte_count);
        self.readinto(io, end)?;
        let p = self.buffer.packet();
        self.seq
            .store_received(p.header.channel, p.header.sequence)?;
        Ok(p)
    }

    /// `_handle_packet` (`__init__.py:829-837`): the persistent pending list,
    /// LIFO; a successful `ME_CALIBRATE`/`SAVE_DCD` command response reads
    /// the clock (`:876-882`). The `print(packet)` dump on error: D80.
    /// Packets on channel 0 (SHTP command) and 1 (executable) are never
    /// reports and are dropped here (agent 0.2.2, the helper's `ShtpI2C`):
    /// a BNO086's error list `01 0e` would otherwise read as a truncated
    /// accelerometer report, and a reset's `01` on channel 1 likewise.
    fn handle_packet<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
        packet: &Packet,
    ) -> Result<(), PyExc> {
        if packet.header.channel == CHANNEL_SHTP_COMMAND || packet.header.channel == CHANNEL_EXE {
            return Ok(());
        }
        let clock = &mut *io.clock;
        sh2::handle_packet_with(packet, &mut self.pending, &mut self.readings, |slice| {
            if sh2::command_response_reads_clock(slice) {
                clock.monotonic();
            }
        })
    }

    /// `_process_available_packets(max_packets)` (`__init__.py:767-783`).
    fn process_available_packets<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
        max_packets: Option<usize>,
    ) -> Result<(), PyExc> {
        let mut processed = 0usize;
        while self.data_ready(io)? {
            if let Some(max) = max_packets {
                if max != 0 && processed > max {
                    return Ok(());
                }
            }
            let packet = match self.read_packet(io) {
                Ok(p) => p,
                Err(e) if is_packet_error(&e) => continue,
                Err(e) => return Err(e),
            };
            self.handle_packet(io, &packet)?;
            processed += 1;
        }
        Ok(())
    }

    /// `_wait_for_packet(timeout)` (`__init__.py:812-819`): busy-polls
    /// `_data_ready`; a `PacketError` from `_read_packet` propagates.
    fn wait_for_packet<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
        timeout: f64,
    ) -> Result<Packet, PyExc> {
        let start = io.clock.monotonic();
        while io.clock.monotonic() - start < timeout {
            if !self.data_ready(io)? {
                continue;
            }
            return self.read_packet(io);
        }
        Err(PyExc::msg(
            ExcKind::RuntimeError,
            "Timed out waiting for a packet",
        ))
    }

    /// `_wait_for_packet_type(channel, report_id, timeout=5.0)`
    /// (`__init__.py:785-810`): packets on other channels than 0/1 are
    /// HANDLED meanwhile; `report_id` reads `data[0]` (an empty packet on the
    /// wanted channel raises `IndexError`).
    fn wait_for_packet_type<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
        channel: u8,
        report_id: u8,
    ) -> Result<Packet, PyExc> {
        let start = io.clock.monotonic();
        while io.clock.monotonic() - start < WAIT_FOR_PACKET_TYPE_TIMEOUT {
            let packet = self.wait_for_packet(io, PACKET_READ_TIMEOUT)?;
            if packet.header.channel == channel {
                let id = packet
                    .report_id()
                    .ok_or_else(|| PyExc::msg(ExcKind::IndexError, BYTEARRAY_INDEX))?;
                if id == report_id {
                    return Ok(packet);
                }
            }
            if packet.header.channel != CHANNEL_EXE && packet.header.channel != CHANNEL_SHTP_COMMAND
            {
                self.handle_packet(io, &packet)?;
            }
        }
        Err(PyExc::new(
            ExcKind::RuntimeError,
            vec![
                "Timed out waiting for a packet on channel".into(),
                i64::from(channel).into(),
            ],
        ))
    }

    /// `_check_id` (`__init__.py:982-1001`): one request, then wait until a
    /// product-id response carries a NON-ZERO part number (read from the
    /// BUFFER); a zero one waits again WITHOUT a new request.
    fn check_id<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
    ) -> Result<bool, PyExc> {
        if self.id_read {
            return Ok(true);
        }
        self.send_packet(io, CHANNEL_CONTROL, &sh2::PRODUCT_ID_REQUEST)?;
        loop {
            self.wait_for_packet_type(io, CHANNEL_CONTROL, sh2::REPORT_PRODUCT_ID_RESPONSE)?;
            if matches!(self.buffer.parse_sensor_id(), Some(part) if part != 0) {
                self.id_read = true;
                self.firmware = self.buffer.parse_firmware_id();
                return Ok(true);
            }
        }
    }

    /// `soft_reset` (`__init__.py:1048-1066`): two resets on channel 1 with
    /// 0.5 s sleeps, then 3 `_read_packet`s (a `PacketError` sleeps 0.5 s).
    /// NOT inside `initialize`'s `try`: any other error escapes.
    fn soft_reset<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
    ) -> Result<(), PyExc> {
        self.send_packet(io, CHANNEL_EXE, &sh2::SOFT_RESET)?;
        io.clock.sleep(INIT_SLEEP_S);
        self.send_packet(io, CHANNEL_EXE, &sh2::SOFT_RESET)?;
        io.clock.sleep(INIT_SLEEP_S);
        for _ in 0..SOFT_RESET_READS {
            match self.read_packet(io) {
                Ok(_) => {}
                Err(e) if is_packet_error(&e) => io.clock.sleep(INIT_SLEEP_S),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// `initialize` (`__init__.py:503-514`).
    fn initialize<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
    ) -> Result<(), PyExc> {
        for _ in 0..INIT_ATTEMPTS {
            // hard_reset(): a no-op without a reset pin (`:1034-1046`).
            self.soft_reset(io)?;
            match self.check_id(io) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(_) => io.clock.sleep(INIT_SLEEP_S),
            }
        }
        Err(PyExc::msg(ExcKind::RuntimeError, "Could not read ID"))
    }

    /// `enable_feature(feature_id)` (`__init__.py:950-980`) for 0x05/0x04
    /// (no raw-report dependency).
    fn enable_feature<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
        feature_id: u8,
    ) -> Result<(), PyExc> {
        let report = sh2::enable_feature_report(feature_id, sh2::DEFAULT_REPORT_INTERVAL_US);
        self.send_packet(io, CHANNEL_CONTROL, &report)?;
        let start = io.clock.monotonic();
        while io.clock.monotonic() - start < FEATURE_ENABLE_TIMEOUT {
            self.process_available_packets(io, Some(ENABLE_MAX_PACKETS))?;
            if self.readings.contains(feature_id) {
                return Ok(());
            }
        }
        Err(PyExc::msg_int(
            ExcKind::RuntimeError,
            "Was not able to enable feature",
            i64::from(feature_id),
        ))
    }

    /// One loop body (`bno085_read.py:80-81`): `bno.quaternion` (drain, then
    /// the cached reading — captured HERE, after the first drain), then
    /// `bno.linear_acceleration` (drain again, cached reading).
    fn read_sample<B: I2cOps, C: ImuClock, S: ImuSink>(
        &mut self,
        io: &mut Io<'_, B, C, S>,
    ) -> Result<([f64; 4], [f64; 3]), PyExc> {
        self.process_available_packets(io, None)?;
        let quat = self.readings.quaternion()?;
        self.process_available_packets(io, None)?;
        let linaccel = self.readings.linear_acceleration()?;
        Ok((quat, linaccel))
    }
}

/// busdevice's probe (`i2c_device.py:159-180`): a ZERO-LENGTH write (a real
/// `write(2)`); on `OSError` (ioctl or write) a 1-byte read into its own
/// buffer; that read failing with `OSError` → `ValueError("No I2C device at
/// address: 0x..")`. A `None`/short result of the fallback read escapes as
/// Blinka's `TypeError`/`IndexError` (not an `OSError`).
fn probe<B: I2cOps>(bus: &mut B, address: i64) -> Result<(), PyExc> {
    let addr7 = select_device_arg(address);
    if bus.write(addr7, &[]).is_ok() {
        return Ok(());
    }
    let mut result = ShtpBuffer::from_bytes(vec![0; 1]);
    match bus.read(addr7, 1) {
        Ok(ReadOutcome::Data(b)) => result.fill(1, ReadBytes::Data(&b)),
        Ok(ReadOutcome::WouldBlock) => result.fill(1, ReadBytes::WouldBlock),
        Err(_) => Err(PyExc::msg(
            ExcKind::ValueError,
            &no_i2c_device_text(address),
        )),
    }
}

/// `busio.I2C(...)` + `BNO08X_I2C(...)` + the two enables (`bno085_read.py:56-59`).
fn init<B: I2cOps, C: ImuClock, S: ImuSink>(
    io: &mut Io<'_, B, C, S>,
    args: &ReaderArgs,
) -> Result<Driver, PyExc> {
    if let Err(e) = io.bus.open(I2C_BUS_1_PATH) {
        if e.errno == ENOENT {
            return Err(PyExc::msg(
                ExcKind::ValueError,
                BUS_MISSING_BCM2712_PROVISIONAL,
            ));
        }
        return Err(os_exc(e));
    }
    probe(io.bus, args.address)?;
    let mut d = Driver::new(args.address);
    d.initialize(io)?;
    for feature in FEATURES {
        d.enable_feature(io, feature)?;
    }
    Ok(d)
}

/// How [`run`] ended, and what the caller must still do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderEnd {
    /// The exit status ([`ReaderExit::code`]).
    pub exit: ReaderExit,
    /// `print()`ed text CPython still holds in its stdout buffer (the
    /// `_data_ready` lines of the last iterations). CPython's teardown order
    /// is: `main()` returns → the bus is CLOSED (`SMBus.__del__`) → the
    /// interpreter FLUSHES stdout (review-wp2 F4). So the caller closes the bus
    /// fd FIRST, then writes this in ONE write (if non-empty), then exits.
    /// Always empty for [`ReaderExit::StdoutClosed`].
    pub unflushed: String,
}

/// Run the reader to its end: the helper's `main()` after argument parsing.
/// Returns how it ended; the caller then closes the bus, writes
/// [`ReaderEnd::unflushed`] and exits with [`ReaderExit::code`]. A death by
/// signal (SIGTERM, SIGINT during init) never returns here: those are the
/// default dispositions of the real process (WP3; D83).
pub fn run<B: I2cOps, C: ImuClock, S: ImuSink>(
    bus: &mut B,
    clock: &mut C,
    sink: &mut S,
    args: &ReaderArgs,
) -> ReaderEnd {
    let mut io = Io {
        bus,
        clock,
        out: Stdout {
            sink,
            buffered: String::new(),
        },
    };
    let exit = run_inner(&mut io, args);
    let unflushed = if exit == ReaderExit::StdoutClosed {
        String::new()
    } else {
        std::mem::take(&mut io.out.buffered)
    };
    ReaderEnd { exit, unflushed }
}

fn run_inner<B: I2cOps, C: ImuClock, S: ImuSink>(
    io: &mut Io<'_, B, C, S>,
    args: &ReaderArgs,
) -> ReaderExit {
    let mut driver = match init(io, args) {
        Ok(d) => d,
        Err(exc) => {
            let line = error_line(&init_failed_text(args.address, &exc.py_str()));
            return match io.out.emit(&line) {
                Ok(()) => ReaderExit::InitFailed,
                Err(StdoutClosed) => ReaderExit::StdoutClosed,
            };
        }
    };
    if io
        .out
        .emit(&ready_line_with(args.address, driver.firmware.as_ref()))
        .is_err()
    {
        return ReaderExit::StdoutClosed;
    }
    io.out.sink.loop_starts();
    let max_errors = args.max_consecutive_errors();
    let mut consecutive: u32 = 0;
    loop {
        match driver.read_sample(io) {
            Err(exc) => {
                consecutive += 1;
                if consecutive >= max_errors {
                    let line = error_line(&read_failed_text(consecutive, &exc.py_str()));
                    return match io.out.emit(&line) {
                        Ok(()) => ReaderExit::ReadFailed,
                        Err(StdoutClosed) => ReaderExit::StdoutClosed,
                    };
                }
            }
            Ok((quat, linaccel)) => {
                consecutive = 0;
                if io.out.emit(&sample_line(quat, linaccel)).is_err() {
                    return ReaderExit::StdoutClosed;
                }
            }
        }
        if io.clock.interval_wait(args.interval) == IntervalWait::Interrupted {
            return ReaderExit::Interrupted;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_consecutive_errors_is_int_of_two_over_interval() {
        assert_eq!(ReaderArgs::default().max_consecutive_errors(), 40);
        let a = |interval| ReaderArgs {
            address: 0x4a,
            interval,
        };
        assert_eq!(a(0.03).max_consecutive_errors(), 66);
        assert_eq!(a(0.0).max_consecutive_errors(), 40);
        assert_eq!(a(-1.0).max_consecutive_errors(), 40);
    }

    // -- agent 0.2.2: the BNO086 fixes, on a small scripted hub ----------

    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use crate::imu_io::{I2cError, ReadOutcome, WriteOutcome};

    /// A scripted SHTP hub (the oracle's `Sensor` model, `tools/imu/oracle/
    /// scenarios.py`, reduced): a read of `n` bytes returns the head frame's
    /// first `n` bytes and pops it once `n` covers it; an empty queue reads
    /// zeros. It answers the product-ID request and each set-feature.
    #[derive(Default)]
    struct Hub {
        queue: VecDeque<Vec<u8>>,
        /// The hub's own per-channel sequence (what its headers carry).
        seq: [u8; 6],
        /// Every host write of 4+ bytes: (channel, sequence byte).
        writes: Vec<(u8, u8)>,
        /// Pushed after each set-feature: a BNO086's error list on channel 0.
        error_list_after_set_feature: bool,
        /// Interval waits so far; at the first, `on_first_wait` frames go in.
        waits: u32,
        on_first_wait: Vec<(u8, Vec<u8>)>,
        now: f64,
    }

    impl Hub {
        fn push(&mut self, channel: u8, payload: &[u8]) {
            let count = (payload.len() + 4) as u16;
            let mut f = count.to_le_bytes().to_vec();
            f.push(channel);
            f.push(self.seq[channel as usize]);
            // The hub's numbering jumps (as the library's host-side
            // overwrite would then make the HOST's jump).
            self.seq[channel as usize] = self.seq[channel as usize].wrapping_add(3);
            f.extend_from_slice(payload);
            self.queue.push_back(f);
        }
    }

    fn f8(part: u32, version: (u8, u8, u16), build: u32) -> Vec<u8> {
        let mut v = vec![0xF8, 0x00, version.0, version.1];
        v.extend_from_slice(&part.to_le_bytes());
        v.extend_from_slice(&build.to_le_bytes());
        v.extend_from_slice(&version.2.to_le_bytes());
        v.extend_from_slice(&[0, 0]);
        v
    }

    struct Bus(Rc<RefCell<Hub>>);
    struct Clock(Rc<RefCell<Hub>>);
    #[derive(Default)]
    struct Out(String);

    impl I2cOps for Bus {
        fn open(&mut self, _path: &str) -> Result<(), I2cError> {
            Ok(())
        }
        fn write(&mut self, _address: u8, bytes: &[u8]) -> Result<WriteOutcome, I2cError> {
            let mut h = self.0.borrow_mut();
            if bytes.len() >= 4 {
                let (channel, payload) = (bytes[2], &bytes[4..]);
                h.writes.push((channel, bytes[3]));
                if channel == 2 && payload.first() == Some(&0xF9) {
                    h.push(2, &f8(10_004_563, (3, 12, 6), 62));
                } else if channel == 2 && payload.first() == Some(&0xFD) {
                    let fid = payload[1];
                    let mut fc = vec![0xFC, fid, 0, 0, 0];
                    fc.extend_from_slice(&50_000u32.to_le_bytes());
                    fc.extend_from_slice(&[0; 8]);
                    h.push(2, &fc);
                    if h.error_list_after_set_feature {
                        h.push(0, &[0x01, 0x0E]);
                    }
                }
            }
            Ok(WriteOutcome::Written(bytes.len()))
        }
        fn read(&mut self, _address: u8, len: usize) -> Result<ReadOutcome, I2cError> {
            let mut h = self.0.borrow_mut();
            let Some(head) = h.queue.front().cloned() else {
                return Ok(ReadOutcome::Data(vec![0; len]));
            };
            if len >= head.len() {
                h.queue.pop_front();
                let mut b = head;
                b.resize(len, 0);
                return Ok(ReadOutcome::Data(b));
            }
            Ok(ReadOutcome::Data(head[..len].to_vec()))
        }
    }

    impl ImuClock for Clock {
        fn monotonic(&mut self) -> f64 {
            let mut h = self.0.borrow_mut();
            h.now += 0.001;
            h.now
        }
        fn sleep(&mut self, secs: f64) {
            self.0.borrow_mut().now += secs;
        }
        fn interval_wait(&mut self, secs: f64) -> IntervalWait {
            let mut h = self.0.borrow_mut();
            h.now += secs;
            h.waits += 1;
            if h.waits == 1 {
                for (ch, p) in std::mem::take(&mut h.on_first_wait) {
                    h.push(ch, &p);
                }
            }
            if h.waits >= 4 {
                IntervalWait::Interrupted
            } else {
                IntervalWait::Elapsed
            }
        }
    }

    impl ImuSink for Out {
        fn write_chunk(&mut self, chunk: &str) -> Result<(), StdoutClosed> {
            self.0.push_str(chunk);
            Ok(())
        }
    }

    fn run_hub(hub: Hub) -> (ReaderEnd, Vec<String>, Hub) {
        let hub = Rc::new(RefCell::new(hub));
        let (mut bus, mut clock, mut out) = (Bus(hub.clone()), Clock(hub.clone()), Out::default());
        let end = run(&mut bus, &mut clock, &mut out, &ReaderArgs::default());
        drop((bus, clock));
        let lines = out.0.lines().map(str::to_string).collect();
        (end, lines, Rc::try_unwrap(hub).ok().unwrap().into_inner())
    }

    /// The host numbers its writes per channel 0, 1, 2…, whatever sequence
    /// the hub's packets carry in between (the library copied the hub's
    /// numbers: a BNO086 answers each jump with an error list).
    #[test]
    fn host_writes_number_each_channel_from_zero() {
        let (end, lines, hub) = run_hub(Hub {
            seq: [0x10, 0x20, 0xFE, 0xFD, 0, 0],
            ..Hub::default()
        });
        assert_eq!(end.exit, ReaderExit::Interrupted, "{lines:?}");
        let per = |ch: u8| -> Vec<u8> {
            hub.writes
                .iter()
                .filter(|(c, _)| *c == ch)
                .map(|(_, s)| *s)
                .collect()
        };
        // Two soft resets on channel 1; product id, then two set-features on 2.
        assert_eq!(per(1), [0, 1]);
        assert_eq!(per(2), [0, 1, 2]);
        // The received table still takes the hub's numbers (channel > 5
        // still raises there), and is no longer what a write carries.
        let mut t = SequenceTable::new();
        t.store_received(2, 0xFE).unwrap();
        assert_eq!(t.get(2), Some(0xFE));
    }

    /// A BNO086's error list (`01 0e`) on channel 0 after each set-feature
    /// (init), then an error list and a reset's 1-byte `01` on channel 1
    /// mid-stream: never handed to the report parser, so no
    /// `Unprocessable Batch bytes`; the reader starts and keeps streaming.
    /// The `ready` line carries the main firmware's identity.
    #[test]
    fn channel_0_and_1_packets_are_ignored_in_init_and_mid_stream() {
        let hub = Hub {
            error_list_after_set_feature: true,
            on_first_wait: vec![(0, vec![0x01, 0x0E, 0x0E]), (1, vec![0x01])],
            ..Hub::default()
        };
        let (end, lines, hub) = run_hub(hub);
        assert_eq!(end.exit, ReaderExit::Interrupted, "{lines:?}");
        assert_eq!(
            lines[0],
            r#"{"type": "ready", "address": "0x4a", "part": 10004563, "version": "3.12.6", "build": 62}"#
        );
        let samples = lines
            .iter()
            .filter(|l| l.starts_with(r#"{"type": "sample""#))
            .count();
        assert_eq!(samples, 4, "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("error")), "{lines:?}");
        assert!(hub.queue.is_empty(), "every packet was read");
    }

    #[test]
    fn exit_codes_match_the_helper() {
        assert_eq!(ReaderExit::Interrupted.code(), 0);
        assert_eq!(ReaderExit::StdoutClosed.code(), 1);
        assert_eq!(ReaderExit::InitFailed.code(), 3);
        assert_eq!(ReaderExit::ReadFailed.code(), 4);
    }
}
