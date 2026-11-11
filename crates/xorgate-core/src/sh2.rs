//! SH-2 report layer for the Rust BNO085 reader (phase 7c WP1), ported from
//! `adafruit_bno08x` 1.3.3 `__init__.py` (the bench's version, `wp0.md`).
//!
//! - **Report tables:** `_AVAIL_SENSOR_REPORTS` (`:154-170`: scalar, word
//!   count, report length), the control-report lengths `_REPORT_LENGTHS`
//!   (`:140-147`), `_RAW_REPORTS` (`:149-153`, unsigned words).
//! - **Fixed point:** rotation vector = **Q14** (4 words i, j, k, real),
//!   linear acceleration = **Q8** (3 words) (`:127-138,159-160`). Q12 is only
//!   the unused geomagnetic RV (`:161`); the RV's accuracy-estimate word
//!   (bytes 12-13) is never read (count 4).
//! - **Batches:** `_separate_batch` (`:365-383`) walks the payload by report
//!   length into a CALLER-OWNED pending list ([`separate_batch`]); an unknown
//!   id raises `KeyError(id)`, a short remainder
//!   `RuntimeError('Unprocessable Batch bytes', n)` — AFTER the earlier slices
//!   were appended. `_handle_packet` (`:829-837`) then pops the list LIFO, so
//!   the OLDEST duplicate wins, and slices left by a failed batch stay in the
//!   list and are processed LAST, after the next packet's (plan P2 items 2-3,
//!   review F1). The pending list is client state (`__init__.py:487`).
//! - **Zero seeds:** every `0xFC` get-feature response stores
//!   `_INITIAL_REPORTS.get(id, (0.0, 0.0, 0.0))` (`:854-859`), at enable time
//!   AND mid-stream (plan P2 item 1, review F7).
//! - **Encoders:** set-feature (`:933-945`), the product-id request
//!   (`:986-990`), the soft reset (`:1051-1053`), and the INVERSE encoders
//!   (sample → RV + LA reports) used to build synthetic fixtures. The inverse
//!   is lossless: every published value is `k·2⁻ⁿ`.
//!
//! Pinned by `fixtures/imu_codec/sh2.json`, printed by the REAL driver code
//! (`tools/imu/codec-fixtures/gen_codec_fixtures.py`).

use std::collections::BTreeMap;

use crate::py_fmt::{float_repr, ExcKind, PyExc, BYTEARRAY_INDEX, LIST_INDEX};
use crate::shtp::Packet;

// Report ids (`__init__.py:59-111`).
pub const REPORT_GET_FEATURE_REQUEST: u8 = 0xFE;
pub const REPORT_SET_FEATURE_COMMAND: u8 = 0xFD;
pub const REPORT_GET_FEATURE_RESPONSE: u8 = 0xFC;
pub const REPORT_BASE_TIMESTAMP: u8 = 0xFB;
pub const REPORT_TIMESTAMP_REBASE: u8 = 0xFA;
pub const REPORT_PRODUCT_ID_RESPONSE: u8 = 0xF8;
pub const REPORT_PRODUCT_ID_REQUEST: u8 = 0xF9;
pub const REPORT_COMMAND_REQUEST: u8 = 0xF2;
pub const REPORT_COMMAND_RESPONSE: u8 = 0xF1;
pub const REPORT_ACCELEROMETER: u8 = 0x01;
pub const REPORT_GYROSCOPE: u8 = 0x02;
pub const REPORT_MAGNETOMETER: u8 = 0x03;
pub const REPORT_LINEAR_ACCELERATION: u8 = 0x04;
pub const REPORT_ROTATION_VECTOR: u8 = 0x05;
pub const REPORT_GRAVITY: u8 = 0x06;
pub const REPORT_GAME_ROTATION_VECTOR: u8 = 0x08;
pub const REPORT_GEOMAGNETIC_ROTATION_VECTOR: u8 = 0x09;
pub const REPORT_STEP_COUNTER: u8 = 0x11;
pub const REPORT_STABILITY_CLASSIFIER: u8 = 0x13;
pub const REPORT_RAW_ACCELEROMETER: u8 = 0x14;
pub const REPORT_RAW_GYROSCOPE: u8 = 0x15;
pub const REPORT_RAW_MAGNETOMETER: u8 = 0x16;
pub const REPORT_SHAKE_DETECTOR: u8 = 0x19;
pub const REPORT_ACTIVITY_CLASSIFIER: u8 = 0x1E;

