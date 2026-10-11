//! Record/replay tape — `phase-2-cloud-plane.md` §Record/replay (README §5.3).
//!
//! A tape is NDJSON, normally gzip-compressed (`.ndjson.gz`). The first line is
//! the [`TapeMeta`] header (`{"v":1,"k":"meta",…}`); every other line is one
//! [`TapeRecord`]: `{"t":<mono ms since tape start>,"w":<wall ms>,"k":<kind>,…}`.
//! The Rust agent writes one when `XORGATE_RS_TAPE=<path>` is set: WP1's spine
//! calls [`crate::io::open_file_tape`], which returns a [`FileTape`] as its
//! [`crate::io::TapeSink`]; every HAL reader reports the raw input at the point
//! of reading. `xorgate-replay` reads tapes back to drive the rig.
//!
//! Tapes are NEVER committed except the shifted ≤60 s smoke tape (decision O8):
//! a bench tape carries the device's location and SIM identity.
//!
//! Recording rules (the table in the phase file):
//! - `nmea` / `imu` lines are recorded verbatim, before parsing;
//! - `lte_file` and `exec` are recorded only when the content changes (per file /
//!   per argv), so the agent reports every read and the writer de-duplicates;
//! - `dt` is recorded once;
//! - `mqtt_in` is every inbound publish, retained redeliveries included;
//! - `clock` is written by the agent every 10 s so a wall step is visible even on
//!   a quiet tape.
//!
//! A tape must never hurt the agent: the first write error logs one warning and
//! disables the tape for the rest of the run. Nothing here panics on I/O.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Tape format version (the meta line's `v`).
pub const TAPE_VERSION: u32 = 1;
/// Env var that turns the tape on in the Rust agent (`XORGATE_RS_TAPE=<path>`).
pub const TAPE_ENV: &str = "XORGATE_RS_TAPE";
/// How often the writer forces a gzip sync-flush, so a tape cut by SIGKILL or a
/// reboot stays readable up to the last few seconds.
pub const FLUSH_EVERY_MS: u64 = 2_000;

/// Env keys copied verbatim into the meta line (the TS input knobs; README
/// common brief rule 6). Values are paths/modes, not secrets.
pub const META_ENV_VERBATIM: &[&str] = &[
    "MODE",
    "OFFLINE",
    "RATE_HZ",
    "MQTT_KEEPALIVE_SEC",
    "GPS_NMEA_PORT",
    "LTE_TELEMETRY_FILE",
    "XORGATE_PYTHON_DIR",
    "KVS_MANIFEST",
    "HARDWARE_MANIFEST",
    "KVS_PIPELINE_DIR",
    "XORGATE_HOME",
    "XORGATE_UPLOAD_BUCKET",
];
/// Env keys whose presence is recorded but whose value is replaced by
/// `"<redacted>"` (identity and credential locations).
pub const META_ENV_REDACTED: &[&str] = &[
    "DEVICE_ID",
    "IOT_DATA_ENDPOINT",
    "CERT_PATH",
    "KEY_PATH",
    "CA_PATH",
];

/// The `"k":"meta"` tag of the header line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MetaTag {
    #[default]
    #[serde(rename = "meta")]
    Meta,
}

/// `kvs:{channels}` of the meta line: the manifest's channel list at start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct KvsInfo {
    #[serde(default)]
    pub channels: Vec<Value>,
    /// Phase 3: the manifest carried the upload identity (credential
    /// endpoint, role alias, region, uploadBucket). The rig then writes a
    /// COMPLETE manifest with its own fake-AWS values (`xorgate-replay`
    /// `aws.rs`); absent = the Phase 2 `{"channels": …}` stub.
    #[serde(default, skip_serializing_if = "is_false")]
    pub upload: bool,
}

/// First line of every tape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapeMeta {
    pub v: u32,
    pub k: MetaTag,
    /// The recording agent's deviceId (the player substitutes the rig ids).
    pub device_id: String,
    /// Build string of the recorder (`xorgate-rs -v` text or `synth`).
    pub build: String,
    /// Wall ms at `t = 0`.
    pub start_wall: i64,
    /// `hardware.json` as parsed at start (`null` when absent).
    #[serde(default)]
    pub hardware: Value,
    #[serde(default)]
    pub kvs: KvsInfo,
    /// The input knobs (see [`META_ENV_VERBATIM`] / [`META_ENV_REDACTED`]).
    #[serde(default)]
    pub env: Map<String, Value>,
    /// Optional: the agent's home files at start, `name -> text | null`
    /// (`settings.json`, `config-sync.json`, `commands.json`). The rig seeds
    /// both agents' homes from it so a replay starts from the bench's state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<Map<String, Value>>,
    /// Where the tape came from: `bench`, `synth:<scenario>`, `record-shared`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Labels of spliced/synthetic content (O8 "labelled `synthetic`").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synthetic: Vec<String>,
    /// Phase 3, synthetic tapes only: rig directives that are not agent
    /// inputs on a device (`s3Faults` windows, the `filler` tree seeded into
    /// both homes before start). The Rust agent never writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rig: Option<Value>,
}

impl TapeMeta {
    /// A meta line with the given identity; everything else empty.
    pub fn new(device_id: impl Into<String>, build: impl Into<String>, start_wall: i64) -> Self {
        Self {
            v: TAPE_VERSION,
            k: MetaTag::Meta,
            device_id: device_id.into(),
            build: build.into(),
            start_wall,
            hardware: Value::Null,
            kvs: KvsInfo::default(),
            env: Map::new(),
            home: None,
            source: None,
            synthetic: Vec::new(),
            rig: None,
        }
    }

    /// The redacted `env` block from an env lookup (`std::env::var` in the
    /// agent, a map in tests). Unset keys are omitted.
    pub fn redacted_env(lookup: &dyn Fn(&str) -> Option<String>) -> Map<String, Value> {
        let mut env = Map::new();
        for key in META_ENV_VERBATIM {
            if let Some(v) = lookup(key) {
                env.insert((*key).to_string(), Value::String(v));
            }
        }
        for key in META_ENV_REDACTED {
            if lookup(key).is_some() {
                env.insert((*key).to_string(), Value::String("<redacted>".into()));
            }
        }
        env
    }
}

/// MQTT connection transitions (`conn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    Connected,
    Closed,
}

fn is_false(b: &bool) -> bool {
    !*b
}

// ------------------------------------------------- Phase 3 record types (P16)

/// `role` of a capture child (`gst-launch-1.0`, WP4).
pub const ROLE_CAPTURE: &str = "capture";
/// `role` of a KVS master (`xorgate-kvs-webrtc`, WP5).
pub const ROLE_KVS: &str = "kvs";

/// The master's contract env keys (`ts:src/hardware/kvs-webrtc.ts:94-106`):
/// the only env keys a `child_spawn` record keeps. Values are paths, ids and
/// levels — no secret is part of the contract (the cert/key are passed by
/// PATH). Anything else the caller hands over is dropped by the writer.
pub const CHILD_ENV_KEYS: &[&str] = &[
    "AWS_DEFAULT_REGION",
    "AWS_IOT_CORE_CREDENTIAL_ENDPOINT",
    "AWS_IOT_CORE_CERT",
    "AWS_IOT_CORE_PRIVATE_KEY",
    "AWS_IOT_CORE_ROLE_ALIAS",
    "AWS_IOT_CORE_THING_NAME",
    "AWS_KVS_CACERT_PATH",
    "AWS_KVS_LOG_LEVEL",
    "LD_LIBRARY_PATH",
    "KVS_GST_PIPELINE",
    "KVS_IDLE_TEARDOWN_SECS",
];

/// Capture stdout/stderr lines the tape keeps (phase file §Record/replay:
/// "capture: only `New clock:`/`Got EOS`/`ERROR` lines"); a line is kept when
/// it CONTAINS one of these.
pub const CAPTURE_LINE_MARKERS: &[&str] = &["New clock:", "Got EOS", "ERROR"];

