//! Cloud → device command channel — the PURE half of `src/agent/commands.ts`
//! (257 lines) plus the decisions the worker makes around it
//! (`src/worker.ts:905-1126`), re-expressed as data.
//!
//! The backend publishes a NON-retained JSON message to
//! `xorgate/devices/{deviceId}/command` when an operator triggers a one-shot
//! action from the web UI; the worker validates it, acks on
//! `xorgate/devices/{deviceId}/command/response`, then executes.
//!
//! Payload shape (`commands.ts:7-11`):
//! `{ "v": 1, "requestId": <uuid>, "command": "reboot"|"power_off"|"refresh"|
//! "update"|"read_sms"|"clear_sms"|"obd_scan"|"obd_forget", "ts": <epoch ms
//! issued>, "expiresAt": <epoch ms>, "args"?: { "limit": 1..50 } }` — `args`
//! for `read_sms` / `clear_sms` only (`commands.ts:7-11` @ 09da38d).
//!
//! Two independent guards stop a stale command from causing a surprise
//! reboot (`commands.ts:21-32`): a TTL (`expiresAt`, clamped to
//! [`MAX_TTL_MS`] past `ts`) and a replay ledger of executed requestIds
//! persisted to disk (`commands.json`, claimed BEFORE execution because
//! `systemctl reboot` does not return).
//!
//! **Invariant 4 in the types** (agent-anatomy §10): the agent never reboots
//! on its own. The only way to obtain a [`PrivilegedCall`] (the three
//! `sudo -n systemctl …` argv) is [`plan`] over a [`DeviceCommandName`] that
//! came from [`parse_command`] — a cloud command — and its fields are
//! private, so the Phase 4 HAL executes exactly those argv and nothing else.
//! No plan ever names `sim7600-lte` or `/dev/ttyUSB2`; `read_sms` resolves
//! its port from sysfs in the HAL (`sms::SMS_INTERFACE_NUMBER`).
//!
//! What stays out of core (I/O): `ledgerPath` (`commands.ts:167`), the
//! `readFileSync` in `loadLedger` (`:174`), `atomicWriteJson` in
//! `claimCommand` (`:213`), the `run` call in `executeCommand` (`:231`), and
//! the worker's publish / flag-file write / timers.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::clock::WallMs;
use crate::sms::clamp_sms_limit;
use crate::telemetry::js_num;

/// `DeviceCommandName` (`commands.ts:63`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceCommandName {
    Reboot,
    PowerOff,
    /// READ-ONLY (`commands.ts:41-48`): re-probe SIM/APN/bias + report now;
    /// in-process, never via the sudo path.
    Refresh,
    /// Starts the oneshot `xorgate-updater.service` (`commands.ts:50-56`).
    Update,
    /// READ-ONLY (`commands.ts:58-61`): sweep stored SMS over the spare AT
    /// port and publish on `…/sms`; in-process.
    ReadSms,
    /// DESTRUCTIVE sibling of `read_sms` (`commands.ts:63-69` @ 09da38d):
    /// delete-all (`xorgate_hal::at_clear`) on SM then ME, sweep, reply on `…/sms` with
    /// `action:"clear"`; in-process, never via sudo.
    ClearSms,
    /// OBD collector one-shot (`commands.ts:71-79` @ 09da38d): a 25 s
    /// Bluetooth discovery (never pairs); in-process.
    ObdScan,
    /// OBD collector one-shot: stop the helper, drop the bond, report
    /// `not_configured`; in-process.
    ObdForget,
}