/// `_SAVE_DCD` / `_ME_CALIBRATE` command codes (`__init__.py:80-81`).
pub const COMMAND_SAVE_DCD: u8 = 0x06;
pub const COMMAND_ME_CALIBRATE: u8 = 0x07;

/// `_DEFAULT_REPORT_INTERVAL` = 50 ms in µs (`__init__.py:118`).
pub const DEFAULT_REPORT_INTERVAL_US: u32 = 50_000;
/// `_ENABLED_ACTIVITIES` (`__init__.py:191`).
pub const ENABLED_ACTIVITIES: u32 = 0x1FF;

/// The product-id request payload `[0xF9, 0]` (`_check_id`, `__init__.py:986-990`), channel 2.
pub const PRODUCT_ID_REQUEST: [u8; 2] = [REPORT_PRODUCT_ID_REQUEST, 0];
/// The soft-reset payload `[1]` (`soft_reset`, `__init__.py:1051-1053`), channel 1 (EXE), sent twice.
pub const SOFT_RESET: [u8; 1] = [1];

/// A sensor report's fixed-point scalar (`__init__.py:127-138`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scalar {
    Q14,
    Q12,
    Q9,
    Q8,
    Q4,
    /// The integer `1` (`(1, n, len)` rows): raw/classifier reports.
    One,
}

impl Scalar {
    /// `2 ** (q * -1)`, exact.
    pub fn value(self) -> f64 {
        match self {
            Scalar::Q14 => 2f64.powi(-14),
            Scalar::Q12 => 2f64.powi(-12),
            Scalar::Q9 => 2f64.powi(-9),
            Scalar::Q8 => 2f64.powi(-8),
            Scalar::Q4 => 2f64.powi(-4),
            Scalar::One => 1.0,
        }
    }
}

/// One `_AVAIL_SENSOR_REPORTS` row (`__init__.py:154-170`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SensorSpec {
    pub scalar: Scalar,
    pub count: usize,
    pub len: usize,
}

/// `_AVAIL_SENSOR_REPORTS[id]` (`__init__.py:154-170`).
pub fn sensor_spec(id: u8) -> Option<SensorSpec> {
    let (scalar, count, len) = match id {
        REPORT_ACCELEROMETER => (Scalar::Q8, 3, 10),
        REPORT_GRAVITY => (Scalar::Q8, 3, 10),
        REPORT_GYROSCOPE => (Scalar::Q9, 3, 10),
        REPORT_MAGNETOMETER => (Scalar::Q4, 3, 10),
        REPORT_LINEAR_ACCELERATION => (Scalar::Q8, 3, 10),
        REPORT_ROTATION_VECTOR => (Scalar::Q14, 4, 14),
        REPORT_GEOMAGNETIC_ROTATION_VECTOR => (Scalar::Q12, 4, 14),
        REPORT_GAME_ROTATION_VECTOR => (Scalar::Q14, 4, 12),
        REPORT_STEP_COUNTER => (Scalar::One, 1, 12),
        REPORT_SHAKE_DETECTOR => (Scalar::One, 1, 6),
        REPORT_STABILITY_CLASSIFIER => (Scalar::One, 1, 6),
        REPORT_ACTIVITY_CLASSIFIER => (Scalar::One, 1, 16),
        REPORT_RAW_ACCELEROMETER => (Scalar::One, 3, 16),
        REPORT_RAW_GYROSCOPE => (Scalar::One, 3, 16),
        REPORT_RAW_MAGNETOMETER => (Scalar::One, 3, 16),
        _ => return None,
    };
    Some(SensorSpec { scalar, count, len })
}