/// KVS master lines are not taped one by one: the writer counts them per
/// `(key, fd)` and writes one `child_line {count}` per window of this length.
pub const KVS_LINE_WINDOW_MS: u64 = 10_000;

/// `child_spawn`: a capture / KVS child was spawned (recorded after spawn).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChildSpawnRec {
    /// [`ROLE_CAPTURE`] or [`ROLE_KVS`].
    pub role: String,
    /// Camera / channel key (`cam0`, …).
    pub key: String,
    /// Full argv, program first, exactly as spawned.
    pub argv: Vec<String>,
    /// Env the agent set for the child, [`CHILD_ENV_KEYS`] only (the writer
    /// filters; sorted by key). Empty for capture children.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// OS pid (`0` = the spawn failed before a process existed).
    pub pid: u32,
    /// The `nice` value applied (`None` = not reniced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nice: Option<i32>,
}

/// `child_exit`: the waiter saw a capture / KVS child exit.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChildExitRec {
    pub role: String,
    pub key: String,
    pub pid: u32,
    /// Exit code (`None` = killed by a signal / spawn error).
    pub code: Option<i32>,
    /// Terminating signal number, if any.
    pub signal: Option<i32>,
    /// True when the exit was NOT caused by an agent-sent signal (crash,
    /// EOF, external kill): the player re-enacts these at tape time.
    #[serde(default)]
    pub external: bool,
}

/// `fs_out` operation (`create`/`close`/`sidecar`/`delete`/`gz`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsOp {
    Create,
    Close,
    Sidecar,
    Delete,
    Gz,
}

impl FsOp {
    pub fn as_str(self) -> &'static str {
        match self {
            FsOp::Create => "create",
            FsOp::Close => "close",
            FsOp::Sidecar => "sidecar",
            FsOp::Delete => "delete",
            FsOp::Gz => "gz",
        }
    }
}

/// `s3_put`: one uploader PUT (recorded when it settles).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct S3PutRec {
    /// Object key (no bucket).
    pub key: String,
    #[serde(rename = "contentType")]
    pub content_type: String,
    /// Body bytes.
    pub size: u64,
    /// Lowercase hex sha256 of the body.
    pub sha256: String,
    /// HTTP status when a response arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The error text the uploader logged, when the PUT failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One input event. `k` is the kind; see the table in the module doc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum TapeEvent {
    /// One NMEA line off the GNSS tty, as split on `\n` (a trailing `\r` kept).
    Nmea { line: String },
    /// One stdout line of the IMU helper.
    Imu { line: String },
    /// The IMU helper was spawned with this argv.
    ImuSpawn { argv: Vec<String> },
    /// The IMU helper exited (`code: null` = killed by a signal / spawn error).
    ImuExit { code: Option<i32> },
    /// `i2cdetect -y 1` output, or `notFound:true` when the tool is absent.
    I2cdetect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdout: Option<String>,
        #[serde(rename = "notFound", default, skip_serializing_if = "is_false")]
        not_found: bool,
    },
    /// The full LTE telemetry file (`text`, or `textB64` when not UTF-8), or
    /// `absent:true`. Recorded only when the content changes.
    LteFile {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(rename = "textB64", default, skip_serializing_if = "Option::is_none")]
        text_b64: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        absent: bool,
    },
    /// A tool run (vcgencmd `get_throttled` / `pmic_read_adc`): stdout + exit
    /// code (`null` = timeout / not found). Recorded only on change per argv.
    Exec {
        argv: Vec<String>,
        stdout: String,
        code: Option<i32>,
    },
    /// `/proc/device-tree/chosen/power/max_current` bytes as lowercase hex, or
    /// `absent:true`. Recorded once.
    Dt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_current: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        absent: bool,
    },
    /// An inbound MQTT publish (`payload` utf8, or `payloadB64`).
    MqttIn {
        topic: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<String>,
        #[serde(
            rename = "payloadB64",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        payload_b64: Option<String>,
        retain: bool,
        qos: u8,
    },
    /// MQTT plane connection transition.
    Conn { state: ConnState },
    /// `xorgate-rs …` argv (without the program name) run while taping.
    Cli { argv: Vec<String> },
    /// Periodic clock sample: wall ms + the host's monotonic ms.
    Clock { wall: i64, mono: u64 },
    /// Optional Rust-only replay of a `/proc`/`/sys` read.
    Proc { path: String, text: String },
    /// A capture / KVS child was spawned.
    ChildSpawn(ChildSpawnRec),
    /// A kept capture line (`line`), or a KVS line count for one
    /// [`KVS_LINE_WINDOW_MS`] window (`count`). `fd` 1 = stdout, 2 = stderr.
    ChildLine {
        role: String,
        key: String,
        fd: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u64>,
    },
    /// The agent sent `sig` to a child.
    ChildSignal {
        role: String,
        key: String,
        pid: u32,
        sig: i32,
    },
    /// A capture / KVS child exited.
    ChildExit(ChildExitRec),
    /// An output file operation under the agent home (`rel` is relative).
    FsOut { rel: String, op: FsOp, size: u64 },
    /// One uploader PUT.
    S3Put(S3PutRec),
    /// `/dev/shm/shmpipe.*` names present (every 60 s + at child exit).
    Shm { names: Vec<String> },
}

impl TapeEvent {
    /// The `k` string of this event.
    pub fn kind(&self) -> &'static str {
        match self {
            TapeEvent::Nmea { .. } => "nmea",
            TapeEvent::Imu { .. } => "imu",
            TapeEvent::ImuSpawn { .. } => "imu_spawn",
            TapeEvent::ImuExit { .. } => "imu_exit",
            TapeEvent::I2cdetect { .. } => "i2cdetect",
            TapeEvent::LteFile { .. } => "lte_file",
            TapeEvent::Exec { .. } => "exec",
            TapeEvent::Dt { .. } => "dt",
            TapeEvent::MqttIn { .. } => "mqtt_in",
            TapeEvent::Conn { .. } => "conn",
            TapeEvent::Cli { .. } => "cli",
            TapeEvent::Clock { .. } => "clock",
            TapeEvent::Proc { .. } => "proc",
            TapeEvent::ChildSpawn(_) => "child_spawn",
            TapeEvent::ChildLine { .. } => "child_line",
            TapeEvent::ChildSignal { .. } => "child_signal",
            TapeEvent::ChildExit(_) => "child_exit",
            TapeEvent::FsOut { .. } => "fs_out",
            TapeEvent::S3Put(_) => "s3_put",
            TapeEvent::Shm { .. } => "shm",
        }
    }

    /// `lte_file` from the bytes read (`None` = absent).
    pub fn lte_file(bytes: Option<&[u8]>) -> Self {
        match bytes {
            None => TapeEvent::LteFile {
                text: None,
                text_b64: None,
                absent: true,
            },
            Some(b) => {
                let (text, text_b64) = text_or_b64(b);
                TapeEvent::LteFile {
                    text,
                    text_b64,
                    absent: false,
                }
            }
        }
    }

    /// The bytes of an `lte_file` event (`None` = absent / not an lte_file).
    pub fn lte_bytes(&self) -> Option<Vec<u8>> {
        match self {
            TapeEvent::LteFile { text, text_b64, .. } => blob_bytes(text, text_b64),
            _ => None,
        }
    }

    /// `mqtt_in` from the raw publish.
    pub fn mqtt_in(topic: &str, payload: &[u8], retain: bool, qos: u8) -> Self {
        let (p, b) = text_or_b64(payload);
        TapeEvent::MqttIn {
            topic: topic.to_string(),
            payload: p,
            payload_b64: b,
            retain,
            qos,
        }
    }

    /// The payload bytes of an `mqtt_in` event.
    pub fn mqtt_payload(&self) -> Option<Vec<u8>> {
        match self {
            TapeEvent::MqttIn {
                payload,
                payload_b64,
                ..
            } => blob_bytes(payload, payload_b64).or_else(|| Some(Vec::new())),
            _ => None,
        }
    }

    /// `dt` from the bytes read (`None` = absent).
    pub fn dt(bytes: Option<&[u8]>) -> Self {
        match bytes {
            None => TapeEvent::Dt {
                max_current: None,
                absent: true,
            },
            Some(b) => TapeEvent::Dt {
                max_current: Some(hex_encode(b)),
                absent: false,
            },
        }
    }
}

