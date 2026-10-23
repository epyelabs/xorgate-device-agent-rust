//! The OBD collector — `createObdCollector` (`src/agent/collectors/obd.ts`
//! @ 09da38d) as a runtime over the sans-I/O [`ObdSupervisor`]
//! (`xorgate_core::obd_supervisor`, traced against the TS) and the edges in
//! [`xorgate_hal::obd_io`].
//!
//! - Exists only when the BOOT-time hardware block declares
//!   `peripherals.obd` (`collectors/index.ts` collector-creation seam; the
//!   registry gates it, [`super::Collectors::new`]). A live block change only
//!   logs the restart warning (the config plane's hunk).
//! - `read()` / `sample()` run the supervisor's `tick` (lazy start on the
//!   first read, then the 1 s settings poll over `settings.json` `obd`, the
//!   `XORGATE_OBD_*` env seam as the fallback); `reapply()` forces a re-read
//!   after every cloud config message.
//! - Every [`ObdAction`] is carried out here: the helper spawn
//!   (`python3 <XORGATE_PYTHON_DIR>/obd_read.py …`, the mock stream under
//!   `XORGATE_MOCK`), SIGINT, the two timers (re-armed by generation; a
//!   stale fire is ignored by the supervisor), console lines (`[obd] …`) and
//!   activity events (`store.event` + forced flush, the worker's `onEvent`).
//! - `scan()` emits its start event synchronously (before the command ack)
//!   and joins a scan in flight; `forget()` goes off, then unbonds.
//! - `stop()` SIGINTs the helper (it ATZs and closes).
//!
//! Lock discipline: the supervisor and the helper handles live under one
//! `std::sync::Mutex`, never held across an await; the actions an input
//! returns are carried out under it (they only spawn tasks, signal, log and
//! write state.json events).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::{error, info, warn};
use xorgate_core::clock::Clock;
use xorgate_core::obd::{ObdEnv, ObdScanResult, ObdSettings, OBD_SCAN_SECONDS};
use xorgate_core::obd_supervisor::{ObdAction, ObdLevel, ObdNow, ObdSupervisor, ObdTimer};
use xorgate_core::telemetry::{ActivityLevel, MetricGroup, ObdStatus};
use xorgate_hal::obd_io::{ObdHelperEvent, ObdHelperHandle, ObdIo, RealObdIo};
use xorgate_hal::settings_io::load_settings;

use super::Collector;
use crate::ctx::{AgentCtx, BoxFut, GroupSample, ObdControl};

/// How long the agent waits for the OBD helper to exit at agent exit, after
/// the SIGTERM (review-wp2 M2): above the helper's own close budget (ATZ with
/// a 2 s timeout, then the `close` audit record), bounded so a wedged helper
/// cannot hold the agent's exit past systemd's stop timeout.
pub const HELPER_EXIT_BUDGET: Duration = Duration::from_millis(3_000);

/// Where the `obd` namespace comes from (settings.json in production; tests
/// inject one).
pub type ObdSettingsSource = Arc<dyn Fn() -> Option<ObdSettings> + Send + Sync>;

/// `createObdCollector({ onEvent })`.
pub struct ObdCollector {
    sh: Arc<Shared>,
}

struct Shared {
    ctx: AgentCtx,
    io: Arc<dyn ObdIo>,
    settings: ObdSettingsSource,
    st: Mutex<Inner>,
    /// Woken on every helper exit ([`ObdCollector::shutdown`] waits on it).
    exited: tokio::sync::Notify,
}

struct Inner {
    sup: ObdSupervisor,
    handles: HashMap<u64, ObdHelperHandle>,
    timers: [Option<AbortHandle>; 2],
    /// `changedMono` as a tokio instant: re-stamped whenever the
    /// supervisor's stamp moves (`GroupSample::updated_mono`).
    stamp: (u64, Instant),
    /// Callers joined to the scan in flight.
    scan_waiters: Vec<oneshot::Sender<ObdScanResult>>,
}

fn timer_slot(t: ObdTimer) -> usize {
    match t {
        ObdTimer::Stable => 0,
        ObdTimer::Restart => 1,
    }
}

impl ObdCollector {
    /// Production: the vendored helpers under `XORGATE_PYTHON_DIR`, the mock
    /// edges under `XORGATE_MOCK`, settings.json's `obd`, the process env.
    pub fn new(ctx: &AgentCtx) -> Self {
        let path = ctx.paths.settings.clone();
        Self::with(
            ctx,
            Arc::new(RealObdIo::from_env(ctx.paths.python_dir.clone())),
            Arc::new(move || load_settings(&path).obd),
            ObdEnv::from_lookup(|k| std::env::var(k).ok()),
        )
    }