impl DeviceCommandName {
    /// The `COMMANDS` set (`commands.ts:89-98` @ 09da38d), in declaration order.
    pub const ALL: [DeviceCommandName; 8] = [
        DeviceCommandName::Reboot,
        DeviceCommandName::PowerOff,
        DeviceCommandName::Refresh,
        DeviceCommandName::Update,
        DeviceCommandName::ReadSms,
        DeviceCommandName::ClearSms,
        DeviceCommandName::ObdScan,
        DeviceCommandName::ObdForget,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DeviceCommandName::Reboot => "reboot",
            DeviceCommandName::PowerOff => "power_off",
            DeviceCommandName::Refresh => "refresh",
            DeviceCommandName::Update => "update",
            DeviceCommandName::ReadSms => "read_sms",
            DeviceCommandName::ClearSms => "clear_sms",
            DeviceCommandName::ObdScan => "obd_scan",
            DeviceCommandName::ObdForget => "obd_forget",
        }
    }

    /// `COMMANDS.has(s)`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// `refresh`, `read_sms`, `clear_sms`, `obd_scan` and `obd_forget` are
    /// handled in-process by the worker (`commands.ts:261-275` @ 09da38d).
    pub fn is_in_process(self) -> bool {
        matches!(
            self,
            DeviceCommandName::Refresh
                | DeviceCommandName::ReadSms
                | DeviceCommandName::ClearSms
                | DeviceCommandName::ObdScan
                | DeviceCommandName::ObdForget
        )
    }

    /// The two commands that carry `args.limit` (`commands.ts:174` @ 09da38d).
    pub fn takes_sms_limit(self) -> bool {
        matches!(
            self,
            DeviceCommandName::ReadSms | DeviceCommandName::ClearSms
        )
    }
}

impl std::fmt::Display for DeviceCommandName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Hard ceiling on how long a command stays valid, measured from the
/// issuing timestamp (`MAX_TTL_MS`, `commands.ts:78`). The cloud sends its
/// own (shorter, 60 s) `expiresAt`; this caps it.
pub const MAX_TTL_MS: i64 = 120_000;

/// Executed requestIds are remembered this long — well past `MAX_TTL_MS`
/// (`LEDGER_RETENTION_MS`, `commands.ts:81`).
pub const LEDGER_RETENTION_MS: i64 = 3_600_000;

/// Backstop so a flood of commands cannot grow the ledger without bound
/// (`LEDGER_MAX_ENTRIES`, `commands.ts:84`).
pub const LEDGER_MAX_ENTRIES: usize = 100;

/// The ledger's file name under the runtime home (`ledgerPath`,
/// `commands.ts:167-169`).
pub const LEDGER_FILE_NAME: &str = "commands.json";

/// `DeviceCommand.args` (`commands.ts:98`): `limit` already clamped to
/// 1..50, defaulting to 10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandArgs {
    pub limit: u32,
}

/// `DeviceCommand` (`commands.ts:86-99`). Serde order = `parseCommand`'s
/// construction order (`:140-149`): requestId, command, ts, expiresAt,
/// then `args` only for `read_sms` / `clear_sms`. `ts`/`expiresAt` are JS numbers
/// (finite, possibly fractional) and print like JS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCommand {
    pub request_id: String,
    pub command: DeviceCommandName,
    /// Epoch ms the cloud issued the command.
    #[serde(with = "js_num")]
    pub ts: f64,
    /// Epoch ms after which the device must refuse it. Already TTL-clamped.
    #[serde(with = "js_num")]
    pub expires_at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<CommandArgs>,
}

/// Why a command was acked `rejected`. `expired` / `duplicate` are
/// `RejectReason` (`commands.ts:101`); `rate-limited` (`worker.ts:1049`) and
/// `managed by host image` (`worker.ts:1036`, replay mode `update`) are the
/// worker's two extra wordings. Serialises as the exact strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    #[serde(rename = "expired")]
    Expired,
    #[serde(rename = "duplicate")]
    Duplicate,
    #[serde(rename = "rate-limited")]
    RateLimited,
    #[serde(rename = "managed by host image")]
    ManagedByHostImage,
    /// `obd_scan` / `obd_forget` on a device without the OBD collector
    /// (`worker.ts:1224` @ 09da38d).
    #[serde(rename = "no OBD peripheral")]
    NoObdPeripheral,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::Expired => "expired",
            RejectReason::Duplicate => "duplicate",
            RejectReason::RateLimited => "rate-limited",
            RejectReason::ManagedByHostImage => "managed by host image",
            RejectReason::NoObdPeripheral => "no OBD peripheral",
        }
    }
}