/// UTF-8 bytes go into the text field; anything else into the base64 one.
fn text_or_b64(bytes: &[u8]) -> (Option<String>, Option<String>) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (Some(s.to_string()), None),
        Err(_) => (None, Some(b64_encode(bytes))),
    }
}

fn blob_bytes(text: &Option<String>, b64: &Option<String>) -> Option<Vec<u8>> {
    if let Some(t) = text {
        return Some(t.as_bytes().to_vec());
    }
    b64.as_deref().and_then(b64_decode)
}

/// Lowercase hex.
pub fn hex_encode(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0xf) as usize] as char);
    }
    s
}

/// Inverse of [`hex_encode`] (either case); `None` on odd length / bad digit.
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| Some(digit(p[0])? << 4 | digit(p[1])?))
        .collect()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (no crate needed for a tape side channel).
pub fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = match chunk.len() {
            3 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8 | chunk[2] as u32,
            2 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8,
            _ => (chunk[0] as u32) << 16,
        };
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Inverse of [`b64_encode`]; `None` on malformed input.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// One tape line after the meta line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TapeRecord {
    /// Monotonic ms since the tape started.
    pub t: u64,
    /// Wall ms when recorded.
    pub w: i64,
    #[serde(flatten)]
    pub ev: TapeEvent,
}

// ---------------------------------------------------------------- writing

/// Time source of a writer: `(t mono ms since start, wall ms, host mono ms)`.
pub trait TapeClock: Send + Sync {
    fn now(&self) -> (u64, i64, u64);
}

/// Real clocks: `Instant` since construction, `SystemTime`, CLOCK_MONOTONIC.
pub struct SystemTapeClock {
    start: Instant,
}

impl SystemTapeClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemTapeClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Wall-clock epoch ms.
pub fn wall_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn host_mono_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the timespec we own; CLOCK_MONOTONIC
    // is always available on Linux.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

impl TapeClock for SystemTapeClock {
    fn now(&self) -> (u64, i64, u64) {
        (
            self.start.elapsed().as_millis() as u64,
            wall_now_ms(),
            host_mono_ms(),
        )
    }
}

struct WriterState<W: Write> {
    out: Option<W>,
    last_lte: Option<Option<Vec<u8>>>,
    last_exec: HashMap<Vec<String>, (String, Option<i32>)>,
    dt_done: bool,
    last_flush_t: u64,
    records: u64,
    /// KVS line counts per `(key, fd)`: `(window start t, count)`.
    kvs_lines: BTreeMap<(String, u8), (u64, u64)>,
}

/// Tape writer over any `Write`. Thread-safe; every method is infallible from
/// the caller's view (see the module doc for the error policy).
pub struct TapeWriter<W: Write + Send> {
    state: Mutex<WriterState<W>>,
    clock: Box<dyn TapeClock>,
}

impl<W: Write + Send> TapeWriter<W> {
    /// Write the meta line and return the writer.
    pub fn new(mut out: W, meta: &TapeMeta, clock: Box<dyn TapeClock>) -> io::Result<Self> {
        let line = serde_json::to_string(meta).map_err(io::Error::other)?;
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")?;
        Ok(Self {
            state: Mutex::new(WriterState {
                out: Some(out),
                last_lte: None,
                last_exec: HashMap::new(),
                dt_done: false,
                last_flush_t: 0,
                records: 0,
                kvs_lines: BTreeMap::new(),
            }),
            clock,
        })
    }

    /// Records written so far (meta line excluded).
    pub fn records(&self) -> u64 {
        self.state.lock().map(|s| s.records).unwrap_or(0)
    }

    /// Is the tape still writing (no error so far, not closed)?
    pub fn is_active(&self) -> bool {
        self.state.lock().map(|s| s.out.is_some()).unwrap_or(false)
    }

    fn write_locked(&self, st: &mut WriterState<W>, ev: TapeEvent, force_flush: bool) {
        let Some(out) = st.out.as_mut() else { return };
        let (t, w, _) = self.clock.now();
        let rec = TapeRecord { t, w, ev };
        let res = serde_json::to_string(&rec)
            .map_err(io::Error::other)
            .and_then(|line| {
                out.write_all(line.as_bytes())?;
                out.write_all(b"\n")
            })
            .and_then(|_| {
                if force_flush || t.saturating_sub(st.last_flush_t) >= FLUSH_EVERY_MS {
                    st.last_flush_t = t;
                    out.flush()
                } else {
                    Ok(())
                }
            });
        match res {
            Ok(()) => st.records += 1,
            Err(e) => {
                tracing::warn!("[agent] tape write failed: {e}; tape disabled for this run");
                st.out = None;
            }
        }
    }

    /// Append one event unconditionally.
    pub fn record(&self, ev: TapeEvent) {
        if let Ok(mut st) = self.state.lock() {
            self.write_locked(&mut st, ev, false);
        }
    }

    pub fn nmea(&self, line: &str) {
        self.record(TapeEvent::Nmea {
            line: line.to_string(),
        });
    }

    pub fn imu_line(&self, line: &str) {
        self.record(TapeEvent::Imu {
            line: line.to_string(),
        });
    }

    pub fn imu_spawn(&self, argv: &[String]) {
        self.record(TapeEvent::ImuSpawn {
            argv: argv.to_vec(),
        });
    }

    pub fn imu_exit(&self, code: Option<i32>) {
        self.record(TapeEvent::ImuExit { code });
    }

    /// `stdout = None` means the tool is not installed.
    pub fn i2cdetect(&self, stdout: Option<&str>) {
        self.record(TapeEvent::I2cdetect {
            stdout: stdout.map(str::to_string),
            not_found: stdout.is_none(),
        });
    }

    /// Report EVERY read of the LTE file; only changes reach the tape.
    pub fn lte_file(&self, bytes: Option<&[u8]>) {
        if let Ok(mut st) = self.state.lock() {
            let cur = bytes.map(<[u8]>::to_vec);
            if st.last_lte.as_ref() == Some(&cur) {
                return;
            }
            st.last_lte = Some(cur);
            self.write_locked(&mut st, TapeEvent::lte_file(bytes), false);
        }
    }

    /// Report EVERY tool run; only a changed (stdout, code) per argv is taped.
    pub fn exec_argv(&self, argv: &[String], stdout: &str, code: Option<i32>) {
        if let Ok(mut st) = self.state.lock() {
            let cur = (stdout.to_string(), code);
            if st.last_exec.get(argv) == Some(&cur) {
                return;
            }
            st.last_exec.insert(argv.to_vec(), cur);
            self.write_locked(
                &mut st,
                TapeEvent::Exec {
                    argv: argv.to_vec(),
                    stdout: stdout.to_string(),
                    code,
                },
                false,
            );
        }
    }

    /// The device-tree PSU limit; only the first call is taped.
    pub fn dt(&self, bytes: Option<&[u8]>) {
        if let Ok(mut st) = self.state.lock() {
            if st.dt_done {
                return;
            }
            st.dt_done = true;
            self.write_locked(&mut st, TapeEvent::dt(bytes), false);
        }
    }

    pub fn mqtt_in(&self, topic: &str, payload: &[u8], retain: bool, qos: u8) {
        self.record(TapeEvent::mqtt_in(topic, payload, retain, qos));
    }

    pub fn conn_state(&self, state: ConnState) {
        if let Ok(mut st) = self.state.lock() {
            self.write_locked(&mut st, TapeEvent::Conn { state }, true);
        }
    }

    pub fn cli(&self, argv: &[String]) {
        if let Ok(mut st) = self.state.lock() {
            self.write_locked(
                &mut st,
                TapeEvent::Cli {
                    argv: argv.to_vec(),
                },
                true,
            );
        }
    }