/// `id in _RAW_REPORTS` (`__init__.py:149-153`): unsigned `<H` words.
pub fn is_raw_report(id: u8) -> bool {
    matches!(
        id,
        REPORT_RAW_ACCELEROMETER | REPORT_RAW_GYROSCOPE | REPORT_RAW_MAGNETOMETER
    )
}

/// `_REPORT_LENGTHS[id]` (`__init__.py:140-147`).
pub fn control_report_length(id: u8) -> Option<usize> {
    match id {
        REPORT_PRODUCT_ID_RESPONSE => Some(16),
        REPORT_GET_FEATURE_RESPONSE => Some(17),
        REPORT_COMMAND_RESPONSE => Some(16),
        REPORT_BASE_TIMESTAMP => Some(5),
        REPORT_TIMESTAMP_REBASE => Some(5),
        _ => None,
    }
}

/// `_report_length(report_id)` (`__init__.py:358-362`): ids below `0xF0` from
/// the sensor table, the rest from the control table; unknown → `KeyError(id)`.
pub fn report_length(id: u8) -> Result<usize, PyExc> {
    let len = if id < 0xF0 {
        sensor_spec(id).map(|s| s.len)
    } else {
        control_report_length(id)
    };
    len.ok_or(PyExc::KeyError(i64::from(id)))
}

/// One `[report_slice[0], report_slice]` entry of `_packet_slices`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportSlice {
    pub id: u8,
    /// `packet.data[i:i + required]` — Python slicing, so SHORTER than the
    /// report length when the packet data was truncated.
    pub bytes: Vec<u8>,
}

/// `_separate_batch(packet, report_slices)` (`__init__.py:365-383`). Appends
/// to the caller's `pending` list as it goes; on error the slices already
/// appended STAY (review F1).
pub fn separate_batch(packet: &Packet, pending: &mut Vec<ReportSlice>) -> Result<(), PyExc> {
    let data_length = packet.header.data_length;
    let mut next = 0usize;
    while next < data_length {
        // `packet.data[next_byte_index]` (:370); data may be shorter than
        // data_length when the header claimed more than the buffer holds.
        let id = *packet
            .data
            .get(next)
            .ok_or_else(|| PyExc::msg(ExcKind::IndexError, BYTEARRAY_INDEX))?;
        let required = report_length(id)?;
        let unprocessed = data_length - next;
        if unprocessed < required {
            return Err(PyExc::msg_int(
                ExcKind::RuntimeError,
                "Unprocessable Batch bytes",
                unprocessed as i64,
            ));
        }
        let end = (next + required).min(packet.data.len());
        pending.push(ReportSlice {
            id,
            bytes: packet.data[next..end].to_vec(),
        });
        next += required;
    }
    Ok(())
}

/// One `_readings[id]` value. The reader publishes only the `Floats` of
/// `0x05` and `0x04`; the others are kept so presence and errors match.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    /// A tuple of Python floats (`raw * scalar`, or a `0.0` seed).
    Floats(Vec<f64>),
    /// A tuple of Python ints (raw reports, scalar `1`).
    Ints(Vec<i64>),
    /// A plain int (the step counter, `__init__.py:247-248,901-903`).
    Int(i64),
    /// A str / dict / bool reading (stability, activity, shake): never
    /// published; only its presence matters.
    Opaque,
}

impl Reading {
    /// `repr(value)` for the non-opaque kinds (fixture comparison).
    pub fn py_repr(&self) -> Option<String> {
        fn tuple(items: Vec<String>) -> String {
            if items.len() == 1 {
                format!("({},)", items[0])
            } else {
                format!("({})", items.join(", "))
            }
        }
        match self {
            Reading::Floats(v) => Some(tuple(v.iter().map(|x| float_repr(*x)).collect())),
            Reading::Ints(v) => Some(tuple(v.iter().map(|x| x.to_string()).collect())),
            Reading::Int(n) => Some(n.to_string()),
            Reading::Opaque => None,
        }
    }
}

/// `self._readings` (`__init__.py:500`): the cached value per report id.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Readings(pub BTreeMap<u8, Reading>);

