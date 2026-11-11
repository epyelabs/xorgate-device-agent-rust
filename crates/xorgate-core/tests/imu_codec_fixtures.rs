//! Phase 7c WP1: the pure BNO085 codec (`shtp`, `sh2`, `py_fmt`) against
//! `fixtures/imu_codec/*.json`. Every expected value in those files was
//! printed by CPython 3.11 running the REAL bench driver stack
//! (adafruit-circuitpython-bno08x 1.3.3, busdevice 5.2.17, Blinka 9.1.0
//! `generic_linux/i2c.py` + the two `busio.I2C` methods, PureIO 1.1.11),
//! faked only at the i2c-dev file object
//! (`tools/imu/codec-fixtures/gen_codec_fixtures.py`). Inputs are synthetic.
//!
//! Each test names the Python source it pins (bno08x paths are
//! `adafruit_bno08x/`, line numbers of 1.3.3).

mod common;

use std::collections::VecDeque;

use serde_json::Value;
use sha2::{Digest, Sha256};
use xorgate_core::imu_io::{I2cError, I2cStage};
use xorgate_core::py_fmt::*;
use xorgate_core::sh2::*;
use xorgate_core::shtp::*;

fn load(name: &str) -> Value {
    let text = common::fixture_text(&format!("imu_codec/{name}.json"));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["generatedBy"],
        "tools/imu/codec-fixtures/gen_codec_fixtures.py"
    );
    assert_eq!(v["synthetic"], true);
    v
}

