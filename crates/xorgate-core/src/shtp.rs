//! SHTP framing for the Rust BNO085 reader (phase 7c WP1), ported from
//! `adafruit_bno08x` 1.3.3 (`__init__.py`, `i2c.py`) as installed on the
//! bench (`phase-7c/evidence/wp0.md`), quirks included (plan P2 items 6-8):
//!
//! - the 4-byte header: `count = u16le & ~0x8000` (the continuation bit is
//!   dropped), channel, sequence, `data_length = max(0, count - 4)`
//!   (`Packet.header_from_buffer`, `__init__.py:451-461`);
//! - `_data_ready` (`i2c.py:107-122`): the `0x7FFF` test runs on the MASKED
//!   count, so a raw `0xFFFF` (bus reads all ones) also trips it, and it
//!   prints two non-JSON lines to stdout;
//! - ONE sequence table shared by both directions (`__init__.py:489-490`):
//!   every received header overwrites `seq[channel]` (`i2c.py:70`, then
//!   again from the full read's header, `i2c.py:89` + `__init__.py:824-827`);
//!   a send uses it and then increments it (`i2c.py:36,45`);
//! - ONE persistent buffer shared by send and receive ([`ShtpBuffer`],
//!   `__init__.py:485`, `i2c.py:34-43,54,61,94-105`), REPLACED by a fresh
//!   zeroed `bytearray(count)` whenever a full read needs more than 512 bytes
//!   (so it can shrink, never below 512); the Packet is re-parsed from that
//!   buffer after the full read (`i2c.py:85`, `__init__.py:389-392`), i.e.
//!   from the THIRD header, and a header claiming more than was read slices
//!   stale bytes.
//!
//! Pure: the client (WP2) sequences the bus ops through
//! [`crate::imu_io::I2cOps`]; this module only holds the state and the rules.
//! Every rule is pinned by `fixtures/imu_codec/shtp.json`, printed by the
//! REAL Adafruit code (`tools/imu/codec-fixtures/gen_codec_fixtures.py`).

use crate::py_fmt::{
    ExcKind, FirmwareId, PyExc, BYTES_INDEX, LIST_ASSIGNMENT_INDEX, NONE_NOT_SUBSCRIPTABLE,
};

/// `_BNO_HEADER_LEN` (`__init__.py:125`).
pub const HEADER_LEN: usize = 4;
/// `DATA_BUFFER_SIZE` (`__init__.py:193`).
pub const DATA_BUFFER_SIZE: usize = 512;
/// The SHTP continuation flag, cleared by `count &= ~0x8000` (`__init__.py:455`).
pub const CONTINUATION_BIT: u16 = 0x8000;
/// The masked count `_data_ready` treats as a bus error (`i2c.py:113`).
pub const ERROR_COUNT: u16 = 0x7FFF;
/// Six channels; `_sequence_number = [0]*6` (`__init__.py:490`).
pub const CHANNEL_COUNT: usize = 6;

/// Channel numbers (`__init__.py:52-57`).
pub const CHANNEL_SHTP_COMMAND: u8 = 0;
pub const CHANNEL_EXE: u8 = 1;
pub const CHANNEL_CONTROL: u8 = 2;
pub const CHANNEL_INPUT_SENSOR_REPORTS: u8 = 3;
pub const CHANNEL_WAKE_INPUT_SENSOR_REPORTS: u8 = 4;
pub const CHANNEL_GYRO_ROTATION_VECTOR: u8 = 5;

/// `_data_ready`'s first stdout line (`i2c.py:114`), printed when the masked
/// count is `0x7FFF`. Reproduced by the reader (plan P2 "Non-JSON stdout").
pub const BYTE_COUNT_ERROR_LINE: &str = "Byte count is 0x7FFF/0xFFFF; Error?";
/// `_data_ready`'s second stdout line (`i2c.py:115-116`), when also `seq == 0xFF`.
pub const SEQUENCE_ERROR_LINE: &str = "Sequence number is 0xFF; Error?";

/// `PacketHeader` (`__init__.py:194-202`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub channel: u8,
    pub sequence: u8,
    /// `max(0, packet_byte_count - 4)`.
    pub data_length: usize,
    /// The count with the continuation bit cleared.
    pub packet_byte_count: u16,
}

