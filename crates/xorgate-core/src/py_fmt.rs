//! Python text formatting for the Rust BNO085 reader (phase 7c WP1).
//!
//! The reader replaces `python/bno085_read.py` (sha256 `7af7a856…`), whose
//! stdout is the agent's IMU contract (plan `phase-7c-rust-imu.md` P1/P2). It
//! must therefore print BYTE-identical lines:
//!
//! - `emit(obj)` = `print(json.dumps(obj), flush=True)` (`bno085_read.py:17-18`):
//!   separators `", "` / `": "`, `ensure_ascii=True`, floats through
//!   `float.__repr__`, and `print`'s `"\n"` ([`PRINT_END`]);
//! - error texts are Python exception `str()`s inside two f-strings
//!   (`bno085_read.py:61,86`): one arg → the arg; several → the tuple repr
//!   (`('Was not able to enable feature', 5)`); `KeyError(n)` → `n`;
//!   `OSError` → `[Errno N] <strerror>` with an optional `: '<path>'`.
//!
//! Every function here is pinned by `fixtures/imu_codec/{float_repr,py_text}.json`,
//! printed by CPython 3.11 (`tools/imu/codec-fixtures/gen_codec_fixtures.py`).
//!
//! **Float repr.** Published values are `k·2⁻¹⁴` (|k| ≤ 32768) or `k·2⁻⁸`, so
//! the shortest round-trip digits ARE the exact decimal; the only exponent
//! form that occurs is ±`6.103515625e-05` (plan P2). The formatter is
//! nevertheless the general CPython rule (`repr` switches to exponent form
//! for a decimal exponent < -4 or ≥ 16, prints a 2-digit signed exponent and
//! adds `.0` to integral fixed-point values); Rust's `{:e}` supplies the same
//! shortest round-trip digits as CPython's `dtoa` mode 0.

use std::fmt::Write as _;

/// What `print()` appends to every line (`bno085_read.py:18`).
pub const PRINT_END: &str = "\n";

/// `IndexError` text of a `list` store out of range (`[0]*6; l[6] = x`):
/// `i2c.py:70` (`self._sequence_number[channel_number] = …`, channel > 5).
pub const LIST_ASSIGNMENT_INDEX: &str = "list assignment index out of range";
/// `IndexError` text of a `bytes` index out of range: Blinka's copy loop
/// (`generic_linux/i2c.py:76-77`, `buffer[i + start] = readin[i]`) on a
/// short read, e.g. i2c-dev's 8192-byte clamp.
pub const BYTES_INDEX: &str = "index out of range";
/// `IndexError` text of a `bytearray` index out of range:
/// `_separate_batch` (`__init__.py:370`, `packet.data[next_byte_index]`).
pub const BYTEARRAY_INDEX: &str = "bytearray index out of range";
/// `IndexError` text of a `list` load out of range: the stability and
/// activity classifier tables (`__init__.py:253,294`).
pub const LIST_INDEX: &str = "list index out of range";
/// `TypeError` text when CPython's `FileIO.read` returned `None` on `EAGAIN`
/// and Blinka indexes it (`generic_linux/i2c.py:75-77`; review F2).
pub const NONE_NOT_SUBSCRIPTABLE: &str = "'NoneType' object is not subscriptable";

/// **PROVISIONAL** (plan fix-check 2026-11-11): the `busio.I2C` text when
/// `/dev/i2c-1` is missing (ENOENT) on BCM2712 + Blinka 9.1.0. DERIVED FROM
/// SOURCE, NEVER RUN: `busio.py:169-182` (9.1.0) swallows generic_linux's
/// `RuntimeError` (`generic_linux/i2c.py:34-39`) and formats
/// `{(scl, sda)}` / `{i2cPorts}` with `Pin.__repr__ = str(self.id)`
/// (`generic_linux/lgpio_pin.py:70-71`) over `bcm2712/pin.py:13-15,96-100`.
/// It becomes final only when WP2 oracle scenario 22 reproduces it.
pub const BUS_MISSING_BCM2712_PROVISIONAL: &str = "No Hardware I2C on (scl,sda)=(3, 2)\nValid I2C ports: ((1, 3, 2), (0, 1, 0), (10, 45, 44)).\nMake sure I2C is enabled.";