    /// The 10 s clock sample from this writer's own clocks; forces a flush.
    pub fn clock_now(&self) {
        let (_, wall, mono) = self.clock.now();
        self.clock_sample(wall, mono);
    }

    /// A clock sample with the caller's values (the agent's `AgentClock`);
    /// forces a flush.
    pub fn clock_sample(&self, wall: i64, mono: u64) {
        if let Ok(mut st) = self.state.lock() {
            let (t, _, _) = self.clock.now();
            self.flush_kvs_lines(&mut st, None, t);
            self.write_locked(&mut st, TapeEvent::Clock { wall, mono }, true);
        }
    }

    pub fn proc_read(&self, path: &str, text: &str) {
        self.record(TapeEvent::Proc {
            path: path.to_string(),
            text: text.to_string(),
        });
    }

    /// Report a spawned child; env is filtered to [`CHILD_ENV_KEYS`].
    pub fn child_spawn(&self, r: &ChildSpawnRec) {
        let mut rec = r.clone();
        rec.env.retain(|k, _| CHILD_ENV_KEYS.contains(&k.as_str()));
        if let Ok(mut st) = self.state.lock() {
            self.write_locked(&mut st, TapeEvent::ChildSpawn(rec), true);
        }
    }

    /// Report EVERY drained child line. Capture lines are kept only when they
    /// carry a [`CAPTURE_LINE_MARKERS`] marker; KVS lines are counted per
    /// `(key, fd)` and written as one `count` record per window; other roles
    /// are kept verbatim.
    pub fn child_line(&self, role: &str, key: &str, fd: u8, line: &str) {
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        if role == ROLE_KVS {
            let (t, _, _) = self.clock.now();
            let slot = st.kvs_lines.entry((key.to_string(), fd)).or_insert((t, 0));
            slot.1 += 1;
            if t.saturating_sub(slot.0) >= KVS_LINE_WINDOW_MS {
                self.flush_kvs_lines(&mut st, Some(key), t);
            }
            return;
        }
        if role == ROLE_CAPTURE && !CAPTURE_LINE_MARKERS.iter().any(|m| line.contains(m)) {
            return;
        }
        let ev = TapeEvent::ChildLine {
            role: role.to_string(),
            key: key.to_string(),
            fd,
            line: Some(line.to_string()),
            count: None,
        };
        self.write_locked(&mut st, ev, false);
    }

    /// Write the pending KVS line counts (all keys, or only `key`) whose
    /// window has run `KVS_LINE_WINDOW_MS`, or every pending one when
    /// `now == u64::MAX` (child exit / close).
    fn flush_kvs_lines(&self, st: &mut WriterState<W>, key: Option<&str>, now: u64) {
        let due: Vec<(String, u8)> = st
            .kvs_lines
            .iter()
            .filter(|((k, _), (start, n))| {
                *n > 0
                    && key.is_none_or(|want| want == k)
                    && (now == u64::MAX || now.saturating_sub(*start) >= KVS_LINE_WINDOW_MS)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in due {
            let Some((_, n)) = st.kvs_lines.remove(&k) else {
                continue;
            };
            let ev = TapeEvent::ChildLine {
                role: ROLE_KVS.to_string(),
                key: k.0,
                fd: k.1,
                line: None,
                count: Some(n),
            };
            self.write_locked(st, ev, false);
        }
    }

    /// The agent sent `sig` to a child.
    pub fn child_signal(&self, role: &str, key: &str, pid: u32, sig: i32) {
        if let Ok(mut st) = self.state.lock() {
            let ev = TapeEvent::ChildSignal {
                role: role.to_string(),
                key: key.to_string(),
                pid,
                sig,
            };
            self.write_locked(&mut st, ev, true);
        }
    }

    /// A child exited; its pending KVS line counts are written first.
    pub fn child_exit(&self, r: &ChildExitRec) {
        if let Ok(mut st) = self.state.lock() {
            if r.role == ROLE_KVS {
                self.flush_kvs_lines(&mut st, Some(&r.key), u64::MAX);
            }
            self.write_locked(&mut st, TapeEvent::ChildExit(r.clone()), true);
        }
    }

    /// An output file operation under the agent home.
    pub fn fs_out(&self, rel: &str, op: FsOp, size: u64) {
        self.record(TapeEvent::FsOut {
            rel: rel.to_string(),
            op,
            size,
        });
    }

    /// One settled uploader PUT.
    pub fn s3_put(&self, r: &S3PutRec) {
        self.record(TapeEvent::S3Put(r.clone()));
    }

    /// The `/dev/shm/shmpipe.*` names present now.
    pub fn shm(&self, names: &[String]) {
        self.record(TapeEvent::Shm {
            names: names.to_vec(),
        });
    }

    /// Stop writing and hand back the sink (flushed). `None` if it failed.
    pub fn into_inner(self) -> Option<W> {
        if let Ok(mut st) = self.state.lock() {
            self.flush_kvs_lines(&mut st, None, u64::MAX);
        }
        let mut st = self.state.into_inner().ok()?;
        let mut out = st.out.take()?;
        out.flush().ok()?;
        Some(out)
    }

    /// Stop writing; later calls are no-ops. Returns the sink for finishing.
    pub fn take(&self) -> Option<W> {
        let mut st = self.state.lock().ok()?;
        self.flush_kvs_lines(&mut st, None, u64::MAX);
        let mut out = st.out.take()?;
        let _ = out.flush();
        Some(out)
    }
}

/// The agent's file tape: gzip NDJSON at a path (`XORGATE_RS_TAPE`).
pub struct FileTape {
    inner: TapeWriter<GzEncoder<BufWriter<File>>>,
}

impl FileTape {
    /// Create (truncate) `path` and write the meta line. Creates parent dirs.
    pub fn create(path: &Path, meta: &TapeMeta) -> io::Result<Self> {
        Self::create_with_clock(path, meta, Box::new(SystemTapeClock::new()))
    }

    pub fn create_with_clock(
        path: &Path,
        meta: &TapeMeta,
        clock: Box<dyn TapeClock>,
    ) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let file = File::create(path)?;
        let gz = GzEncoder::new(BufWriter::new(file), Compression::default());
        let inner = TapeWriter::new(gz, meta, clock)?;
        // The header must survive an immediate SIGKILL too.
        if let Ok(mut st) = inner.state.lock() {
            if let Some(out) = st.out.as_mut() {
                out.flush()?;
            }
        }
        Ok(Self { inner })
    }

    /// The event API (`nmea`, `lte_file`, …).
    pub fn writer(&self) -> &TapeWriter<GzEncoder<BufWriter<File>>> {
        &self.inner
    }

    /// Finish the gzip stream (trailer + fsync). Idempotent; also run on drop.
    pub fn close(&self) -> io::Result<()> {
        if let Some(gz) = self.inner.take() {
            let mut buf = gz.finish()?;
            buf.flush()?;
            let file = buf.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
        }
        Ok(())
    }
}

/// The tape meta line from what the agent knows at start (WP1's
/// [`crate::io::TapeStart`]); `env` is the redacted knob block.
pub fn meta_from_start(
    start: &crate::io::TapeStart,
    env: &dyn Fn(&str) -> Option<String>,
) -> TapeMeta {
    let mut meta = TapeMeta::new(&start.device_id, &start.build, start.start_wall);
    meta.hardware = start.hardware.clone();
    meta.kvs = KvsInfo {
        channels: start.kvs_channels.clone(),
        upload: false,
    };
    meta.env = TapeMeta::redacted_env(env);
    meta.home = start.home_files.clone();
    meta.source = Some("bench".into());
    meta
}

/// WP1's [`crate::io::TapeSink`] over the gz file writer. The writer
/// de-duplicates `lte_file` / `exec` / `dt`; `conn`, `cli` and `clock` force a
/// gzip sync-flush (as does any record ≥ 2 s after the last flush).
impl crate::io::TapeSink for FileTape {
    fn enabled(&self) -> bool {
        self.inner.is_active()
    }
    fn nmea(&self, line: &str) {
        self.inner.nmea(line);
    }
    fn imu(&self, line: &str) {
        self.inner.imu_line(line);
    }
    fn imu_spawn(&self, argv: &[String]) {
        self.inner.imu_spawn(argv);
    }
    fn imu_exit(&self, code: Option<i32>) {
        self.inner.imu_exit(code);
    }
    fn i2cdetect(&self, stdout: Option<&str>) {
        self.inner.i2cdetect(stdout);
    }
    fn lte_file(&self, bytes: Option<&[u8]>) {
        self.inner.lte_file(bytes);
    }
    fn exec(&self, argv: &[&str], stdout: &str, code: Option<i32>) {
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        self.inner.exec_argv(&argv, stdout, code);
    }
    fn dt(&self, max_current: Option<&[u8]>) {
        self.inner.dt(max_current);
    }
    fn mqtt_in(&self, topic: &str, payload: &[u8], retain: bool, qos: u8) {
        self.inner.mqtt_in(topic, payload, retain, qos);
    }
    fn conn(&self, connected: bool) {
        self.inner.conn_state(if connected {
            ConnState::Connected
        } else {
            ConnState::Closed
        });
    }
    // No `cli` override (review-wp6 decision 1): the CLI is a separate process
    // and writes `<tape>.cli.ndjson` (WP6 `cli/set.rs` `tape_cli`); only the
    // agent writes this gz stream. `Tape::read` merges the side file.
    fn clock(&self, wall_ms: i64, mono_ms: u64) {
        self.inner.clock_sample(wall_ms, mono_ms);
    }
    fn proc_read(&self, path: &str, text: &str) {
        self.inner.proc_read(path, text);
    }
    fn child_spawn(&self, r: &ChildSpawnRec) {
        self.inner.child_spawn(r);
    }
    fn child_line(&self, role: &str, key: &str, fd: u8, line: &str) {
        self.inner.child_line(role, key, fd, line);
    }
    fn child_signal(&self, role: &str, key: &str, pid: u32, sig: i32) {
        self.inner.child_signal(role, key, pid, sig);
    }
    fn child_exit(&self, r: &ChildExitRec) {
        self.inner.child_exit(r);
    }
    fn fs_out(&self, rel: &str, op: FsOp, size: u64) {
        self.inner.fs_out(rel, op, size);
    }
    fn s3_put(&self, r: &S3PutRec) {
        self.inner.s3_put(r);
    }
    fn shm(&self, names: &[String]) {
        self.inner.shm(names);
    }
    fn close(&self) {
        if let Err(e) = FileTape::close(self) {
            tracing::warn!("[agent] tape close failed: {e}");
        }
    }
}

impl std::ops::Deref for FileTape {
    type Target = TapeWriter<GzEncoder<BufWriter<File>>>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Drop for FileTape {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            tracing::warn!("[agent] tape close failed: {e}");
        }
    }
}