/// Replay mode (device-simulator plan, Phase 2; `REPLAY_EXIT_CODES`,
/// `commands.ts:104-112`): the agent IS the container's main process, so a
/// command that would reboot or power off a device becomes an exit code the
/// host reads. `reboot` → 0 (Docker's `restart: unless-stopped` brings it
/// straight back); `power_off` → 3 (the entrypoint parks the container).
/// `update` is rejected in replay (managed by the host image).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayExitCodes {
    pub reboot: i32,
    pub power_off: i32,
}

pub const REPLAY_EXIT_CODES: ReplayExitCodes = ReplayExitCodes {
    reboot: 0,
    power_off: 3,
};

/// `containerExitCode` (`commands.ts:114-116`): `Some` for `reboot` /
/// `power_off`, `None` for the commands that never reach the container-exit
/// branch (`worker.ts:1090`).
pub fn container_exit_code(command: DeviceCommandName) -> Option<i32> {
    match command {
        DeviceCommandName::Reboot => Some(REPLAY_EXIT_CODES.reboot),
        DeviceCommandName::PowerOff => Some(REPLAY_EXIT_CODES.power_off),
        _ => None,
    }
}

/// `typeof v === "number" && Number.isFinite(v)` on a parsed JSON value
/// (every `serde_json` number is finite; `1e400` fails to parse at all,
/// which `JSON.parse` turns into `Infinity` → the same `null` result).
fn finite_number(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|f| f.is_finite())
}

/// Parse + validate a command payload (`parseCommand`, `commands.ts:126-151`).
/// `None` when it is not a well-formed v1 command for a known action — the
/// worker cannot meaningfully ack those (there may be no requestId to ack
/// against), so they are dropped with a log.
///
/// `expiresAt` is clamped to `ts + MAX_TTL_MS` here, so every downstream
/// expiry check is against a bound this device controls. `args` is read for
/// `read_sms` / `clear_sms` only (an object or array; anything else counts as `{}`) and
/// its `limit` clamped by [`clamp_sms_limit`]; every other command ignores
/// `args` entirely (contract §1).
pub fn parse_command(payload: &[u8]) -> Option<DeviceCommand> {
    let text = String::from_utf8_lossy(payload);
    let obj: Value = serde_json::from_str(&text).ok()?;
    let o = obj.as_object()?;
    if o.get("v").and_then(Value::as_f64) != Some(1.0) {
        return None;
    }
    let request_id = o.get("requestId")?.as_str()?;
    if request_id.is_empty() {
        return None;
    }
    let command = DeviceCommandName::parse(o.get("command")?.as_str()?)?;
    let ts = finite_number(o.get("ts"))?;
    let expires_at = finite_number(o.get("expiresAt"))?;
    let mut cmd = DeviceCommand {
        request_id: request_id.to_string(),
        command,
        ts,
        expires_at: expires_at.min(ts + MAX_TTL_MS as f64),
        args: None,
    };
    if command.takes_sms_limit() {
        // `o.args && typeof o.args === "object"`: objects AND arrays pass,
        // and `args.limit` on an array is undefined → the default.
        let limit = match o.get("args") {
            Some(Value::Object(a)) => clamp_sms_limit(a.get("limit")),
            _ => clamp_sms_limit(None),
        };
        cmd.args = Some(CommandArgs { limit });
    }
    Some(cmd)
}

/// `isExpired` (`commands.ts:153-155`): `now > cmd.expiresAt`.
pub fn is_expired(cmd: &DeviceCommand, now: WallMs) -> bool {
    (now as f64) > cmd.expires_at
}

// ---------------------------------------------------------------------------
// Replay ledger (`commands.ts:157-218`)
// ---------------------------------------------------------------------------