/// Python `repr(float)` (CPython `float_repr`, `'r'` mode, short style).
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    // `{:e}` = the shortest round-trip digits: "-6.103515625e-5", "1e16".
    let sci = format!("{x:e}");
    let (neg, body) = match sci.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, sci.as_str()),
    };
    let (mant, exp) = body.split_once('e').unwrap_or((body, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if !(-4..16).contains(&exp) {
        // Exponent form: d[.ddd]e±XX (at least two exponent digits).
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let sign = if exp < 0 { '-' } else { '+' };
        let _ = write!(out, "e{sign}{:02}", exp.abs());
        return out;
    }
    let n = digits.len() as i32;
    let decpt = exp + 1; // digits before the decimal point
    if decpt <= 0 {
        out.push_str("0.");
        for _ in 0..(-decpt) {
            out.push('0');
        }
        out.push_str(&digits);
    } else if decpt < n {
        out.push_str(&digits[..decpt as usize]);
        out.push('.');
        out.push_str(&digits[decpt as usize..]);
    } else {
        out.push_str(&digits);
        for _ in 0..(decpt - n) {
            out.push('0');
        }
        out.push_str(".0");
    }
    out
}

/// `json.dumps(float)`: `float.__repr__`, except the `allow_nan` spellings.
pub fn json_float(x: f64) -> String {
    if x.is_nan() {
        "NaN".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        float_repr(x)
    }
}

/// `json.dumps(str)` with `ensure_ascii=True` (CPython
/// `c_encode_basestring_ascii`): `"`/`\\` escaped, the five short escapes
/// (`\b \f \n \r \t`), every other char outside `' '..='~'` as lowercase
/// `\uXXXX` (DEL → `\u007f`), astral chars as a surrogate pair.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{u:04x}");
                }
            }
        }
    }
    out.push('"');
    out
}

/// Python `repr(str)`. Exact for ASCII (the only texts the reader formats);
/// a non-ASCII char is kept as-is except the C1 controls (`\x80`-`\x9f`).
pub fn str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python `hex(n)` (`bno085_read.py:61,64`): `0x4a`, `-0x5`.
pub fn py_hex(n: i64) -> String {
    if n < 0 {
        format!("-0x{:x}", n.unsigned_abs())
    } else {
        format!("0x{n:x}")
    }
}

/// One positional argument of a Python exception.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PyArg {
    Str(String),
    Int(i64),
}

impl PyArg {
    /// `repr(arg)`.
    pub fn repr(&self) -> String {
        match self {
            PyArg::Str(s) => str_repr(s),
            PyArg::Int(n) => n.to_string(),
        }
    }
    /// `str(arg)`.
    pub fn str(&self) -> String {
        match self {
            PyArg::Str(s) => s.clone(),
            PyArg::Int(n) => n.to_string(),
        }
    }
}

impl From<&str> for PyArg {
    fn from(s: &str) -> Self {
        PyArg::Str(s.to_string())
    }
}

impl From<i64> for PyArg {
    fn from(n: i64) -> Self {
        PyArg::Int(n)
    }
}

/// The Python exception classes the reader's paths raise. Only `str()` is
/// observable (`bno085_read.py:61,86`), but the class decides how it is
/// built and which `except` catches it (`PacketError`, `OSError`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcKind {
    RuntimeError,
    ValueError,
    IndexError,
    TypeError,
    /// `struct.error` (`unpack_from` on a short slice).
    StructError,
    /// `adafruit_bno08x.PacketError` (`__init__.py:212-215`).
    PacketError,
}