    /// Injectable edges (tests).
    pub fn with(
        ctx: &AgentCtx,
        io: Arc<dyn ObdIo>,
        settings: ObdSettingsSource,
        env: ObdEnv,
    ) -> Self {
        let now = now_of(ctx);
        Self {
            sh: Arc::new(Shared {
                ctx: ctx.clone(),
                io,
                settings,
                st: Mutex::new(Inner {
                    sup: ObdSupervisor::new(env, now),
                    handles: HashMap::new(),
                    timers: [None, None],
                    stamp: (0, Instant::now()),
                    scan_waiters: Vec::new(),
                }),
                exited: tokio::sync::Notify::new(),
            }),
        }
    }

    /// `stop()`: SIGINT the helper, cancel restarts, link 0.
    pub fn stop(&self) {
        let sh = &self.sh;
        let mut st = sh.lock();
        let a = st.sup.stop(now_of(&sh.ctx));
        sh.apply(&mut st, a);
    }

    /// The agent-exit half of the helper's shutdown (review-wp2 M2). The TS
    /// never SIGKILLs it: `stop()` SIGINTs, then node's exit runs execa's
    /// `cleanup` = SIGTERM to every child still running, and the helper
    /// finishes its close (ATZ with a 2 s timeout, the `close` audit record)
    /// on its own. Here: SIGTERM every helper still alive, then wait for its
    /// exit for at most `budget` (≥ the helper's 2 s ATZ budget) BEFORE the
    /// runtime drop could SIGKILL it (`kill_on_drop`). Logs nothing, as the TS.
    pub async fn shutdown(&self, budget: Duration) {
        let sh = &self.sh;
        let deadline = Instant::now() + budget;
        {
            let mut st = sh.lock();
            for h in st.handles.values_mut() {
                h.sigterm();
            }
        }
        loop {
            let notified = sh.exited.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if sh.lock().handles.is_empty() {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    /// The running helper's pid (tests / diagnostics).
    pub fn helper_pid(&self) -> Option<u32> {
        let st = self.sh.lock();
        let h = st.sup.handle()?;
        st.handles.get(&h).and_then(ObdHelperHandle::pid)
    }

    fn tick(&self) -> MetricGroup {
        let sh = &self.sh;
        let now = now_of(&sh.ctx);
        let mut st = sh.lock();
        let read = sh.settings.clone();
        let mut f = || Ok(read());
        let a = st.sup.tick(now, &mut f);
        sh.apply(&mut st, a);
        st.sup.map_latest(now)
    }
}

fn now_of(ctx: &AgentCtx) -> ObdNow {
    ObdNow {
        wall_ms: ctx.clock.wall_ms() as f64,
        mono_ms: ctx.clock.mono_ms(),
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Carry out the supervisor's effects, in order.
    fn apply(self: &Arc<Self>, st: &mut Inner, actions: Vec<ObdAction>) {
        for a in actions {
            match a {
                ObdAction::Spawn { handle, cfg } => {
                    let (h, mut rx) = self.io.spawn(&cfg);
                    st.handles.insert(handle, h);
                    let me = self.clone();
                    self.ctx.spawn("obd-helper", async move {
                        while let Some(ev) = rx.recv().await {
                            let exit = matches!(ev, ObdHelperEvent::Exit(_));
                            me.on_helper_event(handle, ev);
                            if exit {
                                break;
                            }
                        }
                    });
                }
                ObdAction::Sigint { handle } => {
                    if let Some(h) = st.handles.get_mut(&handle) {
                        h.sigint();
                    }
                }
                ObdAction::Arm {
                    timer,
                    gen,
                    delay_ms,
                } => {
                    let slot = timer_slot(timer);
                    if let Some(old) = st.timers[slot].take() {
                        old.abort();
                    }
                    let me = self.clone();
                    let task = tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        let mut st = me.lock();
                        let a = st.sup.timer_fired(now_of(&me.ctx), timer, gen);
                        me.apply(&mut st, a);
                    });
                    st.timers[slot] = Some(task.abort_handle());
                }
                ObdAction::Disarm { timer } => {
                    if let Some(old) = st.timers[timer_slot(timer)].take() {
                        old.abort();
                    }
                }
                ObdAction::Log { level, line } => match level {
                    ObdLevel::Info => info!("{line}"),
                    ObdLevel::Warn => warn!("{line}"),
                    ObdLevel::Error => error!("{line}"),
                },
                ObdAction::Event { level, msg } => {
                    // The worker's `onEvent`: `store.event(level, msg, t)` + forced flush.
                    let t = self.ctx.now_ms();
                    let level = match level {
                        ObdLevel::Info => ActivityLevel::Info,
                        ObdLevel::Warn => ActivityLevel::Warn,
                        ObdLevel::Error => ActivityLevel::Error,
                    };
                    self.ctx.state.event(level, msg, t);
                    self.ctx.state.flush(t, true);
                }
            }
        }
        let s = st.sup.stamp();
        if s != st.stamp.0 {
            st.stamp = (s, Instant::now());
        }
    }

    fn on_helper_event(self: &Arc<Self>, handle: u64, ev: ObdHelperEvent) {
        let now = now_of(&self.ctx);
        let mut st = self.lock();
        let a = match ev {
            ObdHelperEvent::Message(m) => st.sup.on_message(now, m),
            ObdHelperEvent::Stderr(line) => st.sup.on_stderr(&line),
            ObdHelperEvent::Exit(code) => {
                st.handles.remove(&handle);
                st.sup.on_exit(now, handle, code)
            }
        };
        self.apply(&mut st, a);
        drop(st);
        self.exited.notify_waiters();
    }
}

impl Collector for ObdCollector {
    fn group(&self) -> &'static str {
        "obd"
    }

    /// `read()`: tick, then `mapLatest()` (`link` always).
    fn read(&self) -> BoxFut<'_, MetricGroup> {
        Box::pin(async move { self.tick() })
    }