fn unhex(v: &Value) -> Vec<u8> {
    hex::decode(v.as_str().unwrap()).unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn i(v: &Value) -> i64 {
    v.as_i64().unwrap()
}

fn strerror_table() -> Vec<(i32, String)> {
    load("py_text")["strerror"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (i(&r[0]) as i32, s(&r[1]).to_string()))
        .collect()
}

fn strerror(errno: i32) -> String {
    strerror_table()
        .into_iter()
        .find(|(e, _)| *e == errno)
        .unwrap()
        .1
}

// ---------------------------------------------------------------------------
// float repr: every i16 through the decoder, Q14 and Q8
// ---------------------------------------------------------------------------

/// Plan WP1: "every i16 in Q14 and Q8: decode(encode(k)) == k·2⁻ⁿ, with the
/// Python repr byte-equal". The decode runs through `process_report` on an
/// encoded RV / LA report (`__init__.py:223-244`, scalars `:127-136,159-160`),
/// and the repr stream's sha256 equals CPython's `repr(k * _Q_POINT_n_SCALAR)`
/// stream (`float_repr.json`).
#[test]
fn every_i16_decodes_exactly_and_reprs_like_cpython() {
    let f = load("float_repr");
    for (name, scalar, id) in [
        ("q14", Scalar::Q14, REPORT_ROTATION_VECTOR),
        ("q8", Scalar::Q8, REPORT_LINEAR_ACCELERATION),
    ] {
        let fx = &f[name];
        assert_eq!(i(&fx["count"]), 65_536);
        let mut reprs = Vec::with_capacity(65_536);
        for k in i16::MIN..=i16::MAX {
            let mut r = Readings::new();
            let (bytes, value) = if id == REPORT_ROTATION_VECTOR {
                let b = rotation_vector_report(1, 3, 0, [k, 0, 0, 0], 0x7fff);
                process_report(id, &b, &mut r).unwrap();
                (b, r.quaternion().unwrap()[0])
            } else {
                let b = linear_acceleration_report(1, 3, 0, [k, 0, 0]);
                process_report(id, &b, &mut r).unwrap();
                (b, r.linear_acceleration().unwrap()[0])
            };
            let exact = f64::from(k) * scalar.value();
            assert_eq!(value.to_bits(), exact.to_bits(), "{name} k={k}");
            assert_eq!(to_fixed(value, scalar), Some(k), "{name} inverse k={k}");
            assert_eq!(i16::from_le_bytes([bytes[4], bytes[5]]), k);
            reprs.push(float_repr(value));
        }
        for sample in fx["samples"].as_array().unwrap() {
            let k = i(&sample[0]);
            assert_eq!(reprs[(k + 32768) as usize], s(&sample[1]), "{name} k={k}");
        }
        let digest = hex::encode(Sha256::digest(reprs.join("\n").as_bytes()));
        assert_eq!(digest, s(&fx["sha256"]), "{name} repr stream");
    }
}

/// CPython `float_repr` on general doubles (exponent switch at 1e16 / 1e-4,
/// `e+NN`/`e-NN`, `.0`, `nan`/`inf`) and `json.dumps` (`NaN`, `Infinity`).
#[test]
fn general_float_repr_and_json_match_cpython() {
    for row in load("float_repr")["general"].as_array().unwrap() {
        let bits = u64::from_str_radix(s(&row[0]), 16).unwrap();
        let x = f64::from_bits(bits);
        assert_eq!(float_repr(x), s(&row[1]), "repr {x:e}");
        assert_eq!(json_float(x), s(&row[2]), "json {x:e}");
    }
}

/// splitmix64, mirroring `SplitMix64` in `gen_codec_fixtures.py`.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The generator's `fuzz_doubles()`, bit for bit: raw bit patterns, then
/// decimal-looking `d e e` values (both parsers are correctly rounded), then
/// dyadic `m · 2⁻ᵏ` (exact), which is where shortest-digit ties live.
fn fuzz_doubles(seed: u64, raw: usize, decimal: usize, dyadic: usize) -> Vec<f64> {
    let mut rng = SplitMix64(seed);
    let mut out = Vec::with_capacity(raw + decimal + dyadic);
    for _ in 0..raw {
        out.push(f64::from_bits(rng.next()));
    }
    for _ in 0..decimal {
        let n = 1 + (rng.next() % 17) as u32;
        let d = rng.next() % 10u64.pow(n);
        let e = (rng.next() % 51) as i64 - 25;
        let x: f64 = format!("{d}e{e}").parse().unwrap();
        out.push(if rng.next() & 1 == 1 { -x } else { x });
    }
    for _ in 0..dyadic {
        let m = (1u64 << 40) + rng.next() % ((1u64 << 53) - (1u64 << 40));
        let k = 1 + (rng.next() % 12) as i32;
        let x = m as f64 * 2f64.powi(-k);
        out.push(if rng.next() & 1 == 1 { -x } else { x });
    }
    out
}

/// review-wp1 F1: CPython `repr`/`json.dumps` over 2 000 000 doubles
/// (`float_fuzz.json`: sha256 of both streams + every 10 007th row),
/// including exact shortest-digit ties (dtoa mode 0 → the EVEN digit).
#[test]
fn float_fuzz_matches_cpython_repr_and_json() {
    let f = load("float_fuzz");
    assert_eq!(s(&f["prng"]), "splitmix64");
    let c = &f["counts"];
    let xs = fuzz_doubles(
        f["seed"].as_u64().unwrap(),
        i(&c["raw"]) as usize,
        i(&c["decimal"]) as usize,
        i(&c["dyadic"]) as usize,
    );
    assert!(xs.len() >= 1_000_000);
    let reprs: Vec<String> = xs.iter().map(|x| float_repr(*x)).collect();
    let jsons: Vec<String> = xs.iter().map(|x| json_float(*x)).collect();
    for row in f["samples"].as_array().unwrap() {
        let idx = i(&row[0]) as usize;
        assert_eq!(hex::encode(xs[idx].to_be_bytes()), s(&row[1]), "row {idx} bits");
        assert_eq!(reprs[idx], s(&row[2]), "row {idx} repr");
        assert_eq!(jsons[idx], s(&row[3]), "row {idx} json");
    }
    let rd = hex::encode(Sha256::digest(reprs.join("\n").as_bytes()));
    let jd = hex::encode(Sha256::digest(jsons.join("\n").as_bytes()));
    assert_eq!(rd, s(&f["reprSha256"]), "repr stream");
    assert_eq!(jd, s(&f["jsonSha256"]), "json stream");
}

// ---------------------------------------------------------------------------
// json.dumps lines and Python exception text
// ---------------------------------------------------------------------------

/// `json.dumps(str)` with `ensure_ascii` (`bno085_read.py:18`): `\n`, C0
/// controls as `\u00XX`, DEL as `\u007f`, quote/backslash, astral pairs.
#[test]
fn json_string_matches_ensure_ascii() {
    let t = load("py_text");
    let rows = t["jsonStrings"].as_array().unwrap();
    assert!(rows.len() >= 128);
    for row in rows {
        assert_eq!(json_string(s(&row[0])), s(&row[1]), "{:?}", s(&row[0]));
    }
}

/// `emit({"type": "ready", "address": hex(addr)})` (`bno085_read.py:64`).
#[test]
fn ready_lines_match() {
    for row in load("py_text")["readyLines"].as_array().unwrap() {
        assert_eq!(ready_line(i(&row[0])), s(&row[1]));
    }
}

/// The sample line (`bno085_read.py:80-81,92-96`): RV Q14 `(i, j, k, real)`
/// laid out `r, i, j, k`; LA Q8. Built encoder → decoder → line.
#[test]
fn sample_lines_match_from_encoded_reports() {
    let t = load("py_text");
    for row in t["sampleLines"].as_array().unwrap() {
        let r: Vec<i16> = row[0]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| i(x) as i16)
            .collect();
        let mut readings = Readings::new();
        let rv = rotation_vector_report(1, 3, 0, [r[0], r[1], r[2], r[3]], 0);
        let la = linear_acceleration_report(1, 3, 0, [r[4], r[5], r[6]]);
        process_report(REPORT_ROTATION_VECTOR, &rv, &mut readings).unwrap();
        process_report(REPORT_LINEAR_ACCELERATION, &la, &mut readings).unwrap();
        let line = sample_line(
            readings.quaternion().unwrap(),
            readings.linear_acceleration().unwrap(),
        );
        assert_eq!(line, s(&row[1]));
    }
    // The 0xFC zero seed (`__init__.py:854-859`): float literals 0.0.
    let mut readings = Readings::new();
    for id in [REPORT_ROTATION_VECTOR, REPORT_LINEAR_ACCELERATION] {
        let fc = get_feature_response(id, DEFAULT_REPORT_INTERVAL_US);
        process_report(REPORT_GET_FEATURE_RESPONSE, &fc, &mut readings).unwrap();
    }
    let line = sample_line(
        readings.quaternion().unwrap(),
        readings.linear_acceleration().unwrap(),
    );
    assert_eq!(line, s(&t["zeroSeedLine"]));
}