/// A Python exception value, reduced to what decides its `str()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PyExc {
    /// `Kind(*args)`; `BaseException.__str__`: no arg → `""`, one → `str(arg)`,
    /// more → `repr(args)`.
    Args { kind: ExcKind, args: Vec<PyArg> },
    /// `KeyError(key)`: `str()` is `repr(key)` (`_report_length`,
    /// `__init__.py:358-362`).
    KeyError(i64),
    /// `OSError(errno, strerror[, filename])`. `strerror` is glibc's text,
    /// supplied by the HAL (`strerror_r`) or the scripted transport.
    OsError {
        errno: i32,
        strerror: String,
        filename: Option<String>,
    },
}

impl PyExc {
    pub fn new(kind: ExcKind, args: Vec<PyArg>) -> Self {
        PyExc::Args { kind, args }
    }
    /// `Kind("msg")`.
    pub fn msg(kind: ExcKind, msg: &str) -> Self {
        PyExc::Args {
            kind,
            args: vec![msg.into()],
        }
    }
    /// `Kind("msg", n)`, e.g. `RuntimeError("Unprocessable Batch bytes", n)`.
    pub fn msg_int(kind: ExcKind, msg: &str, n: i64) -> Self {
        PyExc::Args {
            kind,
            args: vec![msg.into(), n.into()],
        }
    }
    /// `struct.error` raised by `unpack_from` when `offset + size > len`
    /// (CPython 3.11 `Struct_unpack_from_impl`).
    pub fn struct_unpack_from(size: usize, offset: usize, len: usize) -> Self {
        PyExc::msg(
            ExcKind::StructError,
            &format!(
                "unpack_from requires a buffer of at least {} bytes for unpacking {size} bytes at offset {offset} (actual buffer size is {len})",
                offset + size
            ),
        )
    }
    pub fn os(errno: i32, strerror: &str) -> Self {
        PyExc::OsError {
            errno,
            strerror: strerror.to_string(),
            filename: None,
        }
    }
    pub fn os_path(errno: i32, strerror: &str, filename: &str) -> Self {
        PyExc::OsError {
            errno,
            strerror: strerror.to_string(),
            filename: Some(filename.to_string()),
        }
    }
    /// The class, for `except` matching (`OsError`/`KeyError` have their own).
    pub fn kind(&self) -> Option<ExcKind> {
        match self {
            PyExc::Args { kind, .. } => Some(*kind),
            _ => None,
        }
    }
    /// `str(exc)`.
    pub fn py_str(&self) -> String {
        match self {
            PyExc::Args { args, .. } => match args.len() {
                0 => String::new(),
                1 => args[0].str(),
                _ => {
                    let parts: Vec<String> = args.iter().map(PyArg::repr).collect();
                    format!("({})", parts.join(", "))
                }
            },
            PyExc::KeyError(k) => k.to_string(),
            PyExc::OsError {
                errno,
                strerror,
                filename,
            } => match filename {
                Some(f) => format!("[Errno {errno}] {strerror}: {}", str_repr(f)),
                None => format!("[Errno {errno}] {strerror}"),
            },
        }
    }
}

/// busdevice's probe failure (`i2c_device.py:176`).
pub fn no_i2c_device_text(address: i64) -> String {
    format!("No I2C device at address: 0x{address:x}")
}

/// `f"init failed at {hex(addr)}: {exc}"` (`bno085_read.py:61`).
pub fn init_failed_text(address: i64, exc: &str) -> String {
    format!("init failed at {}: {exc}", py_hex(address))
}

/// `f"read failed {consecutive_errors}x: {exc}"` (`bno085_read.py:86`).
pub fn read_failed_text(consecutive_errors: u32, exc: &str) -> String {
    format!("read failed {consecutive_errors}x: {exc}")
}

/// `json.dumps({"type": "ready", "address": hex(addr)})` (`bno085_read.py:64`).
pub fn ready_line(address: i64) -> String {
    format!(
        "{{\"type\": \"ready\", \"address\": {}}}",
        json_string(&py_hex(address))
    )
}