impl Readings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, id: u8) -> bool {
        self.0.contains_key(&id)
    }

    pub fn get(&self, id: u8) -> Option<&Reading> {
        self.0.get(&id)
    }

    /// The `quaternion` property's return after its drain
    /// (`__init__.py:525-532`): Adafruit order `(i, j, k, real)`.
    pub fn quaternion(&self) -> Result<[f64; 4], PyExc> {
        match self.0.get(&REPORT_ROTATION_VECTOR) {
            Some(Reading::Floats(v)) if v.len() == 4 => Ok([v[0], v[1], v[2], v[3]]),
            _ => Err(PyExc::msg(
                ExcKind::RuntimeError,
                "No quaternion report found, is it enabled?",
            )),
        }
    }

    /// The `linear_acceleration` property's return after its drain
    /// (`__init__.py:564-572`): `(x, y, z)`.
    pub fn linear_acceleration(&self) -> Result<[f64; 3], PyExc> {
        match self.0.get(&REPORT_LINEAR_ACCELERATION) {
            Some(Reading::Floats(v)) if v.len() == 3 => Ok([v[0], v[1], v[2]]),
            _ => Err(PyExc::msg(
                ExcKind::RuntimeError,
                "No lin. accel report found, is it enabled?",
            )),
        }
    }
}

/// `struct.unpack_from(fmt, buf, offset)`'s length check: the
/// `struct.error` text when `offset + size > len`.
fn need(buf: &[u8], offset: usize, size: usize) -> Result<(), PyExc> {
    if buf.len() < offset + size {
        return Err(PyExc::struct_unpack_from(size, offset, buf.len()));
    }
    Ok(())
}

fn le_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

/// `_INITIAL_REPORTS.get(id, (0.0, 0.0, 0.0))` (`__init__.py:171-189,857-859`).
pub fn initial_reading(feature_id: u8) -> Reading {
    match feature_id {
        REPORT_ACTIVITY_CLASSIFIER | REPORT_STABILITY_CLASSIFIER => Reading::Opaque,
        REPORT_ROTATION_VECTOR
        | REPORT_GAME_ROTATION_VECTOR
        | REPORT_GEOMAGNETIC_ROTATION_VECTOR => Reading::Floats(vec![0.0; 4]),
        _ => Reading::Floats(vec![0.0; 3]),
    }
}

