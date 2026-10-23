//! The OBD collector's state machine — `createObdCollector` of
//! `src/agent/collectors/obd.ts:280-788` @ 09da38d, sans I/O.
//!
//! The TS closure owns a helper process, two timers (`stableTimer`,
//! `restartTimer`), a settings reader, a scanner and an unbonder. Here every
//! edge is an input method and every effect an [`ObdAction`] the runtime
//! (`xorgate-agent::collectors::obd`) performs, in order:
//!
//! | TS effect | action |
//! |---|---|
//! | `spawnHelper(resolved.helper, …)` | [`ObdAction::Spawn`] (a fresh handle id) |
//! | `handle.stop("SIGINT")` | [`ObdAction::Sigint`] |
//! | `setTimeout(…)` / `clearTimeout(…)` | [`ObdAction::Arm`] / [`ObdAction::Disarm`] |
//! | `console.log/warn/error("[obd] …")` | [`ObdAction::Log`] |
//! | `opts.onEvent(level, "obd: …")` | [`ObdAction::Event`] |
//!
//! Inputs: [`ObdSupervisor::tick`] (the lazy start + 1 s settings poll that
//! every `read()`/`sample()` runs), [`ObdSupervisor::reapply`],
//! [`ObdSupervisor::on_message`] (a parsed helper line), [`ObdSupervisor::on_stderr`],
//! [`ObdSupervisor::on_exit`] (a stale handle's exit is ignored — `handle !== h`),
//! [`ObdSupervisor::timer_fired`] (a stale generation is ignored — the
//! `clearTimeout` that raced the fire), the scan/forget halves and
//! [`ObdSupervisor::stop`].
//!
//! Time (invariant 2, D37 candidate): every INTERVAL is measured on the
//! injected monotonic ms — the settings poll (1 s), sample freshness (5 s),
//! `ranMs` vs `STABLE_MS`, the adapter-absent re-warn (10 min) — where the TS
//! uses `Date.now()`. The wall clock stamps only what the wire carries:
//! `status().since`, `scan.ts`, and an audit line's fallback `ts`. With a
//! clock that does not step (the TS tests' fake timers) the two agree, which
//! is what `fixtures/obd/supervisor-traces.json` replays.

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::js::{number_to_string, object_key_order};
use crate::obd::{
    bound_inventory, obd_exit, obd_field_unit, profile_summary, request_rate, resolve_obd_config,
    AuditSnapshot, LastStop, ObdEnv, ObdHelperConfig, ObdMessage, ObdOff, ObdResolvedConfig,
    ObdScanCandidate, ObdScanResult, ObdSettings, ObdSource, MAX_INVENTORY_KEYS, OBD_SCAN_SECONDS,
};
use crate::round::js_math_round;
use crate::telemetry::{MetricGroup, MetricLeaf, ObdStatus};

/// A helper alive this long is up; resets backoff (`STABLE_MS`).
pub const STABLE_MS: u64 = 15_000;
pub const BASE_BACKOFF_MS: u64 = 2_000;
pub const MAX_BACKOFF_MS: u64 = 30_000;
pub const MAX_QUICK_FAILURES: u32 = 3;
/// 3 quick failures, or exit 6.
pub const PARKED_RETRY_MS: u64 = 60_000;
/// Exit 4: the car's protocol will not change in a minute.
pub const PROTOCOL_PARK_MS: u64 = 600_000;
pub const SAMPLE_FRESH_MS: u64 = 5_000;
/// settings.json is re-read at most this often from the tick path.
pub const SETTINGS_POLL_MS: u64 = 1_000;
/// While the adapter stays absent, re-warn at most this often.
pub const ADAPTER_ABSENT_REWARN_MS: u64 = 600_000;

/// Both clocks at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObdNow {
    /// `Date.now()` (epoch ms).
    pub wall_ms: f64,
    /// Monotonic ms (any origin).
    pub mono_ms: u64,
}

/// Console / activity-event level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObdLevel {
    Info,
    Warn,
    Error,
}

impl ObdLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ObdLevel::Info => "info",
            ObdLevel::Warn => "warn",
            ObdLevel::Error => "error",
        }
    }
}

/// The supervisor's two timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObdTimer {
    /// `stableTimer`: 15 s after a spawn, resets the backoff.
    Stable,
    /// `restartTimer`: the backoff / park delay before the next spawn.
    Restart,
}