/// `LedgerEntry` (`commands.ts:157-160`). Unknown keys on an entry survive a
/// load/claim/save round trip exactly as in the TS (the filter at `:180-186`
/// keeps the whole object), hence the flattened `extra` map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEntry {
    pub request_id: String,
    #[serde(with = "js_num")]
    pub at: f64,
    #[serde(
        flatten,
        skip_serializing_if = "serde_json::Map::is_empty",
        serialize_with = "crate::js::json_map::serialize"
    )]
    pub extra: serde_json::Map<String, Value>,
}

/// `Ledger` (`commands.ts:162-165`): the byte-shape of `commands.json`
/// (`atomicWriteJson` writes compact `JSON.stringify`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub v: u8,
    pub executed: Vec<LedgerEntry>,
}

impl Ledger {
    /// `{ v: 1, executed: [] }` — what a missing / unreadable / invalid file
    /// reads as (`commands.ts:193`).
    pub fn empty() -> Self {
        Ledger {
            v: 1,
            executed: Vec::new(),
        }
    }

    pub fn contains(&self, request_id: &str) -> bool {
        self.executed.iter().any(|e| e.request_id == request_id)
    }
}

impl Default for Ledger {
    fn default() -> Self {
        Self::empty()
    }
}

/// Read the replay ledger from the file's text, or an empty one when
/// absent (`None`), unreadable, not JSON, not `v: 1`, or without an
/// `executed` array (`loadLedger`, `commands.ts:172-194`). Entries are
/// kept only when they are objects with a string `requestId` and a numeric
/// `at` (`:180-186`).
pub fn load_ledger(text: Option<&str>) -> Ledger {
    let Some(text) = text else {
        return Ledger::empty();
    };
    let Ok(obj) = serde_json::from_str::<Value>(text) else {
        return Ledger::empty();
    };
    let Some(o) = obj.as_object() else {
        return Ledger::empty();
    };
    if o.get("v").and_then(Value::as_f64) != Some(1.0) {
        return Ledger::empty();
    }
    let Some(Value::Array(executed)) = o.get("executed") else {
        return Ledger::empty();
    };
    let executed = executed
        .iter()
        .filter_map(|e| {
            let m = e.as_object()?;
            let request_id = m.get("requestId")?.as_str()?.to_string();
            let at = m.get("at")?.as_f64()?;
            let extra = m
                .iter()
                .filter(|(k, _)| k.as_str() != "requestId" && k.as_str() != "at")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            Some(LedgerEntry {
                request_id,
                at,
                extra,
            })
        })
        .collect();
    Ledger { v: 1, executed }
}

/// What [`claim`] decided.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimOutcome {
    /// `true`: first sighting — persist `ledger` BEFORE executing. `false`:
    /// a redelivery — the caller must NOT execute, and the TS does not touch
    /// the file (`commands.ts:210`), so `ledger` is the input unchanged.
    pub accepted: bool,
    pub ledger: Ledger,
}

/// The pure core of `claimCommand` (`commands.ts:204-218`): refuse a
/// requestId already in the ledger; otherwise prune entries older than
/// [`LEDGER_RETENTION_MS`] (`now - at < retention` keeps), append
/// `{requestId, at: now}` and keep the LAST [`LEDGER_MAX_ENTRIES`].
///
/// Call this BEFORE executing, never after: `systemctl reboot` does not
/// return, so an "execute then record" ordering would leave no record at
/// all and let the same command fire again on the next connect.
pub fn claim(ledger: &Ledger, request_id: &str, now: WallMs) -> ClaimOutcome {
    if ledger.contains(request_id) {
        return ClaimOutcome {
            accepted: false,
            ledger: ledger.clone(),
        };
    }
    let now_f = now as f64;
    let mut kept: Vec<LedgerEntry> = ledger
        .executed
        .iter()
        .filter(|e| now_f - e.at < LEDGER_RETENTION_MS as f64)
        .cloned()
        .collect();
    kept.push(LedgerEntry {
        request_id: request_id.to_string(),
        at: now_f,
        extra: serde_json::Map::new(),
    });
    let drop = kept.len().saturating_sub(LEDGER_MAX_ENTRIES);
    kept.drain(..drop);
    ClaimOutcome {
        accepted: true,
        ledger: Ledger {
            v: 1,
            executed: kept,
        },
    }
}