/// `_process_report(report_id, report_bytes)` (`__init__.py:886-930`), with
/// `_handle_control_report` (`:839-861`) and `_handle_command_response`
/// (`:863-884`). Raises exactly where the driver raises: `struct.error` on a
/// short slice, `IndexError` on an out-of-table classifier,
/// `RuntimeError('Unable to save calibration data')`.
pub fn process_report(id: u8, bytes: &[u8], readings: &mut Readings) -> Result<(), PyExc> {
    if id >= 0xF0 {
        match id {
            REPORT_PRODUCT_ID_RESPONSE => {
                // parse_sensor_id (:307-318): <B@2, <B@3, <H@12, <I@4, <I@8.
                need(bytes, 2, 1)?;
                need(bytes, 3, 1)?;
                need(bytes, 12, 2)?;
                need(bytes, 4, 4)?;
                need(bytes, 8, 4)?;
            }
            REPORT_GET_FEATURE_RESPONSE => {
                // "<BBBHIII" (:263-264) = 17 bytes; the feature id is byte 1.
                need(bytes, 0, 17)?;
                readings.0.insert(bytes[1], initial_reading(bytes[1]));
            }
            REPORT_COMMAND_RESPONSE => {
                // "<BBBBB" then "<BBBBBBBBBBB" at 5 (:321-332).
                need(bytes, 0, 5)?;
                need(bytes, 5, 11)?;
                let (command, status) = (bytes[2], bytes[5]);
                if command == COMMAND_SAVE_DCD && status != 0 {
                    return Err(PyExc::msg(
                        ExcKind::RuntimeError,
                        "Unable to save calibration data",
                    ));
                }
            }
            _ => {} // 0xFB / 0xFA: no handler.
        }
        return Ok(());
    }
    match id {
        REPORT_STEP_COUNTER => {
            need(bytes, 8, 2)?; // <H@8 (:247-248)
            readings
                .0
                .insert(id, Reading::Int(i64::from(le_u16(bytes, 8))));
        }
        REPORT_SHAKE_DETECTOR => {
            // <H@4 (:302-304). The reading is only replaced when the current
            // one is falsy (:907-912); the only ways a 0x19 reading appears
            // (a 0xFC seed tuple) are truthy, so it never changes.
            need(bytes, 4, 2)?;
        }
        REPORT_STABILITY_CLASSIFIER => {
            need(bytes, 4, 1)?; // <B@4, then a 5-entry list (:251-253)
            if bytes[4] > 4 {
                return Err(PyExc::msg(ExcKind::IndexError, LIST_INDEX));
            }
            readings.0.insert(id, Reading::Opaque);
        }
        REPORT_ACTIVITY_CLASSIFIER => {
            // <B@4, <B@5, 9×<B@6, then activities[most_likely] (9 entries) (:274-299).
            need(bytes, 4, 1)?;
            need(bytes, 5, 1)?;
            need(bytes, 6, 9)?;
            if bytes[5] > 8 {
                return Err(PyExc::msg(ExcKind::IndexError, LIST_INDEX));
            }
            readings.0.insert(id, Reading::Opaque);
        }
        _ => {
            // _parse_sensor_report_data (:223-244): accuracy <B@2 (read,
            // dropped), then `count` words <h (<H for raw) from offset 4.
            let spec = sensor_spec(id).ok_or(PyExc::KeyError(i64::from(id)))?;
            need(bytes, 2, 1)?;
            let raw = is_raw_report(id);
            let mut words = Vec::with_capacity(spec.count);
            for i in 0..spec.count {
                let off = 4 + 2 * i;
                need(bytes, off, 2)?;
                let w = le_u16(bytes, off);
                words.push(if raw {
                    i64::from(w)
                } else {
                    i64::from(w as i16)
                });
            }
            let reading = match spec.scalar {
                Scalar::One => Reading::Ints(words),
                s => Reading::Floats(words.iter().map(|w| *w as f64 * s.value()).collect()),
            };
            readings.0.insert(id, reading);
        }
    }
    Ok(())
}

/// `_handle_packet(packet)` (`__init__.py:829-837`): separate into the
/// persistent `pending` list, then pop LIFO until empty. On error Python
/// runs `print(packet)` and re-raises. That `print` is the multi-line hex
/// dump (D80: the Rust reader does not reproduce it), UNLESS `Packet.__str__`
/// itself raises first ([`packet_str_error`]); then THAT exception replaces
/// the original one and nothing is printed (review-wp1 F2).
///
/// So `Err` means "Python raised"; Python printed the dump iff, in addition,
/// `packet_str_error(packet)` is `None`.
pub fn handle_packet(
    packet: &Packet,
    pending: &mut Vec<ReportSlice>,
    readings: &mut Readings,
) -> Result<(), PyExc> {
    let result = separate_then_process(packet, pending, readings);
    match result {
        Ok(()) => Ok(()),
        Err(original) => Err(packet_str_error(packet).unwrap_or(original)),
    }
}

fn separate_then_process(
    packet: &Packet,
    pending: &mut Vec<ReportSlice>,
    readings: &mut Readings,
) -> Result<(), PyExc> {
    separate_batch(packet, pending)?;
    while let Some(slice) = pending.pop() {
        process_report(slice.id, &slice.bytes, readings)?;
    }
    Ok(())
}

/// The exception `str(packet)` raises, if any (`Packet.__str__`,
/// `__init__.py:394-439`, in evaluation order):
/// - `channels[self.channel_number]` (`:400-403`, `debug.py:7-14`) →
///   `KeyError(channel)` for a channel > 5 (the driver's sequence store
///   rejects those earlier, `i2c.py:70`; reachable only through this API);
/// - `self.report_id` = `self.data[0]` (`:404-408,441-444`) on channel 2 or 3
///   → `IndexError('bytearray index out of range')` for an EMPTY packet.
///
/// The other branches are guarded by `len(self.data) >= 6` / `in reports`.
pub fn packet_str_error(packet: &Packet) -> Option<PyExc> {
    let ch = packet.header.channel;
    if usize::from(ch) >= crate::shtp::CHANNEL_COUNT {
        return Some(PyExc::KeyError(i64::from(ch)));
    }
    let control_or_input =
        ch == crate::shtp::CHANNEL_CONTROL || ch == crate::shtp::CHANNEL_INPUT_SENSOR_REPORTS;
    if control_or_input && packet.data.is_empty() {
        return Some(PyExc::msg(ExcKind::IndexError, BYTEARRAY_INDEX));
    }
    None
}