/// One effect for the runtime, in emission order.
#[derive(Debug, Clone, PartialEq)]
pub enum ObdAction {
    /// Spawn the helper with this config; its exit reports `handle`.
    Spawn { handle: u64, cfg: ObdHelperConfig },
    /// `handle.stop("SIGINT")` (the helper ATZs and closes).
    Sigint { handle: u64 },
    /// `setTimeout`: fire [`ObdSupervisor::timer_fired`]`(timer, gen)` after `delay_ms`.
    Arm {
        timer: ObdTimer,
        gen: u64,
        delay_ms: u64,
    },
    /// `clearTimeout` (the generation already moved; a late fire is ignored).
    Disarm { timer: ObdTimer },
    /// A console line (`[obd] …`).
    Log { level: ObdLevel, line: String },
    /// An activity event (`obd: …`, the worker's `store.event` + flush).
    Event { level: ObdLevel, msg: String },
}

/// The settings source: `readSettings()` → the `obd` namespace (`None` =
/// absent), or the read error text.
pub type ReadSettings<'a> = &'a mut dyn FnMut() -> Result<Option<ObdSettings>, String>;

#[derive(Debug, Clone, PartialEq)]
struct Sample {
    fields: IndexMap<String, f64>,
    at_mono: u64,
}

#[derive(Debug, Clone, Copy)]
struct Absent {
    since_mono: u64,
    last_warn_mono: u64,
}

/// `{run, off, helper}` — the TS `resolvedKey` (a JSON string) as a value.
type ResolvedKey = (bool, Option<ObdOff>, ObdHelperConfig);

/// `createObdCollector(opts)`'s closure state.
#[derive(Debug)]
pub struct ObdSupervisor {
    env: ObdEnv,
    started: bool,
    stopped: bool,
    state: String,
    since: f64,
    detail: Option<String>,
    link: u8,

    resolved: ObdResolvedConfig,
    resolved_key: Option<ResolvedKey>,
    settings_read_at: Option<u64>,
    forgotten_mac: Option<String>,
    restart_pending: bool,

    handle: Option<u64>,
    next_handle: u64,
    started_at_mono: u64,
    backoff_ms: u64,
    quick_failures: u32,
    timer_gen: [u64; 2],
    timer_armed: [bool; 2],

    helper_version: Option<String>,
    allowlist_sha: Option<String>,
    profile: Option<Map<String, Value>>,
    sample: Option<Sample>,
    /// `changedMono`: advances on every sample and link change.
    stamp: u64,
    audit: Option<AuditSnapshot>,
    prev_audit: Option<AuditSnapshot>,
    audit_seq: u64,
    last_error: Option<String>,
    absent: Option<Absent>,
    scan: Option<ObdScanResult>,
    scan_in_flight: bool,

    out: Vec<ObdAction>,
}

fn timer_idx(t: ObdTimer) -> usize {
    match t {
        ObdTimer::Stable => 0,
        ObdTimer::Restart => 1,
    }
}

/// `str.slice(0, n)` (UTF-16 units).
fn slice16(s: &str, n: usize) -> String {
    String::from_utf16_lossy(&s.encode_utf16().take(n).collect::<Vec<u16>>())
}

/// `describe(r)` (`collectors/obd.ts:354-359`).
fn describe(r: &ObdResolvedConfig) -> String {
    let h = &r.helper;
    format!(
        "{}, {} Hz, {}, {} power, from {}",
        match &h.serial {
            Some(s) => format!("serial {s}"),
            None => format!("adapter {}", h.adapter.as_deref().unwrap_or("null")),
        },
        number_to_string(h.poll_hz),
        match &h.fields {
            Some(f) => format!("{} fields", f.len()),
            None => "all fields".to_string(),
        },
        h.power_source,
        r.source.as_str()
    )
}

impl ObdSupervisor {
    /// `createObdCollector({ env, … })` at `now` (`since = now()`; the
    /// initial `resolved` is the not-configured env seam with no serial).
    pub fn new(env: ObdEnv, now: ObdNow) -> Self {
        Self {
            env,
            started: false,
            stopped: false,
            state: "starting".into(),
            since: now.wall_ms,
            detail: None,
            link: 0,
            resolved: resolve_obd_config(None, &ObdEnv::default()),
            resolved_key: None,
            settings_read_at: None,
            forgotten_mac: None,
            restart_pending: false,
            handle: None,
            next_handle: 1,
            started_at_mono: 0,
            backoff_ms: BASE_BACKOFF_MS,
            quick_failures: 0,
            timer_gen: [0; 2],
            timer_armed: [false; 2],
            helper_version: None,
            allowlist_sha: None,
            profile: None,
            sample: None,
            stamp: 0,
            audit: None,
            prev_audit: None,
            audit_seq: 0,
            last_error: None,
            absent: None,
            scan: None,
            scan_in_flight: false,
            out: Vec::new(),
        }
    }