fn exc_kind(name: &str) -> ExcKind {
    match name {
        "RuntimeError" => ExcKind::RuntimeError,
        "ValueError" => ExcKind::ValueError,
        "IndexError" => ExcKind::IndexError,
        "TypeError" => ExcKind::TypeError,
        "PacketError" => ExcKind::PacketError,
        "error" => ExcKind::StructError,
        other => panic!("kind {other}"),
    }
}

/// `str(exc)` (`BaseException.__str__`): one arg → the arg; several → the
/// tuple repr with `", "`; `KeyError(n)` → `n`. The driver's texts:
/// `__init__.py:377,514,810,819,884,980`, busdevice `i2c_device.py:176`.
#[test]
fn exception_str_table_matches() {
    for row in load("py_text")["excStr"].as_array().unwrap() {
        let args: Vec<PyArg> = row[1]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| match a {
                Value::String(x) => PyArg::Str(x.clone()),
                Value::Number(n) => PyArg::Int(n.as_i64().unwrap()),
                other => panic!("{other}"),
            })
            .collect();
        let e = if s(&row[0]) == "KeyError" {
            match &args[0] {
                PyArg::Int(n) => PyExc::KeyError(*n),
                PyArg::Str(_) => panic!("str key"),
            }
        } else {
            PyExc::new(exc_kind(s(&row[0])), args)
        };
        assert_eq!(e.py_str(), s(&row[2]), "{row}");
    }
}