// ---------------------------------------------------------------- reading

/// Why a tape line could not be read.
#[derive(Debug, thiserror::Error)]
pub enum TapeError {
    #[error("tape i/o: {0}")]
    Io(#[from] io::Error),
    #[error("tape is empty (no meta line)")]
    Empty,
    #[error("line {line}: {msg}")]
    Parse { line: usize, msg: String },
}

/// Streaming reader. Auto-detects gzip by magic. A tape cut short (SIGKILL,
/// reboot) ends at the last complete line and sets [`TapeReader::truncated`].
pub struct TapeReader {
    input: Box<dyn BufRead + Send>,
    pub meta: TapeMeta,
    pub truncated: bool,
    line_no: usize,
    done: bool,
}

impl TapeReader {
    pub fn open(path: &Path) -> Result<Self, TapeError> {
        Self::from_reader(File::open(path)?)
    }

    pub fn from_reader<R: Read + Send + 'static>(r: R) -> Result<Self, TapeError> {
        let mut br = BufReader::new(r);
        let gz = {
            let head = br.fill_buf()?;
            head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b
        };
        let input: Box<dyn BufRead + Send> = if gz {
            Box::new(BufReader::new(MultiGzDecoder::new(br)))
        } else {
            Box::new(br)
        };
        let mut rd = Self {
            input,
            meta: TapeMeta::new("", "", 0),
            truncated: false,
            line_no: 0,
            done: false,
        };
        let first = rd.next_line()?.ok_or(TapeError::Empty)?;
        rd.meta = serde_json::from_slice(&first).map_err(|e| TapeError::Parse {
            line: 1,
            msg: format!("meta: {e}"),
        })?;
        Ok(rd)
    }