/// `_get_feature_enable_report(feature_id, report_interval,
/// sensor_specific_config)` (`__init__.py:933-945`): 17 bytes,
/// `[0xFD, id, 0, 0, 0, interval u32le @5, 0 @9, sensor-specific u32le @13]`.
pub fn set_feature_command(
    feature_id: u8,
    report_interval_us: u32,
    sensor_specific: u32,
) -> [u8; 17] {
    let mut r = [0u8; 17];
    r[0] = REPORT_SET_FEATURE_COMMAND;
    r[1] = feature_id;
    r[5..9].copy_from_slice(&report_interval_us.to_le_bytes());
    r[13..17].copy_from_slice(&sensor_specific.to_le_bytes());
    r
}

/// `enable_feature`'s report choice (`__init__.py:958-963`): the activity
/// classifier carries `_ENABLED_ACTIVITIES`, every other feature 0.
pub fn enable_feature_report(feature_id: u8, report_interval_us: u32) -> [u8; 17] {
    let specific = if feature_id == REPORT_ACTIVITY_CLASSIFIER {
        ENABLED_ACTIVITIES
    } else {
        0
    };
    set_feature_command(feature_id, report_interval_us, specific)
}

// ---------------------------------------------------------------------------
// Inverse encoders (fixtures only): synthetic reports the decoder maps back
// to exactly the given values.
// ---------------------------------------------------------------------------

/// An input report: `[id, seq, status, delay, words…] + tail`.
pub fn encode_input_report(
    id: u8,
    seq: u8,
    status: u8,
    delay: u8,
    words: &[i16],
    tail: &[u8],
) -> Vec<u8> {
    let mut out = vec![id, seq, status, delay];
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out.extend_from_slice(tail);
    out
}

/// A rotation-vector report (14 bytes): Q14 words in Adafruit order
/// `(i, j, k, real)`, then the accuracy-estimate word (never read).
pub fn rotation_vector_report(
    seq: u8,
    status: u8,
    delay: u8,
    ijkr: [i16; 4],
    accuracy: i16,
) -> Vec<u8> {
    encode_input_report(
        REPORT_ROTATION_VECTOR,
        seq,
        status,
        delay,
        &ijkr,
        &accuracy.to_le_bytes(),
    )
}

/// A linear-acceleration report (10 bytes): Q8 words `(x, y, z)`.
pub fn linear_acceleration_report(seq: u8, status: u8, delay: u8, xyz: [i16; 3]) -> Vec<u8> {
    encode_input_report(REPORT_LINEAR_ACCELERATION, seq, status, delay, &xyz, &[])
}

/// A base-timestamp report (5 bytes): `[0xFB, delta u32le]`.
pub fn base_timestamp_report(delta: u32) -> Vec<u8> {
    let mut out = vec![REPORT_BASE_TIMESTAMP];
    out.extend_from_slice(&delta.to_le_bytes());
    out
}