    // ----- effect plumbing -----

    fn take(&mut self) -> Vec<ObdAction> {
        std::mem::take(&mut self.out)
    }

    fn log(&mut self, level: ObdLevel, line: String) {
        self.out.push(ObdAction::Log { level, line });
    }

    /// `event(level, msg)`: the console line, then the activity event.
    fn event(&mut self, level: ObdLevel, msg: &str) {
        self.log(level, format!("[obd] {msg}"));
        self.out.push(ObdAction::Event {
            level,
            msg: format!("obd: {msg}"),
        });
    }

    fn set_state(&mut self, next: &str, detail: Option<String>, now: ObdNow) -> bool {
        self.detail = detail;
        if next == self.state {
            return false;
        }
        self.state = next.to_string();
        self.since = now.wall_ms;
        true
    }

    fn set_link(&mut self, next: u8) {
        if next == self.link {
            return;
        }
        self.link = next;
        self.stamp += 1;
    }

    fn arm(&mut self, timer: ObdTimer, delay_ms: u64) {
        let i = timer_idx(timer);
        self.timer_gen[i] += 1;
        self.timer_armed[i] = true;
        self.out.push(ObdAction::Arm {
            timer,
            gen: self.timer_gen[i],
            delay_ms,
        });
    }

    fn disarm(&mut self, timer: ObdTimer) {
        let i = timer_idx(timer);
        if self.timer_armed[i] {
            self.timer_gen[i] += 1;
            self.timer_armed[i] = false;
            self.out.push(ObdAction::Disarm { timer });
        }
    }

    fn clear_timers(&mut self) {
        self.disarm(ObdTimer::Stable);
        self.disarm(ObdTimer::Restart);
    }

    fn sigint(&mut self) {
        if let Some(handle) = self.handle {
            self.out.push(ObdAction::Sigint { handle });
        }
    }

    // ----- the TS functions -----

    /// `goOff(off, why, level)` (`:362-371`).
    fn go_off(&mut self, off: ObdOff, why: &str, level: ObdLevel, now: ObdNow) {
        self.clear_timers();
        self.restart_pending = false;
        self.sample = None;
        self.absent = None;
        let changed = self.set_state(off.as_str(), None, now);
        self.set_link(0);
        self.sigint(); // onExit sees !resolved.run and stays off
        if changed {
            self.event(level, why);
        }
    }

    /// `applySettings(force)` (`:381-433`).
    fn apply_settings(&mut self, force: bool, now: ObdNow, read: ReadSettings<'_>) {
        if self.stopped {
            return;
        }
        let t = now.mono_ms;
        if !force {
            if let Some(at) = self.settings_read_at {
                if t.saturating_sub(at) < SETTINGS_POLL_MS {
                    return;
                }
            }
        }
        self.settings_read_at = Some(t);
        let mut next = match read() {
            Ok(s) => resolve_obd_config(s.as_ref(), &self.env),
            Err(e) => {
                self.log(ObdLevel::Warn, format!("[obd] settings read failed: {e}"));
                return;
            }
        };
        if self.forgotten_mac.is_some() && next.helper.adapter != self.forgotten_mac {
            self.forgotten_mac = None;
        }
        if self.forgotten_mac.is_some() && next.run && next.helper.serial.is_none() {
            next.run = false;
            next.off = Some(ObdOff::NotConfigured);
        }
        let key: ResolvedKey = (next.run, next.off, next.helper.clone());
        let first = self.resolved_key.is_none();
        if self.resolved_key.as_ref() == Some(&key) {
            return;
        }
        let was_running = self.resolved.run;
        self.resolved = next;
        self.resolved_key = Some(key);

        if !self.resolved.run {
            let off = self.resolved.off.unwrap_or(ObdOff::NotConfigured);
            let why = if off == ObdOff::Disabled {
                "disabled by settings (obd.enabled=false); helper stopped, link 0".to_string()
            } else if let Some(mac) = &self.forgotten_mac {
                format!("adapter {mac} forgotten; waiting for new obd settings (link 0)")
            } else {
                "adapter not configured (set obd.adapterMac or XORGATE_OBD_ADAPTER); link 0"
                    .to_string()
            };
            let level = if off == ObdOff::Disabled {
                ObdLevel::Info
            } else {
                ObdLevel::Warn
            };
            self.go_off(off, &why, level, now);
            return;
        }
        self.absent = None;
        if self.handle.is_some() {
            // A live helper: SIGINT it and respawn with the new argv the
            // moment it exits. No backoff — the operator asked for this.
            self.restart_pending = true;
            self.clear_timers();
            let msg = format!(
                "settings changed ({}); restarting helper",
                describe(&self.resolved)
            );
            self.event(ObdLevel::Info, &msg);
            self.set_state("starting", None, now);
            self.sigint();
            return;
        }
        self.clear_timers();
        self.quick_failures = 0;
        self.backoff_ms = BASE_BACKOFF_MS;
        let msg = format!("starting helper ({})", describe(&self.resolved));
        if !first || was_running {
            self.event(ObdLevel::Info, &msg);
        } else {
            self.log(ObdLevel::Info, format!("[obd] {msg}"));
        }
        self.spawn(now);
    }