/// The interpreter-raised texts the codec reproduces as constants:
/// `i2c.py:70` (list store), Blinka `generic_linux/i2c.py:76-77` (bytes index,
/// `None[i]`), `__init__.py:370` (bytearray index), `:253,294` (list load).
#[test]
fn interpreter_raised_texts_match() {
    let want = [
        ("list_assignment", "IndexError", LIST_ASSIGNMENT_INDEX),
        ("bytes_index", "IndexError", BYTES_INDEX),
        ("bytearray_index", "IndexError", BYTEARRAY_INDEX),
        ("list_index", "IndexError", LIST_INDEX),
        ("none_subscript", "TypeError", NONE_NOT_SUBSCRIPTABLE),
    ];
    let rows = load("py_text")["raised"].clone();
    for (row, (label, kind, text)) in rows.as_array().unwrap().iter().zip(want) {
        assert_eq!((s(&row[0]), s(&row[1]), s(&row[2])), (label, kind, text));
    }
}

/// `struct.unpack_from` length errors (CPython 3.11 `_struct.c`), raised by
/// the driver's decoders on a truncated slice (`__init__.py:234,239,248,264,
/// 287-291,312-316,330-331`).
#[test]
fn struct_error_texts_match() {
    for row in load("py_text")["structErrors"].as_array().unwrap() {
        let size = match s(&row[0]) {
            "<B" => 1,
            "<h" | "<H" => 2,
            "<I" => 4,
            "<BBBBB" => 5,
            "<BBBBBBBBB" => 9,
            "<BBBBBBBBBBB" => 11,
            "<BBBHIII" => 17,
            f => panic!("{f}"),
        };
        let e = PyExc::struct_unpack_from(size, i(&row[2]) as usize, i(&row[1]) as usize);
        assert_eq!(e.py_str(), s(&row[3]));
    }
}

/// `OSError(errno, strerror[, filename])` str (`[Errno N] text[: 'path']`),
/// as PureIO's `open` (`smbus.py:140`) and the i2c-dev ops raise it. The
/// strerror table itself (glibc, bookworm) is WP3's HAL pin.
#[test]
fn oserror_texts_match() {
    let t = load("py_text");
    for row in t["osErrorStr"].as_array().unwrap() {
        let e = i(&row[0]) as i32;
        let err = I2cError {
            stage: I2cStage::Transfer,
            errno: e,
            strerror: strerror(e),
        };
        assert_eq!(err.to_py("/dev/i2c-1").py_str(), s(&row[1]));
    }
    for row in t["osErrorPathStr"].as_array().unwrap() {
        let e = i(&row[0]) as i32;
        let err = I2cError {
            stage: I2cStage::Open,
            errno: e,
            strerror: strerror(e),
        };
        assert_eq!(err.to_py("/dev/i2c-1").py_str(), s(&row[1]));
    }
    let enoent = PyExc::os_path(2, &strerror(2), "/nonexistent-p7c/i2c-1");
    assert_eq!(enoent.py_str(), s(&t["openEnoent"][1]));
}

/// The helper's two f-strings (`bno085_read.py:61,86`).
#[test]
fn helper_error_texts_match() {
    for row in load("py_text")["helperTexts"].as_array().unwrap() {
        let got = match s(&row[0]) {
            "init" => init_failed_text(i(&row[1]), s(&row[2])),
            _ => read_failed_text(i(&row[1]) as u32, s(&row[2])),
        };
        assert_eq!(got, s(&row[3]));
    }
    // busdevice `i2c_device.py:176`, incl. Python's `0x-5` (review-wp1 F5).
    for row in load("py_text")["noDeviceTexts"].as_array().unwrap() {
        assert_eq!(no_i2c_device_text(i(&row[0])), s(&row[1]));
    }
}

// ---------------------------------------------------------------------------
// SHTP framing (real i2c.py / Packet code)
// ---------------------------------------------------------------------------

/// `Packet.header_from_buffer` (`__init__.py:451-461`) on 176 headers:
/// continuation bit, 0x7FFF/0xFFFF, counts 0-5, > 512, channels up to 255.
#[test]
fn headers_match() {
    let rows = load("shtp")["headers"].clone();
    assert!(rows.as_array().unwrap().len() >= 150);
    for row in rows.as_array().unwrap() {
        let b = unhex(&row[0]);
        let h = header_from_bytes([b[0], b[1], b[2], b[3]]);
        assert_eq!(
            (
                i64::from(h.channel),
                i64::from(h.sequence),
                h.data_length as i64,
                i64::from(h.packet_byte_count)
            ),
            (i(&row[1]), i(&row[2]), i(&row[3]), i(&row[4])),
            "{}",
            s(&row[0])
        );
        // The inverse encoder writes the count back as given.
        assert_eq!(
            encode_header(u16::from_le_bytes([b[0], b[1]]), b[2], b[3]).to_vec(),
            b
        );
    }
}