// ---------------------------------------------------------------------------
// Execution plan (`commands.ts:229-257`) — data, executed by the HAL
// ---------------------------------------------------------------------------

/// The shape of `lib/exec.ts`'s `RunResult` (`exec.ts:12-19`), in the key
/// order `executeCommand`'s in-process literal uses (`commands.ts:236-242`).
/// Never on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub failed: bool,
    /// Set when the binary itself is missing (ENOENT) rather than exiting
    /// non-zero.
    pub not_found: bool,
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
}

/// One of the three privileged argv. Fields are private on purpose: only
/// [`plan`] builds one (module doc, invariant 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivilegedCall {
    program: &'static str,
    args: &'static [&'static str],
}

impl PrivilegedCall {
    /// `sudo` — `-n` (non-interactive) so a device whose sudoers drop-in is
    /// missing fails fast and loudly instead of hanging on a password prompt
    /// (`commands.ts:220-227`).
    pub fn program(&self) -> &'static str {
        self.program
    }
    pub fn args(&self) -> &'static [&'static str] {
        self.args
    }
    /// `[program, args…]`.
    pub fn argv(&self) -> Vec<&'static str> {
        std::iter::once(self.program)
            .chain(self.args.iter().copied())
            .collect()
    }
}

/// `sudo -n systemctl reboot` (`commands.ts:255-256`). `reboot`/`poweroff`
/// live in /usr/sbin, off the service user's PATH; `systemctl` is on PATH
/// and is the same call underneath.
const REBOOT: PrivilegedCall = PrivilegedCall {
    program: "sudo",
    args: &["-n", "systemctl", "reboot"],
};
/// `sudo -n systemctl poweroff` (`commands.ts:255-256`).
const POWEROFF: PrivilegedCall = PrivilegedCall {
    program: "sudo",
    args: &["-n", "systemctl", "poweroff"],
};
/// `sudo -n systemctl start --no-block xorgate-updater.service`
/// (`commands.ts:244-253`). These argv words must match
/// `/etc/sudoers.d/022_xorgate-update` BYTE-FOR-BYTE — the grant is
/// exact-args. `--no-block` is load-bearing: a blocking start on the oneshot
/// would wait for the updater, which finishes by restarting this very agent.
const UPDATE: PrivilegedCall = PrivilegedCall {
    program: "sudo",
    args: &[
        "-n",
        "systemctl",
        "start",
        "--no-block",
        "xorgate-updater.service",
    ],
};

/// `executeCommand` (`commands.ts:229-257`) as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandPlan {
    /// Run this argv (`reboot`, `power_off`, `update`).
    Privileged(PrivilegedCall),
    /// `refresh` / `read_sms` / `clear_sms` / `obd_*`: handled in-process by the worker; routing it
    /// into `executeCommand` yields the failed [`RunResult`] of
    /// [`in_process_run_result`] and never reaches sudo.
    InProcess(DeviceCommandName),
}

/// `executeCommand`'s dispatch (`commands.ts:233-256`) without the `run`.
pub fn plan(command: DeviceCommandName) -> CommandPlan {
    match command {
        DeviceCommandName::Refresh
        | DeviceCommandName::ReadSms
        | DeviceCommandName::ClearSms
        | DeviceCommandName::ObdScan
        | DeviceCommandName::ObdForget => CommandPlan::InProcess(command),
        DeviceCommandName::Update => CommandPlan::Privileged(UPDATE),
        DeviceCommandName::Reboot => CommandPlan::Privileged(REBOOT),
        DeviceCommandName::PowerOff => CommandPlan::Privileged(POWEROFF),
    }
}