/// `json.dumps({"type": "error", "error": text})` (`bno085_read.py:61,85-86`).
pub fn error_line(text: &str) -> String {
    format!("{{\"type\": \"error\", \"error\": {}}}", json_string(text))
}

/// The sample line (`bno085_read.py:80-81,92-96`). `quat` is in ADAFRUIT
/// order `(i, j, k, real)` (`qi, qj, qk, qr = bno.quaternion`); the line
/// lays it out `r, i, j, k`. `linaccel` is `(x, y, z)`.
pub fn sample_line(quat: [f64; 4], linaccel: [f64; 3]) -> String {
    let [qi, qj, qk, qr] = quat;
    let [ax, ay, az] = linaccel;
    format!(
        "{{\"type\": \"sample\", \"quat\": {{\"r\": {}, \"i\": {}, \"j\": {}, \"k\": {}}}, \"linaccel\": {{\"x\": {}, \"y\": {}, \"z\": {}}}}}",
        json_float(qr),
        json_float(qi),
        json_float(qj),
        json_float(qk),
        json_float(ax),
        json_float(ay),
        json_float(az)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // WP1 mutations "`e-05` printed as `e-5`" and "an integral float printed
    // without `.0`": both are pinned here and by every row of the
    // CPython-printed `float_repr.json`.
    #[test]
    fn float_repr_exponent_has_two_digits_and_sign() {
        // repr(1 * 2**-14) / repr(-1 * 2**-14) (__init__.py:127, plan P2).
        assert_eq!(float_repr(6.103515625e-05), "6.103515625e-05");
        assert_eq!(float_repr(-6.103515625e-05), "-6.103515625e-05");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1.5e-7), "1.5e-07");
        assert_eq!(float_repr(0.0001), "0.0001");
    }

    #[test]
    fn float_repr_integral_values_keep_dot_zero() {
        // repr(16384 * 2**-14) == '1.0'; repr(-32768 * 2**-14) == '-2.0'.
        assert_eq!(float_repr(1.0), "1.0");
        assert_eq!(float_repr(-2.0), "-2.0");
        assert_eq!(float_repr(0.0), "0.0");
        assert_eq!(float_repr(100.0), "100.0");
        assert_eq!(float_repr(9999999999999998.0), "9999999999999998.0");
    }

    // WP1 mutation "the tuple repr without the space after the comma":
    // str(RuntimeError("Was not able to enable feature", 5))
    // (`__init__.py:980`; plan P5 "Feature enable times out").
    #[test]
    fn exception_tuple_repr_has_comma_space() {
        let e = PyExc::msg_int(ExcKind::RuntimeError, "Was not able to enable feature", 5);
        assert_eq!(e.py_str(), "('Was not able to enable feature', 5)");
        let e = PyExc::msg_int(ExcKind::RuntimeError, "Unprocessable Batch bytes", 1);
        assert_eq!(e.py_str(), "('Unprocessable Batch bytes', 1)");
        assert_eq!(PyExc::KeyError(250).py_str(), "250");
    }

    #[test]
    fn json_string_escapes_like_ensure_ascii() {
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
        assert_eq!(json_string("\u{7f}"), "\"\\u007f\"");
        assert_eq!(json_string("\u{1F600}"), "\"\\ud83d\\ude00\"");
    }

    #[test]
    fn provisional_bus_missing_constant_is_three_lines() {
        assert_eq!(BUS_MISSING_BCM2712_PROVISIONAL.lines().count(), 3);
        assert_eq!(
            error_line(&init_failed_text(0x4a, BUS_MISSING_BCM2712_PROVISIONAL)),
            "{\"type\": \"error\", \"error\": \"init failed at 0x4a: No Hardware I2C on (scl,sda)=(3, 2)\\nValid I2C ports: ((1, 3, 2), (0, 1, 0), (10, 45, 44)).\\nMake sure I2C is enabled.\"}"
        );
    }
}