    /// `spawn()` (`:522-546`).
    fn spawn(&mut self, now: ObdNow) {
        if self.stopped || !self.resolved.run {
            return;
        }
        self.last_error = None;
        self.audit_seq = 0;
        self.started_at_mono = now.mono_ms;
        self.set_state("connecting", None, now);
        let handle = self.next_handle;
        self.next_handle += 1;
        self.handle = Some(handle);
        self.out.push(ObdAction::Spawn {
            handle,
            cfg: self.resolved.helper.clone(),
        });
        self.arm(ObdTimer::Stable, STABLE_MS);
    }

    /// `park(ms, why, level)` (`:548-555`).
    fn park(&mut self, ms: u64, why: &str, level: ObdLevel, now: ObdNow) {
        self.set_state("parked", Some(why.to_string()), now);
        self.set_link(0);
        self.quick_failures = 0;
        self.backoff_ms = BASE_BACKOFF_MS;
        let msg = format!(
            "{why}; parked for {} s",
            number_to_string(js_math_round(ms as f64 / 1000.0))
        );
        self.event(level, &msg);
        self.schedule_restart(ms);
    }

    /// `scheduleRestart(delayMs)` (`:623-630`).
    fn schedule_restart(&mut self, delay_ms: u64) {
        if self.stopped {
            return;
        }
        // `if (restartTimer) clearTimeout(restartTimer)`: the re-arm below
        // moves the generation, which is the same thing.
        self.arm(ObdTimer::Restart, delay_ms);
    }

    // ----- inputs -----

    /// `tick()` (`:633-637`): the lazy start on the first read, then the 1 s
    /// settings poll on every `read()` / `sample()`.
    pub fn tick(&mut self, now: ObdNow, read: ReadSettings<'_>) -> Vec<ObdAction> {
        let first = !self.started;
        self.started = true;
        self.apply_settings(first, now, read);
        self.take()
    }

    /// `reapply()` (`:735-738`): after a cloud config message; a no-op before
    /// the first read.
    pub fn reapply(&mut self, now: ObdNow, read: ReadSettings<'_>) -> Vec<ObdAction> {
        if self.started {
            self.apply_settings(true, now, read);
        }
        self.take()
    }