/// `Packet.header_from_buffer` (`__init__.py:451-461`).
pub fn header_from_bytes(b: [u8; 4]) -> PacketHeader {
    let packet_byte_count = u16::from_le_bytes([b[0], b[1]]) & !CONTINUATION_BIT;
    // `data_length = max(0, packet_byte_count - 4)` (:458): counts 0-3 → 0.
    let data_length = (i32::from(packet_byte_count) - HEADER_LEN as i32).max(0) as usize;
    PacketHeader {
        channel: b[2],
        sequence: b[3],
        data_length,
        packet_byte_count,
    }
}

/// The inverse of [`header_from_bytes`] for fixtures: `count` is written as
/// given (set [`CONTINUATION_BIT`] in it to mark a continuation).
pub fn encode_header(count: u16, channel: u8, sequence: u8) -> [u8; 4] {
    let c = count.to_le_bytes();
    [c[0], c[1], channel, sequence]
}

/// A whole SHTP frame for fixtures: header (`payload.len() + 4`, plus the
/// continuation bit when asked) followed by the payload.
pub fn encode_packet(channel: u8, sequence: u8, payload: &[u8], continuation: bool) -> Vec<u8> {
    let mut count = (payload.len() + HEADER_LEN) as u16;
    if continuation {
        count |= CONTINUATION_BIT;
    }
    let mut out = encode_header(count, channel, sequence).to_vec();
    out.extend_from_slice(payload);
    out
}

/// What `_data_ready` returned and printed (`i2c.py:107-122`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataReady {
    pub ready: bool,
    /// Lines `print()`ed to stdout, in order (without the `"\n"`).
    pub stdout: Vec<&'static str>,
}

/// `_data_ready` on the 4 header bytes it just read (`i2c.py:107-122`). The
/// `0x7FFF` test runs on the MASKED count (`:113`, via `header_from_buffer`).
/// A channel > 5 only reaches `_dbg` (`:111-112`), which prints nothing.
pub fn data_ready(raw: [u8; 4]) -> DataReady {
    let header = header_from_bytes(raw);
    if header.packet_byte_count == ERROR_COUNT {
        let mut stdout = vec![BYTE_COUNT_ERROR_LINE];
        if header.sequence == 0xFF {
            stdout.push(SEQUENCE_ERROR_LINE);
        }
        return DataReady {
            ready: false,
            stdout,
        };
    }
    DataReady {
        ready: header.data_length > 0,
        stdout: Vec::new(),
    }
}

/// A per-channel sequence table. The library keeps ONE for both directions
/// (`__init__.py:489-490`): the received side of the client is that table;
/// since agent 0.2.2 the host's writes number from a second one of their own
/// (the helper's SHTP fix; a BNO086 reports a jump with an error list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SequenceTable(pub [u8; CHANNEL_COUNT]);

impl SequenceTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// The byte `_send_packet` puts in the header (`i2c.py:36`).
    /// `None` for a channel > 5 (a Python `IndexError`; no send uses one).
    pub fn get(&self, channel: u8) -> Option<u8> {
        self.0.get(channel as usize).copied()
    }

    /// A received header's store: `self._sequence_number[channel] = seq`
    /// (`i2c.py:70,89`, `__init__.py:827`). A channel > 5 raises
    /// `IndexError('list assignment index out of range')` and stores nothing.
    pub fn store_received(&mut self, channel: u8, sequence: u8) -> Result<(), PyExc> {
        match self.0.get_mut(channel as usize) {
            Some(slot) => {
                *slot = sequence;
                Ok(())
            }
            None => Err(PyExc::msg(ExcKind::IndexError, LIST_ASSIGNMENT_INDEX)),
        }
    }

    /// After a send: `seq[channel] = (seq[channel] + 1) % 256`, returned as
    /// `_send_packet`'s value (`i2c.py:45-46`). Runs even when the write hit
    /// `EAGAIN` (CPython returns `None`, PureIO ignores it; plan P5).
    pub fn advance_after_send(&mut self, channel: u8) -> Option<u8> {
        let slot = self.0.get_mut(channel as usize)?;
        *slot = slot.wrapping_add(1);
        Some(*slot)
    }
}

/// `Packet` (`__init__.py:386-392`): the header and `data =
/// packet_bytes[4:data_length + 4]`, which Python slicing TRUNCATES at the
/// buffer end (so `data.len()` can be < `header.data_length`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub header: PacketHeader,
    pub data: Vec<u8>,
}

impl Packet {
    /// `Packet(packet_bytes)`. `packet_bytes` is at least 4 bytes long (the
    /// driver's buffer is ≥ 512).
    pub fn from_buffer(packet_bytes: &[u8]) -> Packet {
        let header = header_from_bytes([
            packet_bytes[0],
            packet_bytes[1],
            packet_bytes[2],
            packet_bytes[3],
        ]);
        let end = (header.data_length + HEADER_LEN).min(packet_bytes.len());
        let data = packet_bytes[HEADER_LEN.min(end)..end].to_vec();
        Packet { header, data }
    }