    /// Next complete line (without the newline), `None` at the end.
    fn next_line(&mut self) -> Result<Option<Vec<u8>>, TapeError> {
        loop {
            if self.done {
                return Ok(None);
            }
            let mut buf = Vec::new();
            match self.input.read_until(b'\n', &mut buf) {
                Ok(0) => {
                    self.done = true;
                    return Ok(None);
                }
                Ok(_) => {
                    self.line_no += 1;
                    if buf.last() == Some(&b'\n') {
                        buf.pop();
                    } else {
                        // Clean EOF on a line without newline: keep it only if
                        // it parses (checked by the caller); the stream is done.
                        self.done = true;
                    }
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                    if buf.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    return Ok(Some(buf));
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    // Truncated gzip: whatever was buffered is a partial line.
                    self.truncated = true;
                    self.done = true;
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Read everything that is left.
    pub fn read_all(mut self) -> Result<Tape, TapeError> {
        let mut records = Vec::new();
        for r in self.by_ref() {
            records.push(r?);
        }
        Ok(Tape {
            meta: self.meta,
            records,
            truncated: self.truncated,
        })
    }
}

impl Iterator for TapeReader {
    type Item = Result<TapeRecord, TapeError>;

    fn next(&mut self) -> Option<Self::Item> {
        let line = match self.next_line() {
            Ok(Some(l)) => l,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };
        let line_no = self.line_no;
        match serde_json::from_slice::<TapeRecord>(&line) {
            Ok(r) => Some(Ok(r)),
            Err(e) if self.done => {
                // An unterminated last line that does not parse is a cut write.
                let _ = e;
                self.truncated = true;
                None
            }
            Err(e) => Some(Err(TapeError::Parse {
                line: line_no,
                msg: e.to_string(),
            })),
        }
    }
}

/// A whole tape in memory.
#[derive(Debug, Clone, PartialEq)]
pub struct Tape {
    pub meta: TapeMeta,
    pub records: Vec<TapeRecord>,
    pub truncated: bool,
}

/// The CLI's side file next to a tape: `<tape>.cli.ndjson`. `xorgate-rs set`
/// appends `{"w":<wall ms>,"k":"cli","argv":[…]}` there (WP6 `cli/set.rs`
/// `tape_cli`, evidence wp6.md A6): the gz tape has exactly one writer, the
/// agent process, so the CLI never touches it.
pub fn cli_side_path(tape: &Path) -> std::path::PathBuf {
    let mut p = tape.as_os_str().to_owned();
    p.push(".cli.ndjson");
    std::path::PathBuf::from(p)
}

impl Tape {
    /// Read a tape and merge its CLI side file ([`cli_side_path`]) when present.
    pub fn read(path: &Path) -> Result<Self, TapeError> {
        let mut tape = TapeReader::open(path)?.read_all()?;
        match std::fs::read(cli_side_path(path)) {
            Ok(bytes) => {
                tape.merge_cli_side(&String::from_utf8_lossy(&bytes))?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(tape)
    }

    /// Merge side-file `cli` lines (wall time only) into the record stream.
    /// Each line's `t` is placed on the tape's mono axis from the nearest
    /// record at or before its wall time (`t + (w − record.w)`, which follows
    /// any wall step the tape saw), or from `startWall` when it precedes every
    /// record; it is then inserted after every record with a smaller-or-equal
    /// `t` (stable: tape order wins ties). A cut last line (no newline, does
    /// not parse) is ignored, like the gz reader. Returns the number merged.
    pub fn merge_cli_side(&mut self, text: &str) -> Result<usize, TapeError> {
        #[derive(Deserialize)]
        struct Side {
            w: i64,
            k: String,
            argv: Vec<String>,
        }
        let lines: Vec<&str> = text.split('\n').collect();
        let mut add = Vec::new();
        for (i, raw) in lines.iter().enumerate() {
            let line = raw.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            let side: Side = match serde_json::from_str(line) {
                Ok(s) => s,
                Err(_) if i + 1 == lines.len() => break, // cut write
                Err(e) => {
                    return Err(TapeError::Parse {
                        line: i + 1,
                        msg: format!("cli side file: {e}"),
                    })
                }
            };
            if side.k != "cli" {
                continue;
            }
            let anchor = self.records.iter().rev().find(|r| r.w <= side.w);
            let t = match anchor {
                Some(r) => r.t.saturating_add((side.w - r.w).max(0) as u64),
                None => (side.w - self.meta.start_wall).max(0) as u64,
            };
            add.push(TapeRecord {
                t,
                w: side.w,
                ev: TapeEvent::Cli { argv: side.argv },
            });
        }
        let n = add.len();
        for rec in add {
            let at = self.records.partition_point(|r| r.t <= rec.t);
            self.records.insert(at, rec);
        }
        Ok(n)
    }

    /// Serialise as NDJSON (meta first), gzip when `gz`.
    pub fn write(&self, path: &Path, gz: bool) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let file = BufWriter::new(File::create(path)?);
        if gz {
            // Fixed mtime 0 in the header (flate2 default) keeps synth tapes
            // byte-reproducible.
            let mut enc = GzEncoder::new(file, Compression::default());
            self.write_to(&mut enc)?;
            enc.finish()?.flush()
        } else {
            let mut f = file;
            self.write_to(&mut f)?;
            f.flush()
        }
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let meta = serde_json::to_string(&self.meta).map_err(io::Error::other)?;
        w.write_all(meta.as_bytes())?;
        w.write_all(b"\n")?;
        for r in &self.records {
            let line = serde_json::to_string(r).map_err(io::Error::other)?;
            w.write_all(line.as_bytes())?;
            w.write_all(b"\n")?;
        }
        Ok(())
    }

    /// Duration in ms (last record's `t`).
    pub fn duration_ms(&self) -> u64 {
        self.records.last().map(|r| r.t).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_side_file_merges_by_wall_time() {
        let dir = std::env::temp_dir().join(format!("tape-cli-side-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.ndjson");
        let mut meta = TapeMeta::new("dev", "b", 1_000);
        meta.v = 1;
        let rec = |t: u64, w: i64, line: &str| TapeRecord {
            t,
            w,
            ev: TapeEvent::Nmea { line: line.into() },
        };
        let tape = Tape {
            meta,
            records: vec![
                rec(0, 1_000, "a"),
                rec(5_000, 6_000, "b"),
                rec(9_000, 10_000, "c"),
            ],
            truncated: false,
        };
        tape.write(&path, false).unwrap();
        std::fs::write(
            cli_side_path(&path),
            concat!(
                "{\"w\":7500,\"k\":\"cli\",\"argv\":[\"set\",\"timeSync.stepThresholdSecs\",\"15\"]}\n",
                "{\"w\":500,\"k\":\"cli\",\"argv\":[\"set\",\"media\",\"off\"]}\n",
                "{\"w\":9999,\"k\":\"cli\",\"ar"
            ),
        )
        .unwrap();
        assert!(cli_side_path(&path).ends_with("t.ndjson.cli.ndjson"));
        let got = Tape::read(&path).unwrap();
        let kinds: Vec<(u64, &str)> = got.records.iter().map(|r| (r.t, r.ev.kind())).collect();
        assert_eq!(
            kinds,
            vec![
                (0, "nmea"),
                (0, "cli"),
                (5_000, "nmea"),
                (6_500, "cli"),
                (9_000, "nmea")
            ]
        );
        assert_eq!(
            got.records[3].ev,
            TapeEvent::Cli {
                argv: vec![
                    "set".into(),
                    "timeSync.stepThresholdSecs".into(),
                    "15".into()
                ]
            }
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Test clock: t advances by hand.
    struct ManualClock {
        t: Arc<AtomicU64>,
        wall0: i64,
    }
    impl TapeClock for ManualClock {
        fn now(&self) -> (u64, i64, u64) {
            let t = self.t.load(Ordering::SeqCst);
            (t, self.wall0 + t as i64, 1_000 + t)
        }
    }

    fn manual(wall0: i64) -> (Box<dyn TapeClock>, Arc<AtomicU64>) {
        let t = Arc::new(AtomicU64::new(0));
        (
            Box::new(ManualClock {
                t: t.clone(),
                wall0,
            }),
            t,
        )
    }

    #[test]
    fn record_line_shape_is_t_w_k_then_fields() {
        let rec = TapeRecord {
            t: 12,
            w: 1_760_000_000_000,
            ev: TapeEvent::Nmea {
                line: "$GPGGA,1*00\r".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&rec).unwrap(),
            r#"{"t":12,"w":1760000000000,"k":"nmea","line":"$GPGGA,1*00\r"}"#
        );
        let i2c = TapeRecord {
            t: 0,
            w: 0,
            ev: TapeEvent::I2cdetect {
                stdout: None,
                not_found: true,
            },
        };
        assert_eq!(
            serde_json::to_string(&i2c).unwrap(),
            r#"{"t":0,"w":0,"k":"i2cdetect","notFound":true}"#
        );
        let lte = TapeRecord {
            t: 1,
            w: 2,
            ev: TapeEvent::lte_file(None),
        };
        assert_eq!(
            serde_json::to_string(&lte).unwrap(),
            r#"{"t":1,"w":2,"k":"lte_file","absent":true}"#
        );
        let m = TapeRecord {
            t: 1,
            w: 2,
            ev: TapeEvent::mqtt_in("xorgate/devices/d/config", b"{}", true, 1),
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"t":1,"w":2,"k":"mqtt_in","topic":"xorgate/devices/d/config","payload":"{}","retain":true,"qos":1}"#
        );
    }

    #[test]
    fn meta_line_shape() {
        let mut meta = TapeMeta::new("dev-1", "synth", 1_760_000_000_000);
        meta.kvs.channels = vec![];
        let env = |k: &str| match k {
            "MODE" => Some("real".to_string()),
            "DEVICE_ID" => Some("a73f".to_string()),
            "SECRET_THING" => Some("x".to_string()),
            _ => None,
        };
        meta.env = TapeMeta::redacted_env(&env);
        assert_eq!(
            serde_json::to_string(&meta).unwrap(),
            r#"{"v":1,"k":"meta","deviceId":"dev-1","build":"synth","startWall":1760000000000,"hardware":null,"kvs":{"channels":[]},"env":{"MODE":"real","DEVICE_ID":"<redacted>"}}"#
        );
    }

    #[test]
    fn every_kind_round_trips() {
        let evs = vec![
            TapeEvent::Nmea { line: "a".into() },
            TapeEvent::Imu { line: "{}".into() },
            TapeEvent::ImuSpawn {
                argv: vec!["python3".into(), "x.py".into()],
            },
            TapeEvent::ImuExit { code: Some(1) },
            TapeEvent::ImuExit { code: None },
            TapeEvent::I2cdetect {
                stdout: Some("40: -- 4a".into()),
                not_found: false,
            },
            TapeEvent::lte_file(Some(b"{\"csq\":20}")),
            TapeEvent::lte_file(Some(&[0xff, 0x00, b'{'])),
            TapeEvent::lte_file(None),
            TapeEvent::Exec {
                argv: vec!["vcgencmd".into(), "get_throttled".into()],
                stdout: "throttled=0x0\n".into(),
                code: Some(0),
            },
            TapeEvent::dt(Some(&[0, 0, 0x13, 0x88])),
            TapeEvent::dt(None),
            TapeEvent::mqtt_in("t", &[0xc3, 0x28], false, 0),
            TapeEvent::Conn {
                state: ConnState::Closed,
            },
            TapeEvent::Cli {
                argv: vec![
                    "set".into(),
                    "timeSync.stepThresholdSecs".into(),
                    "15".into(),
                ],
            },
            TapeEvent::Clock { wall: 5, mono: 6 },
            TapeEvent::Proc {
                path: "/proc/meminfo".into(),
                text: "MemTotal: 1 kB\n".into(),
            },
        ];
        for ev in evs {
            let rec = TapeRecord { t: 3, w: 4, ev };
            let s = serde_json::to_string(&rec).unwrap();
            let back: TapeRecord = serde_json::from_str(&s).unwrap();
            assert_eq!(back, rec, "{s}");
            assert!(s.contains(&format!("\"k\":\"{}\"", rec.ev.kind())), "{s}");
        }
        assert_eq!(
            TapeEvent::lte_file(Some(&[0xff, 0x00, b'{'])).lte_bytes(),
            Some(vec![0xff, 0x00, b'{'])
        );
        assert_eq!(
            TapeEvent::mqtt_in("t", &[0xc3, 0x28], false, 0).mqtt_payload(),
            Some(vec![0xc3, 0x28])
        );
        match TapeEvent::dt(Some(&[0, 0, 0x13, 0x88])) {
            TapeEvent::Dt {
                max_current: Some(h),
                ..
            } => assert_eq!(hex_decode(&h), Some(vec![0, 0, 0x13, 0x88])),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(b64_encode(raw.as_bytes()), enc);
            assert_eq!(b64_decode(enc).unwrap(), raw.as_bytes());
        }
        assert_eq!(b64_decode("@@"), None);
    }

    #[test]
    fn writer_dedupes_lte_exec_dt_and_reader_round_trips_gz() {
        let dir = std::env::temp_dir().join(format!("xorgate-tape-{}", std::process::id()));
        let path = dir.join("t.ndjson.gz");
        let (clock, t) = manual(1_760_000_000_000);
        let meta = TapeMeta::new("dev", "test", 1_760_000_000_000);
        {
            let tape = FileTape::create_with_clock(&path, &meta, clock).unwrap();
            tape.lte_file(Some(b"{\"csq\":20}"));
            t.store(1000, Ordering::SeqCst);
            tape.lte_file(Some(b"{\"csq\":20}")); // same → not taped
            tape.lte_file(None);
            tape.lte_file(None); // same → not taped
            let argv = vec!["vcgencmd".to_string(), "get_throttled".to_string()];
            tape.exec_argv(&argv, "throttled=0x0\n", Some(0));
            tape.exec_argv(&argv, "throttled=0x0\n", Some(0)); // same
            tape.exec_argv(&argv, "throttled=0x50000\n", Some(0));
            tape.dt(Some(&[0, 0, 0x13, 0x88]));
            tape.dt(None); // once only
            t.store(2500, Ordering::SeqCst);
            tape.nmea("$GPGGA");
            tape.clock_now();
            assert_eq!(tape.records(), 7);
            tape.close().unwrap();
            tape.nmea("after close"); // no-op
        }
        let read = Tape::read(&path).unwrap();
        assert!(!read.truncated);
        assert_eq!(read.meta, meta);
        let kinds: Vec<_> = read.records.iter().map(|r| r.ev.kind()).collect();
        assert_eq!(
            kinds,
            ["lte_file", "lte_file", "exec", "exec", "dt", "nmea", "clock"]
        );
        assert_eq!(read.records[1].t, 1000);
        assert_eq!(read.records[1].w, 1_760_000_001_000);
        assert_eq!(
            read.records[6].ev,
            TapeEvent::Clock {
                wall: 1_760_000_002_500,
                mono: 3_500
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn phase3_kinds_line_shape_writer_filters_and_gz_round_trip() {
        use crate::io::TapeSink;
        // Exact line shapes (the rig's diff and the JS-side drivers key on them).
        let spawn = ChildSpawnRec {
            role: ROLE_KVS.into(),
            key: "cam0".into(),
            argv: vec![
                "xorgate-kvs-webrtc".into(),
                "cam0".into(),
                "video-only".into(),
                "testsrc".into(),
            ],
            env: [
                ("AWS_KVS_LOG_LEVEL".to_string(), "3".to_string()),
                ("AWS_DEFAULT_REGION".to_string(), "ca-central-1".to_string()),
            ]
            .into_iter()
            .collect(),
            pid: 4242,
            nice: None,
        };
        let line = |ev: TapeEvent| serde_json::to_string(&TapeRecord { t: 1, w: 2, ev }).unwrap();
        assert_eq!(
            line(TapeEvent::ChildSpawn(spawn.clone())),
            r#"{"t":1,"w":2,"k":"child_spawn","role":"kvs","key":"cam0","argv":["xorgate-kvs-webrtc","cam0","video-only","testsrc"],"env":{"AWS_DEFAULT_REGION":"ca-central-1","AWS_KVS_LOG_LEVEL":"3"},"pid":4242}"#
        );
        assert_eq!(
            line(TapeEvent::ChildExit(ChildExitRec {
                role: ROLE_CAPTURE.into(),
                key: "cam1".into(),
                pid: 7,
                code: None,
                signal: Some(9),
                external: true,
            })),
            r#"{"t":1,"w":2,"k":"child_exit","role":"capture","key":"cam1","pid":7,"code":null,"signal":9,"external":true}"#
        );
        assert_eq!(
            line(TapeEvent::FsOut {
                rel: "recordings/video/cam0/s/00000.json".into(),
                op: FsOp::Sidecar,
                size: 312,
            }),
            r#"{"t":1,"w":2,"k":"fs_out","rel":"recordings/video/cam0/s/00000.json","op":"sidecar","size":312}"#
        );
        assert_eq!(
            line(TapeEvent::S3Put(S3PutRec {
                key: "media-sessions/v1/d/cam0/s/00000.json".into(),
                content_type: "application/json".into(),
                size: 312,
                sha256: "ab".into(),
                status: Some(200),
                error: None,
            })),
            r#"{"t":1,"w":2,"k":"s3_put","key":"media-sessions/v1/d/cam0/s/00000.json","contentType":"application/json","size":312,"sha256":"ab","status":200}"#
        );

        // Every new kind round-trips through serde.
        let evs = vec![
            TapeEvent::ChildSpawn(spawn.clone()),
            TapeEvent::ChildSpawn(ChildSpawnRec {
                role: ROLE_CAPTURE.into(),
                key: "cam0".into(),
                argv: vec!["gst-launch-1.0".into(), "-e".into()],
                env: BTreeMap::new(),
                pid: 9,
                nice: Some(10),
            }),
            TapeEvent::ChildLine {
                role: ROLE_CAPTURE.into(),
                key: "cam0".into(),
                fd: 1,
                line: Some("New clock: GstSystemClock".into()),
                count: None,
            },
            TapeEvent::ChildLine {
                role: ROLE_KVS.into(),
                key: "cam0".into(),
                fd: 2,
                line: None,
                count: Some(3),
            },
            TapeEvent::ChildSignal {
                role: ROLE_CAPTURE.into(),
                key: "cam0".into(),
                pid: 9,
                sig: 2,
            },
            TapeEvent::ChildExit(ChildExitRec {
                role: ROLE_KVS.into(),
                key: "cam0".into(),
                pid: 4242,
                code: Some(0),
                signal: None,
                external: false,
            }),
            TapeEvent::FsOut {
                rel: "recordings/telemetry/s/x.jsonl.gz".into(),
                op: FsOp::Gz,
                size: 10,
            },
            TapeEvent::S3Put(S3PutRec {
                key: "k".into(),
                content_type: "video/mp4".into(),
                size: 1,
                sha256: "00".into(),
                status: None,
                error: Some("fetch failed".into()),
            }),
            TapeEvent::Shm {
                names: vec!["shmpipe.123.0".into()],
            },
        ];
        for ev in evs {
            let rec = TapeRecord { t: 3, w: 4, ev };
            let s = serde_json::to_string(&rec).unwrap();
            let back: TapeRecord = serde_json::from_str(&s).unwrap();
            assert_eq!(back, rec, "{s}");
            assert!(s.contains(&format!("\"k\":\"{}\"", rec.ev.kind())), "{s}");
        }
        for op in [
            FsOp::Create,
            FsOp::Close,
            FsOp::Sidecar,
            FsOp::Delete,
            FsOp::Gz,
        ] {
            assert_eq!(
                serde_json::to_string(&op).unwrap(),
                format!("\"{}\"", op.as_str())
            );
        }

        // Through the sink on a gz file: env filtered, capture lines filtered,
        // KVS lines counted per window, reader returns the records.
        let dir = std::env::temp_dir().join(format!("xorgate-tape-p3-{}", std::process::id()));
        let path = dir.join("t.ndjson.gz");
        let (clock, t) = manual(1_760_000_000_000);
        let meta = TapeMeta::new("dev", "test", 1_760_000_000_000);
        {
            let tape = FileTape::create_with_clock(&path, &meta, clock).unwrap();
            let sink: &dyn TapeSink = &tape;
            let mut noisy = spawn.clone();
            noisy.env.insert("HOME".into(), "/root".into());
            noisy.env.insert("AWS_SECRET_ACCESS_KEY".into(), "s".into());
            sink.child_spawn(&noisy);
            sink.child_line(ROLE_CAPTURE, "cam0", 1, "Setting pipeline to PLAYING ...");
            sink.child_line(ROLE_CAPTURE, "cam0", 1, "New clock: GstSystemClock");
            sink.child_line(ROLE_KVS, "cam0", 1, "a");
            t.store(4_000, Ordering::SeqCst);
            sink.child_line(ROLE_KVS, "cam0", 1, "b");
            t.store(10_000, Ordering::SeqCst);
            sink.child_line(ROLE_KVS, "cam0", 1, "c"); // window full → count 3
            sink.child_line(ROLE_KVS, "cam0", 1, "d"); // pending
            sink.child_signal(ROLE_KVS, "cam0", 4242, 2);
            sink.child_exit(&ChildExitRec {
                role: ROLE_KVS.into(),
                key: "cam0".into(),
                pid: 4242,
                code: Some(0),
                signal: None,
                external: false,
            }); // flushes count 1 first
            sink.fs_out("recordings/video/cam0/s/00000.mp4", FsOp::Create, 0);
            sink.s3_put(&S3PutRec {
                key: "k".into(),
                content_type: "video/mp4".into(),
                size: 5,
                sha256: "00".into(),
                status: Some(200),
                error: None,
            });
            sink.shm(&["shmpipe.4242.0".to_string()]);
            sink.close();
        }
        let read = Tape::read(&path).unwrap();
        let kinds: Vec<_> = read.records.iter().map(|r| r.ev.kind()).collect();
        assert_eq!(
            kinds,
            [
                "child_spawn",
                "child_line",
                "child_line",
                "child_signal",
                "child_line",
                "child_exit",
                "fs_out",
                "s3_put",
                "shm"
            ]
        );
        assert_eq!(read.records[0].ev, TapeEvent::ChildSpawn(spawn));
        let counts: Vec<_> = read
            .records
            .iter()
            .filter_map(|r| match &r.ev {
                TapeEvent::ChildLine { line, count, .. } => Some((line.clone(), *count)),
                _ => None,
            })
            .collect();
        assert_eq!(
            counts,
            [
                (Some("New clock: GstSystemClock".to_string()), None),
                (None, Some(3)),
                (None, Some(1)),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncated_gzip_reads_up_to_the_last_flushed_line() {
        let dir = std::env::temp_dir().join(format!("xorgate-tape-trunc-{}", std::process::id()));
        let path = dir.join("cut.ndjson.gz");
        let (clock, t) = manual(0);
        let tape = FileTape::create_with_clock(&path, &TapeMeta::new("d", "b", 0), clock).unwrap();
        tape.nmea("one");
        tape.clock_now(); // forces a sync flush
        t.store(10, Ordering::SeqCst);
        tape.nmea("two (never flushed)");
        // Simulate SIGKILL: forget the tape without finishing the gzip stream.
        std::mem::forget(tape);
        let read = Tape::read(&path).unwrap();
        assert!(read.truncated);
        let kinds: Vec<_> = read.records.iter().map(|r| r.ev.kind()).collect();
        assert_eq!(kinds, ["nmea", "clock"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plain_ndjson_and_partial_last_line() {
        let text = "{\"v\":1,\"k\":\"meta\",\"deviceId\":\"d\",\"build\":\"b\",\"startWall\":0}\n\
                    {\"t\":0,\"w\":0,\"k\":\"conn\",\"state\":\"connected\"}\n\
                    {\"t\":1,\"w\":1,\"k\":\"nm";
        let read = TapeReader::from_reader(std::io::Cursor::new(text.as_bytes().to_vec()))
            .unwrap()
            .read_all()
            .unwrap();
        assert!(read.truncated);
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.meta.hardware, Value::Null);
        let bad = "{\"v\":1,\"k\":\"meta\",\"deviceId\":\"d\",\"build\":\"b\",\"startWall\":0}\n{\"t\":0,\"w\":0,\"k\":\"nope\"}\n";
        let mut rd =
            TapeReader::from_reader(std::io::Cursor::new(bad.as_bytes().to_vec())).unwrap();
        assert!(matches!(
            rd.next(),
            Some(Err(TapeError::Parse { line: 2, .. }))
        ));
        assert!(matches!(
            TapeReader::from_reader(std::io::Cursor::new(Vec::new())),
            Err(TapeError::Empty)
        ));
    }

    #[test]
    fn tape_sink_through_open_file_tape() {
        use crate::io::{open_file_tape, TapeSink, TapeStart};
        let dir = std::env::temp_dir().join(format!("xorgate-tape-sink-{}", std::process::id()));
        let p = dir.join("sub/t.ndjson.gz");
        let start = TapeStart {
            device_id: "a73f".into(),
            build: "xorgate-rs 0.0.1".into(),
            start_wall: 5,
            hardware: serde_json::json!({"platform": "cm5"}),
            kvs_channels: vec![],
            home_files: Some(serde_json::Map::from_iter([(
                "settings.json".to_string(),
                serde_json::Value::Null,
            )])),
        };
        let t: std::sync::Arc<dyn TapeSink> = open_file_tape(&p, &start).unwrap();
        assert!(t.enabled());
        t.nmea("$GPGGA");
        t.exec(&["vcgencmd", "get_throttled"], "throttled=0x0\n", Some(0));
        t.exec(&["vcgencmd", "get_throttled"], "throttled=0x0\n", Some(0));
        t.conn(false);
        t.clock(7, 8);
        t.close();
        t.close(); // idempotent
        assert!(!t.enabled());
        let tape = Tape::read(&p).unwrap();
        assert_eq!(tape.meta.device_id, "a73f");
        assert_eq!(tape.meta.hardware["platform"], "cm5");
        assert_eq!(tape.meta.source.as_deref(), Some("bench"));
        let kinds: Vec<_> = tape.records.iter().map(|r| r.ev.kind()).collect();
        assert_eq!(kinds, ["nmea", "exec", "conn", "clock"]);
        assert_eq!(tape.records[3].ev, TapeEvent::Clock { wall: 7, mono: 8 });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tape_write_is_byte_reproducible() {
        let mut tape = Tape {
            meta: TapeMeta::new("d", "synth", 5),
            records: vec![],
            truncated: false,
        };
        tape.records.push(TapeRecord {
            t: 0,
            w: 5,
            ev: TapeEvent::Nmea { line: "x".into() },
        });
        let dir = std::env::temp_dir().join(format!("xorgate-tape-rep-{}", std::process::id()));
        let (a, b) = (dir.join("a.gz"), dir.join("b.gz"));
        tape.write(&a, true).unwrap();
        tape.write(&b, true).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        assert_eq!(Tape::read(&a).unwrap().records, tape.records);
        std::fs::remove_dir_all(&dir).ok();
    }
}
