//! The gps collector — `src/agent/collectors/gps.ts:108-338`
//! (`createGpsCollector`) over `src/hardware/gps.ts:46-80` (`streamGps`).
//!
//! The NMEA port (`GPS_NMEA_PORT`, default `/dev/ttyUSB1`) is opened lazily on
//! the first `read`/`sample`/`clock`/`sky` call and kept open; a per-stream
//! task feeds every chunk through a FRESH `FixAggregator` (one per
//! `streamGps` call, so a reopen starts a new aggregate) and a generation
//! guard drops the callbacks of a stream that has been replaced. The decisions
//! (freshness, watchdog, TTFF, sky cadence, `mapLatest`) are pure functions in
//! [`xorgate_core::gps_group`].
//!
//! TIME (O2): `lastFixAt`, `openedAt`, `acquiringSince` and the sky-file
//! cadence are MONOTONIC ms (`ctx.clock.mono_ms()`); only the sky file's
//! `timestamp` is wall time (`ctx.now_ms()`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::warn;
use xorgate_core::clock::{Clock, MonoMs};
use xorgate_core::gps_group::{
    clock_reading, describe_error, has_fix, is_fresh, map_latest, reopen_due, sky_publish_due,
    TtffClock, NMEA_SILENT_REOPEN, REOPEN_SILENT_MS,
};
use xorgate_core::nmea::{has_position, parse_gsv, FixAggregator, GpsFix, GsvMessage};
use xorgate_core::sky_state::{GnssSkyFile, SkySnapshot, SkyTracker};
use xorgate_core::telemetry::MetricGroup;
use xorgate_core::types::{GpsClockReading, SkyState};
use xorgate_hal::atomic_write::atomic_write_json;
use xorgate_hal::nmea_tty::{NmeaOpener, NmeaStream, TtyEvent};

use super::{to_group, Collector};
use crate::ctx::{AgentCtx, BoxFut, GroupSample};

/// Options (`GpsCollectorOptions`, `gps.ts:102-106`).
#[derive(Debug, Clone)]
pub struct GpsOptions {
    /// `reopenSilentMs` (default [`REOPEN_SILENT_MS`]).
    pub reopen_silent_ms: u64,
    /// `skyStatePath` (default `<runtime home>/gnss-sky.json`).
    pub sky_state_path: PathBuf,
    /// The NMEA port (`GPS_NMEA_PORT`).
    pub port: PathBuf,
}

impl GpsOptions {
    /// The defaults the worker uses.
    pub fn from_ctx(ctx: &AgentCtx) -> Self {
        Self {
            reopen_silent_ms: REOPEN_SILENT_MS,
            sky_state_path: ctx.paths.gnss_sky.clone(),
            port: ctx.paths.gps_nmea_port.clone(),
        }
    }
}

/// `createGpsCollector()`.
pub struct GpsCollector {
    shared: Arc<Shared>,
}

struct Shared {
    ctx: AgentCtx,
    opener: Arc<dyn NmeaOpener>,
    opts: GpsOptions,
    st: Mutex<State>,
}

/// The TS closure's `let`s (`gps.ts:115-137`).
struct State {
    sky: SkyTracker,
    /// `skyPublishedState` / `skyPublishedAt` (`None` = `""`, never written).
    sky_published: Option<(SkyState, MonoMs)>,
    started: bool,
    latest: Option<GpsFix>,
    /// `lastFixAt` (mono, O2); `None` = the TS initial `0`.
    last_fix_at: Option<MonoMs>,
    /// `lastFixMono` — the recorder's freshness stamp.
    last_fix_mono: Instant,
    clock_utc_ms: Option<f64>,
    clock_mono: MonoMs,
    clock_valid: bool,
    warned: bool,
    handle: Option<NmeaStream>,
    /// `openedAt` (mono, O2).
    opened_at: MonoMs,
    gen: u64,
    ttff: TtffClock,
}

impl GpsCollector {
    /// `createGpsCollector()` with the worker's defaults.
    pub fn new(ctx: &AgentCtx, opener: Arc<dyn NmeaOpener>) -> Self {
        Self::with_options(ctx, opener, GpsOptions::from_ctx(ctx))
    }