/// A get-feature response (17 bytes, `"<BBBHIII"`), as the sensor answers a
/// set-feature: `[0xFC, id, flags, sensitivity u16, interval u32, batch u32, specific u32]`.
pub fn get_feature_response(feature_id: u8, report_interval_us: u32) -> Vec<u8> {
    let mut out = vec![REPORT_GET_FEATURE_RESPONSE, feature_id, 0, 0, 0];
    out.extend_from_slice(&report_interval_us.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    out
}

/// A product-id response (16 bytes): `[0xF8, reset cause, major, minor,
/// part u32, build u32, patch u16, 0, 0]` (`parse_sensor_id`, `:307-318`).
pub fn product_id_response(part: u32, major: u8, minor: u8, patch: u16, build: u32) -> Vec<u8> {
    let mut out = vec![REPORT_PRODUCT_ID_RESPONSE, 0, major, minor];
    out.extend_from_slice(&part.to_le_bytes());
    out.extend_from_slice(&build.to_le_bytes());
    out.extend_from_slice(&patch.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

/// The raw word `k` with `k · scalar == v` exactly, if one exists.
pub fn to_fixed(v: f64, scalar: Scalar) -> Option<i16> {
    let k = v / scalar.value();
    if !k.is_finite()
        || k.fract() != 0.0
        || !(f64::from(i16::MIN)..=f64::from(i16::MAX)).contains(&k)
    {
        return None;
    }
    let k = k as i16;
    (f64::from(k) * scalar.value() == v).then_some(k)
}

/// A published sample → its raw RV (Q14, Adafruit `(i, j, k, real)`) and LA
/// (Q8) words; `None` if any value is not exactly representable.
pub fn encode_sample(quat_ijkr: [f64; 4], linaccel: [f64; 3]) -> Option<([i16; 4], [i16; 3])> {
    let mut q = [0i16; 4];
    for (o, v) in q.iter_mut().zip(quat_ijkr) {
        *o = to_fixed(v, Scalar::Q14)?;
    }
    let mut a = [0i16; 3];
    for (o, v) in a.iter_mut().zip(linaccel) {
        *o = to_fixed(v, Scalar::Q8)?;
    }
    Some((q, a))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shtp::encode_packet;

    fn pkt(payload: &[u8]) -> Packet {
        Packet::from_buffer(&encode_packet(3, 0, payload, false))
    }

    // WP1 mutation "Q8 → Q12 for linaccel": `_AVAIL_SENSOR_REPORTS[0x04]`
    // = (_Q_POINT_8_SCALAR, 3, 10) (`__init__.py:159`).
    #[test]
    fn linear_acceleration_is_q8() {
        let mut r = Readings::new();
        let la = linear_acceleration_report(1, 3, 0, [256, -512, 1]);
        process_report(REPORT_LINEAR_ACCELERATION, &la, &mut r).unwrap();
        assert_eq!(r.linear_acceleration().unwrap(), [1.0, -2.0, 0.00390625]);
        let rv = rotation_vector_report(1, 3, 0, [16384, -16384, 1, -32768], 0x1234);
        process_report(REPORT_ROTATION_VECTOR, &rv, &mut r).unwrap();
        assert_eq!(r.quaternion().unwrap(), [1.0, -1.0, 6.103515625e-05, -2.0]);
    }

    // WP1 mutation "FIFO instead of LIFO": `_packet_slices.pop()`
    // (`__init__.py:833-834`, comment :927-929): the OLDEST duplicate wins.
    #[test]
    fn duplicates_in_one_batch_resolve_lifo_oldest_wins() {
        let mut payload = rotation_vector_report(1, 3, 0, [11, 12, 13, 14], 0);
        payload.extend(rotation_vector_report(2, 3, 0, [21, 22, 23, 24], 0));
        let (mut pending, mut r) = (Vec::new(), Readings::new());
        handle_packet(&pkt(&payload), &mut pending, &mut r).unwrap();
        let q14 = Scalar::Q14.value();
        assert_eq!(
            r.quaternion().unwrap(),
            [11.0 * q14, 12.0 * q14, 13.0 * q14, 14.0 * q14]
        );
    }

    // Leftover slices persist and are processed LAST (review F1; WP2
    // scenario 17's shape): FB + 05 + 04 + a 1-byte remainder, then a good 05.
    #[test]
    fn leftover_slices_from_a_failed_batch_win_over_the_next_packet() {
        let mut bad = base_timestamp_report(1);
        bad.extend(rotation_vector_report(1, 3, 0, [51, 52, 53, 54], 0));
        bad.extend(linear_acceleration_report(1, 2, 0, [5, 6, 7]));
        bad.push(REPORT_ROTATION_VECTOR);
        let (mut pending, mut r) = (Vec::new(), Readings::new());
        let e = handle_packet(&pkt(&bad), &mut pending, &mut r).unwrap_err();
        assert_eq!(e.py_str(), "('Unprocessable Batch bytes', 1)");
        assert_eq!(pending.len(), 3);
        let good = rotation_vector_report(2, 3, 0, [61, 62, 63, 64], 0);
        handle_packet(&pkt(&good), &mut pending, &mut r).unwrap();
        let q14 = Scalar::Q14.value();
        assert_eq!(r.quaternion().unwrap()[0], 51.0 * q14, "the stale RV wins");
        assert!(pending.is_empty());
    }

    // review-wp1 F2: `_handle_packet`'s `print(packet)` (`__init__.py:836`)
    // raises itself for an EMPTY channel-2/3 packet (`self.report_id`,
    // `:404-408,442-444`), replacing the leftover's error; nothing printed.
    #[test]
    fn empty_control_packet_error_comes_from_print_packet() {
        let mut bad = encode_input_report(REPORT_STABILITY_CLASSIFIER, 1, 0, 0, &[9], &[]);
        bad.extend([0x07; 10]);
        let (mut pending, mut r) = (Vec::new(), Readings::new());
        let e = handle_packet(&pkt(&bad), &mut pending, &mut r).unwrap_err();
        assert_eq!(e, PyExc::KeyError(7));
        assert_eq!(pending.len(), 1);
        let empty = |ch: u8| Packet::from_buffer(&encode_packet(ch, 0, &[], false));
        let mut p2 = pending.clone();
        let e = handle_packet(&empty(3), &mut p2, &mut r).unwrap_err();
        assert_eq!(e.py_str(), "bytearray index out of range");
        assert!(packet_str_error(&empty(3)).is_some());
        let e = handle_packet(&empty(4), &mut pending, &mut r).unwrap_err();
        assert_eq!(e.py_str(), "list index out of range");
        assert!(packet_str_error(&empty(4)).is_none());
    }

    #[test]
    fn unknown_id_is_a_key_error_with_the_decimal_id() {
        let (mut pending, mut r) = (Vec::new(), Readings::new());
        let e = handle_packet(&pkt(&[0x07; 10]), &mut pending, &mut r).unwrap_err();
        assert_eq!(e, PyExc::KeyError(7));
        assert_eq!(e.py_str(), "7");
    }

    #[test]
    fn get_feature_response_seeds_zeros_any_time() {
        let mut r = Readings::new();
        let rv = rotation_vector_report(1, 3, 0, [1, 2, 3, 4], 0);
        process_report(REPORT_ROTATION_VECTOR, &rv, &mut r).unwrap();
        let fc = get_feature_response(REPORT_ROTATION_VECTOR, DEFAULT_REPORT_INTERVAL_US);
        process_report(REPORT_GET_FEATURE_RESPONSE, &fc, &mut r).unwrap();
        assert_eq!(r.quaternion().unwrap(), [0.0; 4]);
    }

    // The set-feature bytes for 0x05/0x04 at 50 000 µs (plan WP1 tests).
    #[test]
    fn set_feature_bytes_at_50ms() {
        let want = |id: u8| {
            let mut v = vec![0xFD, id, 0, 0, 0, 0x50, 0xC3, 0, 0];
            v.extend([0u8; 8]);
            v
        };
        assert_eq!(enable_feature_report(0x05, 50_000).to_vec(), want(0x05));
        assert_eq!(enable_feature_report(0x04, 50_000).to_vec(), want(0x04));
    }

    #[test]
    fn encode_sample_round_trips_and_rejects_non_grid_values() {
        let (q, a) = encode_sample(
            [1.0, -2.0, 6.103515625e-05, 0.0],
            [127.99609375, -128.0, 0.0],
        )
        .unwrap();
        assert_eq!(q, [16384, -32768, 1, 0]);
        assert_eq!(a, [32767, -32768, 0]);
        assert!(encode_sample([0.1, 0.0, 0.0, 0.0], [0.0; 3]).is_none());
        assert!(encode_sample([2.5, 0.0, 0.0, 0.0], [0.0; 3]).is_none());
    }
}