    /// `Packet.report_id` = `data[0]` (`__init__.py:441-444`).
    pub fn report_id(&self) -> Option<u8> {
        self.data.first().copied()
    }
}

/// What one `FileIO.read(n)` gave Blinka's copy loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadBytes<'a> {
    /// The bytes `read(2)` returned (fewer than asked at i2c-dev's 8192 clamp).
    Data(&'a [u8]),
    /// `EAGAIN`: CPython's `FileIO.read` returned `None` (review F2).
    WouldBlock,
}

/// The driver's ONE persistent `_data_buffer` (`__init__.py:485`), shared by
/// `_send_packet` and every read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShtpBuffer {
    buf: Vec<u8>,
}

impl Default for ShtpBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ShtpBuffer {
    /// `bytearray(DATA_BUFFER_SIZE)` (`__init__.py:485`).
    pub fn new() -> Self {
        Self {
            buf: vec![0; DATA_BUFFER_SIZE],
        }
    }

    /// A buffer with given contents (tests and fixtures).
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { buf: bytes }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// `_send_packet`'s staging (`i2c.py:30-43`): the header
    /// `<H write_length>, channel, seq` and the data are written INTO the
    /// shared buffer, and `buffer[0:write_length]` is what goes to ONE
    /// `write(2)`. Returns those bytes. `None` if the data does not fit the
    /// buffer (unreachable: the driver sends ≤ 17 data bytes).
    pub fn stage_send(&mut self, channel: u8, sequence: u8, data: &[u8]) -> Option<Vec<u8>> {
        let write_length = data.len() + HEADER_LEN;
        if write_length > self.buf.len() || write_length > usize::from(u16::MAX) {
            return None;
        }
        self.buf[..HEADER_LEN].copy_from_slice(&encode_header(
            write_length as u16,
            channel,
            sequence,
        ));
        self.buf[HEADER_LEN..write_length].copy_from_slice(data);
        Some(self.buf[..write_length].to_vec())
    }

    /// `_read(packet_byte_count - 4)`'s allocation rule (`i2c.py:94-103`):
    /// `total_read_length = (count - 4) + 4 = count`; if it exceeds 512 the
    /// buffer is REPLACED by a fresh zeroed `bytearray(total)`. Returns the
    /// `end` the following `readinto` uses.
    pub fn prepare_full_read(&mut self, packet_byte_count: u16) -> usize {
        let total = usize::from(packet_byte_count);
        if total > DATA_BUFFER_SIZE {
            self.buf = vec![0; total];
        }
        total
    }

    /// The `n` of the `read(2)` a `readinto(buffer, end=end)` issues:
    /// busio slices `memoryview(buffer)[0:end]` (`busio.py:207-213`), so
    /// `min(end, len)`.
    pub fn read_len(&self, end: usize) -> usize {
        end.min(self.buf.len())
    }

    /// Blinka's copy loop (`generic_linux/i2c.py:75-77`) over
    /// `buffer[0:end]`: `for i in range(n): buffer[i] = readin[i]`.
    /// - `WouldBlock` → `TypeError("'NoneType' object is not subscriptable")`
    ///   (only when `n > 0`; `range(0)` never indexes);
    /// - fewer bytes than `n` → the prefix is copied, then
    ///   `IndexError('index out of range')` (i2c-dev's 8192 clamp);
    /// - extra bytes are ignored.
    pub fn fill(&mut self, end: usize, got: ReadBytes<'_>) -> Result<(), PyExc> {
        let n = self.read_len(end);
        match got {
            ReadBytes::WouldBlock => {
                if n > 0 {
                    return Err(PyExc::msg(ExcKind::TypeError, NONE_NOT_SUBSCRIPTABLE));
                }
                Ok(())
            }
            ReadBytes::Data(bytes) => {
                let copied = n.min(bytes.len());
                self.buf[..copied].copy_from_slice(&bytes[..copied]);
                if copied < n {
                    return Err(PyExc::msg(ExcKind::IndexError, BYTES_INDEX));
                }
                Ok(())
            }
        }
    }

    /// `Packet.header_from_buffer(self._data_buffer)` (`i2c.py:55,65`).
    pub fn header(&self) -> PacketHeader {
        header_from_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
    }