enum Step {
    Data(Vec<u8>),
    WouldBlock,
    Os(i32),
}

fn script(v: &Value) -> VecDeque<Step> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| match x {
            Value::String(h) => Step::Data(hex::decode(h).unwrap()),
            Value::Null => Step::WouldBlock,
            Value::Array(a) => Step::Os(a[1].as_i64().unwrap() as i32),
            other => panic!("{other}"),
        })
        .collect()
}

/// `I2CDevice.readinto(buffer, end=end)` → busio's memoryview slice →
/// Blinka's copy loop over ONE `read(2)` (`i2c_device.py:64-79`,
/// `busio.py:207-213`, `generic_linux/i2c.py:70-77`, `smbus.py:164-170`).
fn readinto(
    buf: &mut ShtpBuffer,
    end: usize,
    steps: &mut VecDeque<Step>,
    reads: &mut Vec<usize>,
) -> Result<(), PyExc> {
    reads.push(buf.read_len(end));
    match steps.pop_front().expect("script exhausted") {
        Step::Data(b) => buf.fill(end, ReadBytes::Data(&b)),
        Step::WouldBlock => buf.fill(end, ReadBytes::WouldBlock),
        Step::Os(e) => Err(PyExc::os(e, &strerror(e))),
    }
}

/// `BNO08X_I2C._read_packet` (`i2c.py:59-91`) composed from the codec, as
/// WP2's client will: header read, seq store BEFORE the empty check,
/// `PacketError` on count 0, `_read` with the replace rule, Packet from the
/// buffer (the THIRD header), seq store again from it.
fn read_packet(
    buf: &mut ShtpBuffer,
    seq: &mut SequenceTable,
    steps: &mut VecDeque<Step>,
    reads: &mut Vec<usize>,
) -> Result<Packet, PyExc> {
    readinto(buf, HEADER_LEN, steps, reads)?; // :60-61
    let h = buf.header(); // :65
    seq.store_received(h.channel, h.sequence)?; // :70
    if h.packet_byte_count == 0 {
        return Err(PyExc::msg(ExcKind::PacketError, "No packet available")); // :71-73
    }
    let end = buf.prepare_full_read(h.packet_byte_count); // :74,83,94-103
    readinto(buf, end, steps, reads)?; // :104-105
    let p = buf.packet(); // :85
    seq.store_received(p.header.channel, p.header.sequence)?; // :89, __init__.py:824-827
    Ok(p)
}