    /// `createGpsCollector(opts)`.
    pub fn with_options(ctx: &AgentCtx, opener: Arc<dyn NmeaOpener>, opts: GpsOptions) -> Self {
        let now = ctx.clock.mono_ms();
        Self {
            shared: Arc::new(Shared {
                ctx: ctx.clone(),
                opener,
                opts,
                st: Mutex::new(State {
                    sky: SkyTracker::with_defaults(now),
                    sky_published: None,
                    started: false,
                    latest: None,
                    last_fix_at: None,
                    last_fix_mono: Instant::now(),
                    clock_utc_ms: None,
                    clock_mono: 0,
                    clock_valid: false,
                    warned: false,
                    handle: None,
                    opened_at: 0,
                    gen: 0,
                    ttff: TtffClock::default(),
                }),
            }),
        }
    }

    /// `clock()` (`gps.ts:319-325`): latest fresh RMC UTC time; also keeps
    /// the stream open.
    pub fn clock(&self) -> Option<GpsClockReading> {
        let sh = &self.shared;
        let mut st = sh.lock();
        sh.ensure_streaming(&mut st);
        let now = sh.mono();
        clock_reading(st.clock_utc_ms, st.clock_mono, st.clock_valid, now)
    }

    /// `sky()` (`gps.ts:326-329`): the sky snapshot; also keeps the stream open.
    pub fn sky(&self) -> SkySnapshot {
        let sh = &self.shared;
        let mut st = sh.lock();
        sh.ensure_streaming(&mut st);
        sh.sky_now(&mut st)
    }

    /// `hasFix()` (`gps.ts:330-336`). Does NOT open the stream.
    pub fn has_fix(&self) -> bool {
        let sh = &self.shared;
        let st = sh.lock();
        has_fix(sh.fresh_fix(&st).as_ref())
    }