    /// The first 4 bytes (what `_data_ready` judges).
    pub fn header_bytes(&self) -> [u8; 4] {
        [self.buf[0], self.buf[1], self.buf[2], self.buf[3]]
    }

    /// `Packet(self._data_buffer)` after the full read (`i2c.py:85`): parsed
    /// from the buffer, i.e. from the THIRD header.
    pub fn packet(&self) -> Packet {
        Packet::from_buffer(&self.buf)
    }

    /// The identity `_parse_sensor_id` reads with the part number (agent
    /// 0.2.2's `ready` line): `None` when `buffer[4] != 0xF8` or the part
    /// number is zero, else the first entry's part number, version and build.
    pub fn parse_firmware_id(&self) -> Option<FirmwareId> {
        let part = self.parse_sensor_id().filter(|p| *p != 0)?;
        let b = |i: usize| self.buf.get(i).copied().unwrap_or(0);
        Some(FirmwareId {
            part,
            major: b(6),
            minor: b(7),
            patch: u16::from_le_bytes([b(16), b(17)]),
            build: u32::from_le_bytes([b(12), b(13), b(14), b(15)]),
        })
    }

    /// `_parse_sensor_id` (`__init__.py:1003-1019`): reads the BUFFER, not
    /// the packet. `None` when `buffer[4] != 0xF8`, else the part number
    /// (`<I` at payload offset 4), which `_check_id` tests for truthiness
    /// (`:994-998`, review: a zero part number keeps waiting).
    pub fn parse_sensor_id(&self) -> Option<u32> {
        if self.buf[4] != crate::sh2::REPORT_PRODUCT_ID_RESPONSE {
            return None;
        }
        Some(u32::from_le_bytes([
            self.buf[8],
            self.buf[9],
            self.buf[10],
            self.buf[11],
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // WP1 mutation "continuation mask omitted": `count &= ~0x8000`
    // (`__init__.py:455`). 0x8012 must read as an 18-byte packet.
    #[test]
    fn header_clears_the_continuation_bit() {
        let h = header_from_bytes([0x12, 0x80, 3, 7]);
        assert_eq!(h.packet_byte_count, 0x12);
        assert_eq!(h.data_length, 14);
        assert_eq!((h.channel, h.sequence), (3, 7));
        assert_eq!(
            header_from_bytes([0xFF, 0xFF, 0, 0]).packet_byte_count,
            0x7FFF
        );
    }

    // WP1 mutation "mask applied AFTER the 0x7FFF check": `_data_ready`
    // tests the MASKED count (`i2c.py:109,113`), so a raw 0xFFFF prints the
    // error line(s) too.
    #[test]
    fn data_ready_tests_the_masked_count() {
        let r = data_ready([0xFF, 0xFF, 3, 0xFF]);
        assert!(!r.ready);
        assert_eq!(r.stdout, vec![BYTE_COUNT_ERROR_LINE, SEQUENCE_ERROR_LINE]);
        let r = data_ready([0xFF, 0x7F, 3, 0x00]);
        assert_eq!(r.stdout, vec![BYTE_COUNT_ERROR_LINE]);
    }

    // WP1 mutation "`max(0, count-4)` without the clamp": counts 1-3 give
    // data_length 0 (`__init__.py:458`), so `_data_ready` says not ready
    // (`i2c.py:119`).
    #[test]
    fn short_counts_clamp_data_length_to_zero() {
        for c in 0u16..=4 {
            let h = header_from_bytes(encode_header(c, 3, 0));
            assert_eq!(h.data_length, 0, "count {c}");
            assert!(!data_ready(encode_header(c, 3, 0)).ready, "count {c}");
        }
        assert!(data_ready(encode_header(5, 3, 0)).ready);
    }

    #[test]
    fn sequence_table_is_shared_and_wraps() {
        let mut t = SequenceTable::new();
        t.store_received(2, 255).unwrap();
        assert_eq!(t.get(2), Some(255));
        assert_eq!(t.advance_after_send(2), Some(0));
        assert_eq!(
            t.store_received(6, 1).unwrap_err().py_str(),
            "list assignment index out of range"
        );
    }

    #[test]
    fn buffer_replace_rule_can_shrink_but_not_below_512() {
        let mut b = ShtpBuffer::from_bytes(vec![0xEE; 1000]);
        assert_eq!(b.prepare_full_read(520), 520);
        assert_eq!(b.len(), 520);
        assert!(b.as_bytes().iter().all(|x| *x == 0));
        assert_eq!(b.prepare_full_read(18), 18);
        assert_eq!(b.len(), 520);
    }
}