/// The `RunResult` `executeCommand` resolves for an in-process command
/// (`commands.ts:236-242`).
pub fn in_process_run_result(command: DeviceCommandName) -> RunResult {
    RunResult {
        failed: true,
        not_found: false,
        exit_code: Some(1),
        stdout: String::new(),
        stderr: format!("{command} is handled in-process, not via executeCommand"),
    }
}

/// The `reason` the worker acks when the privileged call fails
/// (`worker.ts:1118-1120`): `"sudo not found"` when the binary is missing,
/// else the trimmed stderr, else `exit <code>` (`exit null` when the process
/// had no exit code — JS prints `null`).
pub fn failure_reason(res: &RunResult) -> String {
    if res.not_found {
        return "sudo not found".to_string();
    }
    let stderr = res.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    match res.exit_code {
        Some(c) => format!("exit {c}"),
        None => "exit null".to_string(),
    }
}

// ---------------------------------------------------------------------------
// `refresh` (worker.ts:905-909, 1047-1067)
// ---------------------------------------------------------------------------

/// Device-side floor between honoured refresh commands
/// (`REFRESH_MIN_INTERVAL_MS`, `worker.ts:906`).
pub const REFRESH_MIN_INTERVAL_MS: i64 = 30_000;
/// How long after the flag before the re-probed values are re-reported
/// (`REFRESH_REPROBE_REPORT_MS`, `worker.ts:908`).
pub const REFRESH_REPROBE_REPORT_MS: i64 = 45_000;
/// The flag file the daemon (root) polls for in both its loops
/// (`worker.ts:1058`), under the runtime home.
pub const REFRESH_FLAG_FILE: &str = "refresh-modem.flag";
/// How long to wait for the ack's PUBACK before going down anyway
/// (`ACK_FLUSH_TIMEOUT_MS`, `worker.ts:903`).
pub const ACK_FLUSH_TIMEOUT_MS: i64 = 3000;

/// `now - lastRefreshAt < REFRESH_MIN_INTERVAL_MS` (`worker.ts:1048`):
/// a hammered UI button must not turn into unbounded AT traffic.
/// `last_refresh_at` starts at 0 (`worker.ts:909`).
pub fn refresh_rate_limited(now: WallMs, last_refresh_at: WallMs) -> bool {
    now - last_refresh_at < REFRESH_MIN_INTERVAL_MS
}

/// What an accepted `refresh` does (`worker.ts:1052-1067`): write
/// `String(now)` to the flag file, request a config report NOW and again
/// after [`REFRESH_REPROBE_REPORT_MS`] so the re-probed values land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshPlan {
    pub flag_file: &'static str,
    pub flag_content: String,
    pub report_now: bool,
    pub report_again_after_ms: i64,
}

pub fn refresh_plan(now: WallMs) -> RefreshPlan {
    RefreshPlan {
        flag_file: REFRESH_FLAG_FILE,
        flag_content: now.to_string(),
        report_now: true,
        report_again_after_ms: REFRESH_REPROBE_REPORT_MS,
    }
}

// ---------------------------------------------------------------------------
// Ack (`…/command/response`; worker.ts:953-968)
// ---------------------------------------------------------------------------

/// `status` of a command ack (`worker.ts:956`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AckStatus {
    Accepted,
    Rejected,
    Failed,
}