    /// `sample()`: the same mapping + the stamp that moves on every sample
    /// or link change.
    fn sample(&self) -> BoxFut<'_, Option<GroupSample>> {
        Box::pin(async move {
            let fields = self.tick();
            let updated_mono = self.sh.lock().stamp.1;
            Some(GroupSample {
                fields,
                updated_mono,
            })
        })
    }
}

impl ObdControl for ObdCollector {
    fn status(&self) -> ObdStatus {
        self.sh.lock().sup.status()
    }

    fn report(&self) -> serde_json::Value {
        self.sh.lock().sup.report()
    }

    fn reapply(&self) {
        let sh = &self.sh;
        let now = now_of(&sh.ctx);
        let mut st = sh.lock();
        let read = sh.settings.clone();
        let mut f = || Ok(read());
        let a = st.sup.reapply(now, &mut f);
        sh.apply(&mut st, a);
    }

    fn scan(&self) -> BoxFut<'static, ObdScanResult> {
        let sh = self.sh.clone();
        let (tx, rx) = oneshot::channel();
        let begun = {
            let mut st = sh.lock();
            st.scan_waiters.push(tx);
            match st.sup.scan_begin() {
                // A new scan: the "scanning …" event goes out NOW (sync, before the ack).
                Some(a) => {
                    sh.apply(&mut st, a);
                    true
                }
                None => false, // joined the scan in flight
            }
        };
        if begun {
            let me = sh.clone();
            sh.ctx.spawn("obd-scan", async move {
                let res = me.io.scan(f64::from(OBD_SCAN_SECONDS)).await;
                let mut st = me.lock();
                let (result, a) = st.sup.scan_end(now_of(&me.ctx), res);
                me.apply(&mut st, a);
                for w in st.scan_waiters.drain(..) {
                    let _ = w.send(result.clone());
                }
            });
        }
        Box::pin(async move {
            rx.await.unwrap_or_else(|_| ObdScanResult {
                ts: sh.ctx.now_ms() as f64,
                candidates: vec![],
                error: Some("scan cancelled".into()),
            })
        })
    }

    fn forget(&self) -> BoxFut<'static, Option<String>> {
        let sh = self.sh.clone();
        let mac = {
            let mut st = sh.lock();
            let (mac, a) = st.sup.forget(now_of(&sh.ctx));
            sh.apply(&mut st, a);
            mac
        };
        // `await unbond(mac)` is reached synchronously in the TS
        // (`collectors/obd.ts:770-772`): `bluetoothctl remove` is spawned
        // HERE, before the command's accepted ack (review-wp2 S2).
        let unbond = mac.clone().map(|m| sh.io.unbond(m));
        Box::pin(async move {
            if let (Some(m), Some(unbond)) = (&mac, unbond) {
                if let Err(e) = unbond.await {
                    let mut st = sh.lock();
                    let a = st.sup.forget_unbond_failed(m, &e);
                    sh.apply(&mut st, a);
                }
            }
            mac
        })
    }
}