    /// `onMessage(msg)` (`:435-520`): one parsed helper line.
    pub fn on_message(&mut self, now: ObdNow, msg: ObdMessage) -> Vec<ObdAction> {
        match msg {
            ObdMessage::Ready {
                helper,
                version,
                allowlist_sha256,
            } => {
                let line = format!(
                    "[obd] helper ready ({} {}, allowlist {})",
                    helper.as_deref().unwrap_or("obd_read"),
                    version.as_deref().unwrap_or("?"),
                    slice16(allowlist_sha256.as_deref().unwrap_or("?"), 12)
                );
                if version.is_some() {
                    self.helper_version = version;
                }
                if allowlist_sha256.is_some() {
                    self.allowlist_sha = allowlist_sha256;
                }
                self.log(ObdLevel::Info, line);
            }
            ObdMessage::Status {
                state,
                link,
                detail,
                ..
            } => self.on_status(now, &state, link, detail),
            ObdMessage::Profile(rest) => {
                let p = &rest;
                let present = |k: &str| p.get(k).filter(|v| !v.is_null());
                let dp = present("protocol")
                    .and_then(|pr| pr.get("dp"))
                    .filter(|v| !v.is_null())
                    .map_or_else(|| "?".to_string(), js_string);
                let responders = present("responders")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|v| {
                                if v.is_null() {
                                    String::new()
                                } else {
                                    js_string(v)
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                let supported = present("supported_fields")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let powertrain = present("powertrain").map_or_else(|| "?".to_string(), js_string);
                let line = format!(
                    "[obd] profile: {dp} responders={} supported={supported} powertrain={powertrain}",
                    if responders.is_empty() {
                        "none"
                    } else {
                        &responders
                    }
                );
                self.profile = Some(rest);
                self.log(ObdLevel::Info, line);
            }
            ObdMessage::Sample { mut fields, .. } => {
                let sample_link = fields.shift_remove("link");
                self.sample = Some(Sample {
                    fields,
                    at_mono: now.mono_ms,
                });
                self.stamp += 1;
                if let Some(l) = sample_link.and_then(crate::obd::obd_link) {
                    self.set_link(l);
                }
            }
            ObdMessage::Audit {
                counters,
                inventory,
                last_stop,
                ts,
            } => {
                self.prev_audit = self.audit.take();
                let audit = AuditSnapshot {
                    counters,
                    inventory: bound_inventory(&inventory, MAX_INVENTORY_KEYS),
                    last_stop,
                    ts: ts.unwrap_or(now.wall_ms / 1000.0),
                };
                self.audit_seq += 1;
                let rate = request_rate(self.prev_audit.as_ref(), Some(&audit));
                let line = format!(
                    "[obd] {}",
                    crate::obd::audit_line(self.audit_seq, &audit, rate)
                );
                self.audit = Some(audit);
                self.log(ObdLevel::Info, line);
            }
            ObdMessage::Error { code, error } => {
                let e = format!("exit {}: {error}", number_to_string(code));
                self.log(ObdLevel::Error, format!("[obd] helper error {e}"));
                self.last_error = Some(e);
            }
        }
        self.take()
    }

    /// The `status` arm of `onMessage` (`:446-482`).
    fn on_status(&mut self, now: ObdNow, state: &str, link: u8, detail: Option<String>) {
        self.set_link(link);
        let changed = self.set_state(state, detail.clone(), now);
        if !changed {
            return;
        }
        let d = match detail.as_deref() {
            Some(d) if !d.is_empty() => format!(" ({d})"),
            _ => String::new(),
        };
        if state == "adapter_not_found" {
            let t = now.mono_ms;
            match &mut self.absent {
                None => {
                    self.absent = Some(Absent {
                        since_mono: t,
                        last_warn_mono: t,
                    });
                    self.event(ObdLevel::Warn, &format!("adapter not found{d}"));
                }
                Some(a) if t.saturating_sub(a.last_warn_mono) >= ADAPTER_ABSENT_REWARN_MS => {
                    a.last_warn_mono = t;
                    let mins = js_math_round((t - a.since_mono) as f64 / 60_000.0);
                    self.event(
                        ObdLevel::Warn,
                        &format!(
                            "adapter still not found after {} min{d}",
                            number_to_string(mins)
                        ),
                    );
                }
                Some(_) => self.log(ObdLevel::Info, format!("[obd] adapter_not_found{d}")),
            }
            return;
        }
        if let Some(a) = self.absent {
            if matches!(state, "pairing" | "probing" | "live" | "ecu_silent") {
                let mins =
                    js_math_round(now.mono_ms.saturating_sub(a.since_mono) as f64 / 60_000.0);
                self.absent = None;
                self.event(
                    ObdLevel::Info,
                    &format!("adapter found again after {} min", number_to_string(mins)),
                );
            }
        }
        match state {
            "live" => self.event(ObdLevel::Info, &format!("live, ECU answering{d}")),
            "ecu_silent" => self.event(ObdLevel::Info, &format!("ECU silent, polling stopped{d}")),
            "stopped" => self.event(ObdLevel::Warn, &format!("helper stopped{d}")),
            _ => self.log(ObdLevel::Info, format!("[obd] {state}{d}")),
        }
    }

    /// The helper's stderr (`onStderr`, `:530-533`): a trimmed, non-empty line.
    pub fn on_stderr(&mut self, line: &str) -> Vec<ObdAction> {
        self.log(ObdLevel::Error, format!("[obd] python: {line}"));
        if self.last_error.is_none() {
            self.last_error = Some(line.to_string());
        }
        self.take()
    }

    /// `h.done.then(res => { if (handle !== h) return; onExit(res.exitCode) })`
    /// (`:541-545`, `onExit` `:557-621`). `code` = `exitCode` (`None` when
    /// killed by a signal).
    pub fn on_exit(&mut self, now: ObdNow, handle: u64, code: Option<i64>) -> Vec<ObdAction> {
        if self.handle != Some(handle) {
            return self.take(); // a stale exit from a handle we already replaced
        }
        self.disarm(ObdTimer::Stable);
        self.handle = None;
        self.sample = None; // whatever it last said, nothing is polling now
        if self.stopped {
            self.set_state("stopped", None, now);
            self.set_link(0);
            return self.take();
        }
        if !self.resolved.run {
            let off = self.resolved.off.unwrap_or(ObdOff::NotConfigured);
            self.set_state(off.as_str(), None, now);
            self.set_link(0);
            return self.take();
        }
        if self.restart_pending {
            self.restart_pending = false;
            self.quick_failures = 0;
            self.backoff_ms = BASE_BACKOFF_MS;
            self.spawn(now);
            return self.take();
        }
        let ran_ms = now.mono_ms.saturating_sub(self.started_at_mono);
        let code_s = code.map_or_else(|| "?".to_string(), |c| c.to_string());
        let err = self
            .last_error
            .as_ref()
            .map_or_else(String::new, |e| format!(" — {e}"));

        if code == Some(obd_exit::SOFTWARE_BUG) {
            let why = format!("helper reported a software bug (exit 6){err}");
            self.park(PARKED_RETRY_MS, &why, ObdLevel::Error, now);
            return self.take();
        }
        if code == Some(obd_exit::PROTOCOL_REFUSED) {
            let why = format!("vehicle protocol refused (exit 4, not ISO 15765-4 CAN){err}");
            self.park(PROTOCOL_PARK_MS, &why, ObdLevel::Error, now);
            return self.take();
        }
        if ran_ms >= STABLE_MS {
            self.quick_failures = 0;
            self.backoff_ms = BASE_BACKOFF_MS;
            self.set_state("retrying", None, now);
            self.set_link(0);
            let msg = format!(
                "helper exited (code {code_s}) after {} s{err}; restarting in {} s",
                number_to_string(js_math_round(ran_ms as f64 / 1000.0)),
                BASE_BACKOFF_MS / 1000
            );
            self.event(ObdLevel::Warn, &msg);
            self.schedule_restart(BASE_BACKOFF_MS);
            return self.take();
        }
        self.quick_failures += 1;
        if self.quick_failures >= MAX_QUICK_FAILURES {
            let why = format!(
                "helper exited (code {code_s}) before stabilizing {}x{err}",
                self.quick_failures
            );
            self.park(PARKED_RETRY_MS, &why, ObdLevel::Error, now);
            return self.take();
        }
        self.set_state("retrying", None, now);
        self.set_link(0);
        self.log(
            ObdLevel::Warn,
            format!(
                "[obd] helper exited (code {code_s}){err}; retry {}/{MAX_QUICK_FAILURES} in {} ms",
                self.quick_failures, self.backoff_ms
            ),
        );
        self.schedule_restart(self.backoff_ms);
        self.backoff_ms = (self.backoff_ms * 2).min(MAX_BACKOFF_MS);
        self.take()
    }

    /// A timer the runtime armed fired. A generation that moved on (the TS
    /// `clearTimeout` won the race) is ignored.
    pub fn timer_fired(&mut self, now: ObdNow, timer: ObdTimer, gen: u64) -> Vec<ObdAction> {
        let i = timer_idx(timer);
        if !self.timer_armed[i] || self.timer_gen[i] != gen {
            return self.take();
        }
        self.timer_armed[i] = false;
        match timer {
            ObdTimer::Stable => {
                if !self.stopped && self.handle.is_some() {
                    self.quick_failures = 0;
                    self.backoff_ms = BASE_BACKOFF_MS;
                }
            }
            ObdTimer::Restart => self.spawn(now),
        }
        self.take()
    }

    /// `scan()` up to the scanner call (`:739-743`): `None` = join the scan
    /// already in flight; `Some(actions)` = a new scan started (the
    /// "scanning …" event emitted) — run the scanner for
    /// [`OBD_SCAN_SECONDS`], then [`Self::scan_end`].
    pub fn scan_begin(&mut self) -> Option<Vec<ObdAction>> {
        if self.scan_in_flight {
            return None;
        }
        self.scan_in_flight = true;
        self.event(
            ObdLevel::Info,
            &format!("scanning for Bluetooth adapters ({OBD_SCAN_SECONDS} s, never pairs)"),
        );
        Some(self.take())
    }

    /// The rest of `scan()` (`:744-755`): the result lands in `report().scan`.
    pub fn scan_end(
        &mut self,
        now: ObdNow,
        result: Result<Vec<ObdScanCandidate>, String>,
    ) -> (ObdScanResult, Vec<ObdAction>) {
        let scan = match result {
            Ok(candidates) => {
                let n = candidates.len();
                let s = ObdScanResult {
                    ts: now.wall_ms,
                    candidates,
                    error: None,
                };
                self.scan = Some(s.clone());
                self.event(ObdLevel::Info, &format!("scan done: {n} named device(s)"));
                s
            }
            Err(msg) => {
                let s = ObdScanResult {
                    ts: now.wall_ms,
                    candidates: vec![],
                    error: Some(msg.clone()),
                };
                self.scan = Some(s.clone());
                self.event(ObdLevel::Warn, &format!("scan failed: {msg}"));
                s
            }
        };
        self.scan_in_flight = false;
        (scan, self.take())
    }

    /// `forget()` up to the unbond (`:758-769`): latch the MAC, drop the
    /// profile and the last stop, go off. Returns the forgotten MAC (`None` =
    /// no adapter was configured) — the runtime then unbonds it and reports a
    /// failure through [`Self::forget_unbond_failed`].
    pub fn forget(&mut self, now: ObdNow) -> (Option<String>, Vec<ObdAction>) {
        let mac = self.resolved.helper.adapter.clone();
        self.forgotten_mac = mac.clone();
        self.profile = None;
        if let Some(a) = &mut self.audit {
            a.last_stop = None;
        }
        self.resolved.run = false;
        self.resolved.off = Some(ObdOff::NotConfigured);
        self.resolved_key = Some((
            false,
            Some(ObdOff::NotConfigured),
            self.resolved.helper.clone(),
        ));
        let why = match &mac {
            Some(m) => format!(
                "adapter {m} forgotten; helper stopped, bond removed, waiting for new obd settings"
            ),
            None => "forget: no adapter was configured".to_string(),
        };
        self.go_off(ObdOff::NotConfigured, &why, ObdLevel::Info, now);
        (mac, self.take())
    }

    /// `catch (err) { event("warn", …) }` around `unbond(mac)` (`:771-775`).
    pub fn forget_unbond_failed(&mut self, mac: &str, err: &str) -> Vec<ObdAction> {
        self.event(
            ObdLevel::Warn,
            &format!("forget: bond removal failed for {mac}: {err}"),
        );
        self.take()
    }

    /// `stop()` (`:779-786`): SIGINT the helper, cancel restarts, link 0.
    pub fn stop(&mut self, now: ObdNow) -> Vec<ObdAction> {
        if self.stopped {
            return self.take();
        }
        self.stopped = true;
        self.clear_timers();
        self.set_state("stopped", None, now);
        self.set_link(0);
        self.sigint();
        self.take()
    }

    // ----- outputs -----

    /// The helper currently supervised.
    pub fn handle(&self) -> Option<u64> {
        self.handle
    }

    /// `changedMono`: advances on every sample and every link change.
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    fn current_link(&self) -> u8 {
        if self.handle.is_none() {
            0
        } else {
            self.link
        }
    }

    /// `mapLatest()` (`:649-659`): `link` always; the other leaves only from
    /// a sample < 5 s old while a helper runs, finite numbers only, with units.
    pub fn map_latest(&self, now: ObdNow) -> MetricGroup {
        let mut out = MetricGroup::new();
        out.insert(
            "link".into(),
            MetricLeaf::new(f64::from(self.current_link())),
        );
        let Some(s) = &self.sample else { return out };
        if self.handle.is_none() || now.mono_ms.saturating_sub(s.at_mono) >= SAMPLE_FRESH_MS {
            return out;
        }
        for (k, v) in &s.fields {
            if k == "link" || !v.is_finite() {
                continue;
            }
            let leaf = match obd_field_unit(k) {
                Some(u) => MetricLeaf::with_unit(*v, u),
                None => MetricLeaf::new(*v),
            };
            out.insert(k.clone(), leaf);
        }
        out
    }

    fn voltage(&self) -> Option<f64> {
        let f = &self.sample.as_ref()?.fields;
        f.get("module_voltage")
            .or_else(|| f.get("adapter_voltage"))
            .copied()
    }

    fn req_per_sec(&self) -> Option<f64> {
        self.handle?;
        if let Some(r) = request_rate(self.prev_audit.as_ref(), self.audit.as_ref()) {
            return Some(r);
        }
        let seed = self.profile.as_ref()?.get("rate_req_s")?.as_f64()?;
        (seed.is_finite() && seed >= 0.0).then(|| js_math_round(seed * 10.0) / 10.0)
    }

    /// `status()` (`:691-710`).
    pub fn status(&self) -> ObdStatus {
        let audit = self.audit.as_ref();
        let c = |k: &str| {
            audit
                .and_then(|a| a.counters.get(k).copied())
                .unwrap_or(0.0)
        };
        ObdStatus {
            state: self.state.clone(),
            link: self.current_link(),
            req_per_sec: self.req_per_sec(),
            errors: audit.map_or(0.0, AuditSnapshot::errors) as u64,
            forbidden: audit.map_or(0.0, AuditSnapshot::forbidden) as u64,
            voltage: self.voltage(),
            detail: self.detail.clone(),
            since: self.since,
            helper_version: self.helper_version.clone(),
            adapter: Some(
                self.resolved
                    .helper
                    .serial
                    .clone()
                    .or_else(|| self.resolved.helper.adapter.clone()),
            ),
            profile: Some(profile_summary(self.profile.as_ref())),
            voltage_low_events: Some(c("voltage_low_events") as u64),
            audit_seq: Some(self.audit_seq),
            last_stop: Some(audit.and_then(|a| a.last_stop.as_ref()?.reason.clone())),
        }
    }

    /// `report()` (`:712-734`) = `effective.obd`, in the TS key order
    /// (the six namespace keys, status, link, source, adapter, profile?,
    /// audit?, scan?, helper_version?, allowlist_sha256?).
    pub fn report(&self) -> Value {
        let mut m = match serde_json::to_value(&self.resolved.settings) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        m.insert("status".into(), Value::String(self.state.clone()));
        m.insert("link".into(), Value::from(self.current_link()));
        m.insert(
            "source".into(),
            Value::String(self.resolved.source.as_str().into()),
        );
        let h = &self.resolved.helper;
        let mut adapter = Map::new();
        adapter.insert(
            "mac".into(),
            h.adapter.clone().map_or(Value::Null, Value::String),
        );
        if let Some(s) = h.serial.as_ref().filter(|s| !s.is_empty()) {
            adapter.insert("serial".into(), Value::String(s.clone()));
        }
        m.insert("adapter".into(), Value::Object(adapter));
        if let Some(p) = &self.profile {
            m.insert("profile".into(), Value::Object(p.clone()));
        }
        if let Some(a) = &self.audit {
            let mut am = Map::new();
            am.insert("counters".into(), num_map(&a.counters));
            am.insert("inventory".into(), num_map(&a.inventory));
            am.insert(
                "last_stop".into(),
                a.last_stop.as_ref().map_or(Value::Null, LastStop::to_json),
            );
            m.insert("audit".into(), Value::Object(am));
        }
        if let Some(s) = &self.scan {
            m.insert(
                "scan".into(),
                serde_json::to_value(s).unwrap_or(Value::Null),
            );
        }
        if let Some(v) = self.helper_version.as_ref().filter(|v| !v.is_empty()) {
            m.insert("helper_version".into(), Value::String(v.clone()));
        }
        if let Some(v) = self.allowlist_sha.as_ref().filter(|v| !v.is_empty()) {
            m.insert("allowlist_sha256".into(), Value::String(v.clone()));
        }
        Value::Object(m)
    }

    /// The resolved config (tests / the CLI).
    pub fn resolved(&self) -> &ObdResolvedConfig {
        &self.resolved
    }

    /// The source of the six namespace fields.
    pub fn source(&self) -> ObdSource {
        self.resolved.source
    }
}

fn num_map(m: &IndexMap<String, f64>) -> Value {
    Value::Object(
        object_key_order(m.clone())
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number),
                )
            })
            .collect(),
    )
}

/// `${v}` of a JSON value (profile log line).
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        Value::Array(a) => a
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}