/// The `…/command/response` payload (`publishCommandAck`,
/// `worker.ts:953-968`): `{ v: 1, deviceId, ts: Date.now(), ...ack }` with
/// `ack = { ...cmd, status, reason? }`.
///
/// ⚠️ Byte-shape consequence of that spread: `ack` carries the command's own
/// `ts` key, which OVERWRITES the `Date.now()` value while keeping the third
/// position — so the wire `ts` is the command's issue time, not the ack
/// time, and `expiresAt` + `args` ride along. Key order: v, deviceId, ts,
/// requestId, command, expiresAt, args?, status, reason?. Nothing consumes
/// this topic cloud-side today (contract §2), but the shape is preserved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandAck {
    pub v: u8,
    pub device_id: String,
    /// `cmd.ts` (see above).
    #[serde(with = "js_num")]
    pub ts: f64,
    pub request_id: String,
    pub command: DeviceCommandName,
    #[serde(with = "js_num")]
    pub expires_at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<CommandArgs>,
    pub status: AckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl CommandAck {
    pub fn new(
        device_id: &str,
        cmd: &DeviceCommand,
        status: AckStatus,
        reason: Option<String>,
    ) -> Self {
        CommandAck {
            v: 1,
            device_id: device_id.to_string(),
            ts: cmd.ts,
            request_id: cmd.request_id.clone(),
            command: cmd.command,
            expires_at: cmd.expires_at,
            args: cmd.args,
            status,
            reason,
        }
    }

    pub fn accepted(device_id: &str, cmd: &DeviceCommand) -> Self {
        Self::new(device_id, cmd, AckStatus::Accepted, None)
    }

    pub fn rejected(device_id: &str, cmd: &DeviceCommand, reason: RejectReason) -> Self {
        Self::new(
            device_id,
            cmd,
            AckStatus::Rejected,
            Some(reason.as_str().to_string()),
        )
    }

    pub fn failed(device_id: &str, cmd: &DeviceCommand, reason: String) -> Self {
        Self::new(device_id, cmd, AckStatus::Failed, Some(reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_extra_keys_print_like_js() {
        // A copy-through key's number goes out as `JSON.stringify` prints it
        // (review-integration M1), not serde_json's `5e-6` / `1e20`.
        let l = load_ledger(Some(
            r#"{"v":1,"executed":[{"requestId":"r","at":1,"x":0.000005,"y":100000000000000000000}]}"#,
        ));
        assert_eq!(
            serde_json::to_string(&l).unwrap(),
            r#"{"v":1,"executed":[{"requestId":"r","at":1,"x":0.000005,"y":100000000000000000000}]}"#
        );
    }

    #[test]
    fn only_plan_hands_out_privileged_argv() {
        assert_eq!(
            plan(DeviceCommandName::Reboot),
            CommandPlan::Privileged(REBOOT)
        );
        if let CommandPlan::Privileged(c) = plan(DeviceCommandName::Update) {
            assert_eq!(
                c.argv(),
                vec![
                    "sudo",
                    "-n",
                    "systemctl",
                    "start",
                    "--no-block",
                    "xorgate-updater.service"
                ]
            );
        } else {
            panic!("update is privileged");
        }
        assert_eq!(
            plan(DeviceCommandName::Refresh),
            CommandPlan::InProcess(DeviceCommandName::Refresh)
        );
        for c in DeviceCommandName::ALL {
            if let CommandPlan::Privileged(p) = plan(c) {
                for word in p.argv() {
                    assert!(!word.contains("sim7600"), "never restarts the modem daemon");
                    assert!(!word.contains("ttyUSB"), "never names a tty");
                }
            }
        }
    }

    #[test]
    fn refresh_rate_limit_window() {
        assert!(!refresh_rate_limited(1_000_000, 0));
        assert!(refresh_rate_limited(29_999, 0));
        assert!(!refresh_rate_limited(30_000, 0));
        let p = refresh_plan(1750000000000);
        assert_eq!(p.flag_content, "1750000000000");
        assert_eq!(p.report_again_after_ms, 45_000);
    }

    #[test]
    fn exit_codes() {
        assert_eq!(container_exit_code(DeviceCommandName::Reboot), Some(0));
        assert_eq!(container_exit_code(DeviceCommandName::PowerOff), Some(3));
        assert_eq!(container_exit_code(DeviceCommandName::Update), None);
    }
}

// ---------------------------------------------------------------------------
// Phase 4 (I5): the two sudo helpers — `xorgate-setclock` (sudoers 021) and
// `xorgate-camrotate` (sudoers 023)
// ---------------------------------------------------------------------------

/// One privileged helper invocation. Next to the three [`PrivilegedCall`]s
/// these are the ONLY other sudo argv the agent can run (invariant 4, I5).
///
/// The TS has no such type: `clock-step.ts:186` and `boot-config.ts:285`
/// call `run("sudo", ["-n", HELPER, …])` with whatever values they hold and
/// let the helper re-validate them. Here the fields are private and the only
/// constructors are [`crate::clock_step::setclock_call`] and
/// [`crate::boot_config::camrotate_call`], which apply the helper's OWN
/// validation (`system/xorgate-setclock:16-21`, `system/xorgate-camrotate:39-50`)
/// first, so a value the helper would refuse never reaches sudo. A refusal
/// carries the helper's own stderr line, so the caller's `detail` (cloud-
/// visible for the camera mount) reads exactly as if the helper had run
/// (accepted divergence D33: no sudo journal line for a refused call).
///
/// Shipped form vs the frozen I5 sketch (`pub enum HelperCall { SetClock {..},
/// CamRotate {..} }`): a struct over the read-only [`HelperKind`] enum, so the
/// validated values cannot be edited after minting (an enum with public
/// fields could be built or mutated anywhere).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperCall {
    kind: HelperKind,
}

/// What a [`HelperCall`] does (read-only view; build one through the mint fns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperKind {
    /// `sudo -n /usr/local/bin/xorgate-setclock <epoch-seconds>` (`clock-step.ts:186`).
    SetClock { epoch_s: u64 },
    /// `sudo -n /usr/local/bin/xorgate-camrotate <sensor> <camN> <0|180>`
    /// (`boot-config.ts:285`).
    CamRotate {
        sensor: String,
        cam_token: String,
        deg: u16,
    },
}

impl HelperCall {
    /// Crate-private: only the two mint functions call this.
    pub(crate) fn mint(kind: HelperKind) -> Self {
        Self { kind }
    }

    pub fn kind(&self) -> &HelperKind {
        &self.kind
    }

    /// The helper's absolute path (the sudoers grants name it exactly).
    pub fn helper(&self) -> &'static str {
        match self.kind {
            HelperKind::SetClock { .. } => crate::clock_step::SETCLOCK_HELPER,
            HelperKind::CamRotate { .. } => crate::boot_config::CAMROTATE_HELPER,
        }
    }

    /// The helper's own arguments, as the TS stringifies them
    /// (`String(Math.round(targetMs / 1000))`, `String(deg)`).
    pub fn helper_args(&self) -> Vec<String> {
        match &self.kind {
            HelperKind::SetClock { epoch_s } => vec![epoch_s.to_string()],
            HelperKind::CamRotate {
                sensor,
                cam_token,
                deg,
            } => vec![sensor.clone(), cam_token.clone(), deg.to_string()],
        }
    }

    /// `["sudo", "-n", <helper>, …]` — always `-n` (fail fast, never prompt).
    pub fn argv(&self) -> Vec<String> {
        let mut v = vec![
            "sudo".to_string(),
            "-n".to_string(),
            self.helper().to_string(),
        ];
        v.extend(self.helper_args());
        v
    }

    /// `run(…, { timeoutMs })`: the camera helper is bounded at 20 s
    /// (`boot-config.ts:285-287`); the clock helper has no timeout
    /// (`clock-step.ts:186`).
    pub fn timeout_ms(&self) -> Option<u64> {
        match self.kind {
            HelperKind::SetClock { .. } => None,
            HelperKind::CamRotate { .. } => Some(20_000),
        }
    }
}

#[cfg(test)]
mod helper_call_tests {
    use super::*;
    use crate::clock_step::{setclock_argv, setclock_call};

    #[test]
    fn setclock_argv_is_sudo_plus_the_phase1_argv() {
        let call = setclock_call(1_785_326_700_499.0).unwrap();
        let mut want = vec!["sudo".to_string()];
        want.extend(setclock_argv(1_785_326_700_499.0));
        assert_eq!(call.argv(), want);
        assert_eq!(call.timeout_ms(), None);
        assert_eq!(
            call.kind(),
            &HelperKind::SetClock {
                epoch_s: 1_785_326_700
            }
        );
    }
}