/// `_read_packet` on 24 scripted cases, incl. 2nd/3rd header disagreement,
/// a third header beyond the buffer (stale bytes), > 512 (buffer REPLACED,
/// also shrinking 1000 → 520), the 8192 clamp on a `0x7FFF`/`0xFFFF` second
/// header (`index out of range`, buffer 32767 partly filled), EAGAIN
/// (`TypeError`), EIO, a short read, counts 1-5 (review F2/F5), and a full
/// read failing at exactly 512 (stale buffer KEPT) vs 513 (replaced): the
/// `> 512` threshold (review-wp1 F4).
#[test]
fn read_packet_matches_the_real_driver() {
    let rows = load("shtp")["readPackets"].clone();
    assert_eq!(rows.as_array().unwrap().len(), 24);
    for row in rows.as_array().unwrap() {
        let label = s(&row["label"]);
        let len_before = i(&row["bufferLenBefore"]) as usize;
        // The generator's starting buffer: the 0xA0.. ramp, or zeros.
        let start: Vec<u8> = match s(&row["start"]) {
            "ramp" => (0..len_before).map(|n| (0xA0 + n) as u8).collect(),
            "zeros" => vec![0; len_before],
            other => panic!("{other}"),
        };
        assert_eq!(
            hex::encode(&start[..16]),
            s(&row["bufferBefore"]),
            "{label}"
        );
        let mut buf = ShtpBuffer::from_bytes(start);
        let seqs: Vec<u8> = row["seqBefore"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| i(x) as u8)
            .collect();
        let mut seq = SequenceTable(seqs.try_into().unwrap());
        let mut steps = script(&row["script"]);
        let mut reads = Vec::new();
        match read_packet(&mut buf, &mut seq, &mut steps, &mut reads) {
            Ok(p) => {
                let want = &row["packet"];
                assert!(!want.is_null(), "{label}: Python raised {}", row["exc"]);
                assert_eq!(i64::from(p.header.channel), i(&want["channel"]), "{label}");
                assert_eq!(i64::from(p.header.sequence), i(&want["seq"]), "{label}");
                assert_eq!(
                    p.header.data_length as i64,
                    i(&want["dataLength"]),
                    "{label}"
                );
                assert_eq!(
                    i64::from(p.header.packet_byte_count),
                    i(&want["count"]),
                    "{label}"
                );
                assert_eq!(hex::encode(&p.data), s(&want["data"]), "{label}");
            }
            Err(e) => {
                assert!(
                    row["packet"].is_null(),
                    "{label}: Rust raised {}",
                    e.py_str()
                );
                assert_eq!(e.py_str(), s(&row["exc"][1]), "{label}");
            }
        }
        let want_reads: Vec<usize> = row["reads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| i(x) as usize)
            .collect();
        assert_eq!(reads, want_reads, "{label} read sizes");
        let want_seq: Vec<u8> = row["seqAfter"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| i(x) as u8)
            .collect();
        assert_eq!(seq.0.to_vec(), want_seq, "{label} seq table");
        assert_eq!(buf.len() as i64, i(&row["bufferLen"]), "{label} buffer len");
        assert_eq!(
            hex::encode(Sha256::digest(buf.as_bytes())),
            s(&row["bufferSha256"]),
            "{label} buffer"
        );
    }
}

/// `_data_ready` (`i2c.py:107-122`) on 66 headers: the two stdout lines on a
/// MASKED `0x7FFF` (raw `0x7FFF` or `0xFFFF`), the `0xFF` sequence line, not
/// ready for counts ≤ 4; plus its read failing (EAGAIN → `TypeError`, a short
/// read → `IndexError`, EIO → `OSError`).
#[test]
fn data_ready_matches_the_real_driver() {
    for row in load("shtp")["dataReady"].as_array().unwrap() {
        if let Some(label) = row["label"].as_str() {
            let mut buf = ShtpBuffer::new();
            let mut steps = match label {
                "eagain" => VecDeque::from([Step::WouldBlock]),
                "short2" => VecDeque::from([Step::Data(vec![0x05, 0x00])]),
                "eio" => VecDeque::from([Step::Os(5)]),
                other => panic!("{other}"),
            };
            let mut reads = Vec::new();
            let e = readinto(&mut buf, HEADER_LEN, &mut steps, &mut reads).unwrap_err();
            assert_eq!(e.py_str(), s(&row["exc"][1]), "{label}");
            assert_eq!(
                hex::encode(&buf.as_bytes()[..8]),
                s(&row["bufferHead"]),
                "{label}"
            );
            continue;
        }
        let raw = unhex(&row["read"]);
        let r = data_ready([raw[0], raw[1], raw[2], raw[3]]);
        assert_eq!(
            r.ready,
            row["ready"].as_bool().unwrap(),
            "{}",
            s(&row["read"])
        );
        let printed: String = r.stdout.iter().map(|l| format!("{l}{PRINT_END}")).collect();
        assert_eq!(printed, s(&row["stdout"]), "{}", s(&row["read"]));
    }
}

/// `_send_packet` (`i2c.py:30-46`): header + data staged INTO the shared
/// buffer (stale tail kept), `buffer[0:write_length]` written, then the
/// shared sequence byte incremented mod 256 and returned.
#[test]
fn send_framing_matches_the_real_driver() {
    for row in load("shtp")["sends"].as_array().unwrap() {
        let ch = i(&row["channel"]) as u8;
        let data = unhex(&row["data"]);
        let seqs: Vec<u8> = row["seqBefore"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| i(x) as u8)
            .collect();
        let mut seq = SequenceTable(seqs.try_into().unwrap());
        let stale: Vec<u8> = (0..512).map(|n| (n % 256) as u8).collect();
        let mut buf = ShtpBuffer::from_bytes(stale);
        let written = buf.stage_send(ch, seq.get(ch).unwrap(), &data).unwrap();
        let ret = seq.advance_after_send(ch).unwrap();
        assert_eq!(hex::encode(&written), s(&row["written"][0]));
        assert_eq!(i64::from(ret), i(&row["ret"]));
        let want: Vec<u8> = row["seqAfter"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| i(x) as u8)
            .collect();
        assert_eq!(seq.0.to_vec(), want);
        assert_eq!(hex::encode(&buf.as_bytes()[..32]), s(&row["bufferHead"]));
    }
}

// ---------------------------------------------------------------------------
// SH-2 reports (real __init__.py code)
// ---------------------------------------------------------------------------

/// `_report_length` (`__init__.py:358-362`) for all 256 ids: the sensor and
/// control tables (`:140-170`) and `KeyError(id)` for the rest.
#[test]
fn report_lengths_match_for_all_ids() {
    let rows = load("sh2")["reportLengths"].clone();
    assert_eq!(rows.as_array().unwrap().len(), 256);
    for row in rows.as_array().unwrap() {
        let id = i(&row[0]) as u8;
        match report_length(id) {
            Ok(n) => assert_eq!(n as i64, i(&row[1]), "id {id:#x}"),
            Err(e) => {
                assert!(row[1].is_null(), "id {id:#x}");
                assert_eq!(e.py_str(), s(&row[2]));
            }
        }
    }
}

/// `_AVAIL_SENSOR_REPORTS` / `_RAW_REPORTS` (`__init__.py:149-170`):
/// Q14 RV, Q8 LA (not Q12), Q12 only for the geomagnetic RV.
#[test]
fn sensor_table_matches() {
    for row in load("sh2")["sensorTable"].as_array().unwrap() {
        let id = i(&row[0]) as u8;
        let spec = sensor_spec(id).unwrap();
        let scalar_repr = match spec.scalar {
            Scalar::One => "1".to_string(),
            s => float_repr(s.value()),
        };
        assert_eq!(scalar_repr, s(&row[1]), "id {id:#x}");
        assert_eq!(spec.count as i64, i(&row[2]));
        assert_eq!(spec.len as i64, i(&row[3]));
        assert_eq!(is_raw_report(id), row[4].as_bool().unwrap());
    }
}

/// `_get_feature_enable_report` (`__init__.py:933-945`) incl. 0x05/0x04 at
/// 50 000 µs and the activity classifier's 0x1FF.
#[test]
fn set_feature_bytes_match() {
    for row in load("sh2")["setFeature"].as_array().unwrap() {
        let (id, interval, specific) = (i(&row[0]) as u8, i(&row[1]) as u32, i(&row[2]) as u32);
        assert_eq!(
            hex::encode(set_feature_command(id, interval, specific)),
            s(&row[3])
        );
        let enable_shape =
            specific == 0 || (id == REPORT_ACTIVITY_CLASSIFIER && specific == ENABLED_ACTIVITIES);
        if enable_shape && interval == DEFAULT_REPORT_INTERVAL_US {
            assert_eq!(hex::encode(enable_feature_report(id, interval)), s(&row[3]));
        }
    }
}

/// `_handle_packet` (`__init__.py:829-837`) over 42 packet sequences with a
/// PERSISTENT pending list: LIFO (oldest duplicate wins), leftovers from a
/// failed batch processed last (F1), `KeyError` / `Unprocessable Batch
/// bytes` / `IndexError` / `struct.error` texts, `0xFC` zero seeds at any
/// time (F7), the self-reset advertisement + reset-complete, every sensor id,
/// and `print(packet)` itself raising on an empty channel-2/3 packet or a
/// channel > 5 (review-wp1 F2).
#[test]
fn batches_match_the_real_driver() {
    let rows = load("sh2")["batches"].clone();
    assert!(rows.as_array().unwrap().len() >= 25);
    for seq in rows.as_array().unwrap() {
        let name = s(&seq["name"]);
        let (mut pending, mut readings) = (Vec::new(), Readings::new());
        for (n, step) in seq["steps"].as_array().unwrap().iter().enumerate() {
            let packet = Packet::from_buffer(&unhex(&step["packet"]));
            let res = handle_packet(&packet, &mut pending, &mut readings);
            match (&res, step["exc"].is_null()) {
                (Ok(()), true) => {}
                (Err(e), false) => assert_eq!(e.py_str(), s(&step["exc"][1]), "{name}[{n}]"),
                _ => panic!("{name}[{n}]: rust {res:?} vs python {}", step["exc"]),
            }
            // Python printed the D80 dump iff it raised AND `str(packet)`
            // did not raise itself (review-wp1 F2).
            let dumped = res.is_err() && packet_str_error(&packet).is_none();
            assert_eq!(dumped, step["dumped"].as_bool().unwrap(), "{name}[{n}]");
            let want_pending: Vec<&str> =
                step["pending"].as_array().unwrap().iter().map(s).collect();
            let got_pending: Vec<String> = pending.iter().map(|p| hex::encode(&p.bytes)).collect();
            assert_eq!(got_pending, want_pending, "{name}[{n}] pending");
            let want = step["readings"].as_object().unwrap();
            assert_eq!(readings.0.len(), want.len(), "{name}[{n}] reading ids");
            for (id, entry) in want {
                let r = readings
                    .get(id.parse().unwrap())
                    .unwrap_or_else(|| panic!("{name}[{n}] id {id}"));
                match s(&entry["kind"]) {
                    "other" => assert_eq!(r, &Reading::Opaque, "{name}[{n}] id {id}"),
                    _ => assert_eq!(
                        r.py_repr().as_deref(),
                        Some(s(&entry["repr"])),
                        "{name}[{n}] id {id}"
                    ),
                }
            }
        }
        assert!(pending.iter().all(|p| p.id == p.bytes[0]));
    }
}

/// `_parse_sensor_report_data` (`__init__.py:223-244`): words from offset 4,
/// `<h`, times the scalar; the accuracy is the low 2 bits of byte 2.
#[test]
fn sensor_decode_matches() {
    for row in load("sh2")["decode"].as_array().unwrap() {
        let b = unhex(&row[0]);
        let mut r = Readings::new();
        process_report(b[0], &b, &mut r).unwrap();
        let reprs: Vec<String> = match r.get(b[0]).unwrap() {
            Reading::Floats(v) => v.iter().map(|x| float_repr(*x)).collect(),
            other => panic!("{other:?}"),
        };
        let want: Vec<&str> = row[1].as_array().unwrap().iter().map(s).collect();
        assert_eq!(reprs, want);
        assert_eq!(i64::from(b[2] & 0b11), i(&row[2]));
    }
}

/// The scrubber/W0 inverse path: a sample's values encode to RV + LA reports
/// and a framed packet that decodes back to the identical NDJSON line.
#[test]
fn inverse_encoder_round_trips_a_sample_line() {
    let quat = [6.103515625e-05, -0.5, 0.25, 0.8125];
    let lin = [-1.5, 0.00390625, 9.80078125];
    let (q, a) = encode_sample(quat, lin).unwrap();
    let mut payload = base_timestamp_report(0x10);
    payload.extend(rotation_vector_report(7, 3, 0, q, 0));
    payload.extend(linear_acceleration_report(7, 3, 0, a));
    let frame = encode_packet(CHANNEL_INPUT_SENSOR_REPORTS, 9, &payload, false);
    let (mut pending, mut r) = (Vec::new(), Readings::new());
    handle_packet(&Packet::from_buffer(&frame), &mut pending, &mut r).unwrap();
    assert_eq!(
        sample_line(r.quaternion().unwrap(), r.linear_acceleration().unwrap()),
        sample_line(quat, lin)
    );
    assert_eq!(
        sample_line(quat, lin),
        "{\"type\": \"sample\", \"quat\": {\"r\": 0.8125, \"i\": 6.103515625e-05, \"j\": -0.5, \"k\": 0.25}, \"linaccel\": {\"x\": -1.5, \"y\": 0.00390625, \"z\": 9.80078125}}"
    );
}