    /// Close the port (process exit in the TS).
    pub fn stop(&self) {
        let mut st = self.shared.lock();
        if let Some(h) = st.handle.take() {
            h.stop();
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn mono(&self) -> MonoMs {
        self.ctx.clock.mono_ms()
    }

    /// `start()` (`gps.ts:153-207`).
    fn start(self: &Arc<Self>, st: &mut State) {
        st.started = true; // set before opening so reads don't double-start
        st.opened_at = self.mono();
        // Fresh stream = fresh acquisition observation; never bill the silent gap.
        st.ttff.stream_opened();
        st.gen += 1;
        let my_gen = st.gen;
        let (handle, rx) = self.opener.open(&self.opts.port);
        st.handle = Some(handle);
        let me = self.clone();
        self.ctx.spawn(
            "gps-nmea-stream",
            async move { me.run_stream(my_gen, rx).await },
        );
    }

    /// `ensureStreaming()` (`gps.ts:212-224`): lazy open + the silent-fd watchdog.
    fn ensure_streaming(self: &Arc<Self>, st: &mut State) {
        if !st.started {
            self.start(st);
        } else if reopen_due(
            st.last_fix_at,
            st.opened_at,
            self.mono(),
            self.opts.reopen_silent_ms,
        ) {
            // Silent-fd watchdog: destroy it and reopen on the next tick.
            warn!("{NMEA_SILENT_REOPEN}");
            if let Some(h) = st.handle.take() {
                h.stop();
            }
            st.started = false;
        }
    }

    /// `skyNow()` (`gps.ts:232-259`): evaluate the tracker and keep the
    /// daemon-facing file fresh (written on a state change or every 15 s).
    fn sky_now(&self, st: &mut State) -> SkySnapshot {
        let now = self.mono();
        let snap = st.sky.snapshot(now);
        if sky_publish_due(st.sky_published, snap.state, now) {
            st.sky_published = Some((snap.state, now));
            let path = &self.opts.sky_state_path;
            // Only if the runtime home already exists — a unit test or a CLI run
            // on a box that has never started the agent must not create it.
            if path.parent().is_some_and(|d| d.exists()) {
                let doc = GnssSkyFile::from_snapshot(&snap, self.ctx.now_ms());
                // the sky file is an optimization; never let it break the collector
                let _ = atomic_write_json(path, &doc);
            }
        }
        snap
    }

    /// `freshFix()` (`gps.ts:302-305`).
    fn fresh_fix(&self, st: &State) -> Option<GpsFix> {
        let fix = st.latest.as_ref()?;
        is_fresh(st.last_fix_at, self.mono()).then(|| fix.clone())
    }

    /// `mapLatest(fix)` (`gps.ts:261-300`), `skyNow()` included.
    fn map(&self, st: &mut State, fix: &GpsFix) -> MetricGroup {
        let sky = self.sky_now(st);
        to_group(&map_latest(fix, &sky, st.ttff.ttff_s()))
    }

    /// One `streamGps` stream (`hardware/gps.ts:46-80`): split chunks into
    /// lines, feed the aggregator and the GSV handler, then `onFix` once per
    /// chunk that changed the fix; `error` → `onError`, `close` → `onEnd`.
    async fn run_stream(self: Arc<Self>, my_gen: u64, mut rx: mpsc::UnboundedReceiver<TtyEvent>) {
        let mut agg = FixAggregator::new();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                TtyEvent::Data(bytes) => {
                    buf.extend_from_slice(&bytes);
                    let mut updated = false;
                    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                        let raw: Vec<u8> = buf.drain(..=pos).collect();
                        let line = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
                        self.ctx.tape.nmea(&line);
                        if agg.update(&line) {
                            updated = true;
                        }
                        if line.contains("GSV") {
                            if let Some(gsv) = parse_gsv(&line) {
                                self.on_gsv(my_gen, &gsv);
                            }
                        }
                    }
                    if updated {
                        self.on_fix(my_gen, agg.fix.clone());
                    }
                }
                TtyEvent::Error(e) => {
                    let port = self.opts.port.display().to_string();
                    self.on_error(
                        my_gen,
                        &describe_error(e.code.as_deref(), &e.message, &port),
                    );
                }
                TtyEvent::Close => {
                    self.on_end(my_gen);
                    break;
                }
            }
        }
    }

    /// `onFix` (`gps.ts:160-172`).
    fn on_fix(&self, my_gen: u64, fix: GpsFix) {
        let mut st = self.lock();
        if my_gen != st.gen {
            return;
        }
        let now = self.mono();
        st.last_fix_at = Some(now);
        st.last_fix_mono = Instant::now();
        if let Some(utc) = fix.utc_ms {
            if Some(utc) != st.clock_utc_ms {
                st.clock_utc_ms = Some(utc);
                st.clock_mono = now;
                st.clock_valid = fix.valid == Some(true);
            }
        }
        st.ttff.note_fix_quality(&fix, now);
        st.latest = Some(fix);
        st.warned = false; // streaming again — allow a fresh warning if it drops
    }

    /// `onGsv` (`gps.ts:173-192`): feed the sky tracker; a GSV also refreshes
    /// the freshness of a POSITION-LESS fix (never of a held position).
    fn on_gsv(&self, my_gen: u64, msg: &GsvMessage) {
        let mut st = self.lock();
        if my_gen != st.gen {
            return;
        }
        let now = self.mono();
        st.sky.note(msg, now);
        if st.latest.as_ref().is_some_and(|f| !has_position(f)) {
            st.last_fix_at = Some(now);
            st.last_fix_mono = Instant::now();
        }
    }

    /// `onError` (`gps.ts:193-199`): one warning until the stream recovers.
    fn on_error(&self, my_gen: u64, msg: &str) {
        let mut st = self.lock();
        if my_gen != st.gen {
            return;
        }
        if !st.warned {
            warn!("[gps] {msg}");
            st.warned = true;
        }
    }

    /// `onEnd` (`gps.ts:200-205`): allow a re-open next tick.
    fn on_end(&self, my_gen: u64) {
        let mut st = self.lock();
        if my_gen != st.gen {
            return;
        }
        st.started = false;
    }
}

impl Collector for GpsCollector {
    fn group(&self) -> &'static str {
        "gps"
    }

    /// `read()` (`gps.ts:309-313`).
    fn read(&self) -> BoxFut<'_, MetricGroup> {
        Box::pin(async move {
            let sh = &self.shared;
            let mut st = sh.lock();
            sh.ensure_streaming(&mut st);
            match sh.fresh_fix(&st) {
                Some(fix) => sh.map(&mut st, &fix),
                None => MetricGroup::new(),
            }
        })
    }

    /// `sample()` (`gps.ts:314-318`).
    fn sample(&self) -> BoxFut<'_, Option<GroupSample>> {
        Box::pin(async move {
            let sh = &self.shared;
            let mut st = sh.lock();
            sh.ensure_streaming(&mut st);
            let fix = sh.fresh_fix(&st)?;
            let fields = sh.map(&mut st, &fix);
            Some(GroupSample {
                fields,
                updated_mono: st.last_fix_mono,
            })
        })
    }
}
