//! `OpsHooks` (I10, Phase 4 WP6) — the Phase 4 [`PhaseHooks`] methods of
//! `src/worker.ts` @ 89b5c64, composed over Phase 3's [`RecordingHooks`]
//! (every Phase 3 method delegates unchanged).
//!
//! | hook | TS |
//! |---|---|
//! | `led_tick` | heartbeat `led.tick(); ledStarting = false;` (`:633-636`) |
//! | `led_config_applied` | `led.configApplied()` on a REAL change (`:866-872`; the caller gates on `changed`) |
//! | `led_stop` | shutdown `led.stop()` (`:1302`) |
//! | `reconcile_camera_mount` | `void reconcileCameraMountNow()` — config path (`:889`) + the 60 s poll (`:648-651`) |
//! | `reconcile_camera_mount_boot` | `await reconcileCameraMountNow()` (`:432`) |
//! | `camera_mount_state` | ConfigSync `cameraMount: () => cameraMountState` (`:713`) |
//! | `report_hints` | ConfigSync `hints()` (`:719-726`) + `clockStepperArmed: clockStepper?.status().armed` |
//!
//! The leaves (WP1 `LedManager`, WP3 `ClockStepper` / `CameraMountReconciler`)
//! are reached through the three small traits below, bound late (the TS
//! builds the LED after `recording.start()` and the stepper after it too,
//! `:536-625`, while the hooks exist from `:512` on). Unbound = the TS's
//! "not constructed yet": inert.
//!
//! ConfigSync mutex rule (`ctx.rs` `report_hints` doc, review-wp3 #5):
//! `camera_mount_state` and `report_hints` read snapshots only — the
//! implementations behind [`CameraMountControl::state`] and
//! [`StepperControl::armed`] must not take a lock whose holder can call into
//! ConfigSync.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use xorgate_core::config_report::{ReportHints, ReportOrigin};
use xorgate_core::telemetry::{RecordingStatus, TelemetryMedia};
use xorgate_core::types::CameraMountState;

use crate::ctx::{BoxFut, PhaseHooks, ReportRequester};
use crate::recording::hooks::RecordingHooks;

/// The worker's view of the status LED manager (WP1 `LedManager`, I2).
pub trait LedControl: Send + Sync + 'static {
    /// `led.start()` — the worker fires and forgets it (`void led.start()`, `:625`).
    fn start(&self) -> BoxFut<'_, ()>;
    /// `led.tick()` (1 s heartbeat).
    fn tick(&self);
    /// `led.configApplied()` (the two white flashes).
    fn config_applied(&self);
    /// `led.stop()` (best-effort blank at shutdown).
    fn stop(&self);
}

/// The worker's view of the offline clock stepper (WP3 `ClockStepper`, I6).
pub trait StepperControl: Send + Sync + 'static {
    /// `clockStepper.status().armed` — a snapshot read (ConfigSync mutex rule).
    fn armed(&self) -> bool;
}

/// The worker's view of the camera-mount reconciler (WP3, I7). The
/// reconciler owns `cameraMountState`, the summary comparison
/// (`summariseCameraMount`, `:197-199`) and the `[camera-mount] …` log
/// closure; it calls its change callback when the summary moved.
pub trait CameraMountControl: Send + Sync + 'static {
    /// One `reconcileCameraMount(…)` pass; never fails (the TS catch,
    /// `:190-194`, lives inside).
    fn now(&self) -> BoxFut<'_, ()>;
    /// `cameraMountState` — a snapshot read (ConfigSync mutex rule).
    fn state(&self) -> Option<CameraMountState>;
}

/// The Phase 4 hooks.
pub struct OpsHooks {
    rec: Arc<RecordingHooks>,
    /// `led` (`:596`), bound once the worker has built it.
    led: OnceLock<Arc<dyn LedControl>>,
    /// `let ledStarting = true;` (`:567`): cleared after the first heartbeat tick.
    led_starting: Arc<AtomicBool>,
    /// `clockStepper` (`:536`): bound in real mode only (else `null`).
    stepper: OnceLock<Arc<dyn StepperControl>>,
    /// The reconciler; unbound off real mode (`reconcileCameraMountNow`'s
    /// `if (config.mode !== "real") return;`, `:170`). Bound before the boot
    /// pass; its change callback is [`OpsHooks::camera_mount_on_change`].
    camera: OnceLock<Arc<dyn CameraMountControl>>,
    /// `requestConfigReport` (`:168`): `null` until the report plane exists
    /// (`:746`) — a change seen before that (the boot pass) requests nothing.
    reports: Arc<OnceLock<ReportRequester>>,
}

impl OpsHooks {
    /// The hooks exist before the camera reconciler, the LED and the
    /// stepper (each bound later, at its TS position) and before the report
    /// plane.
    pub fn new(rec: Arc<RecordingHooks>) -> Self {
        Self {
            rec,
            led: OnceLock::new(),
            led_starting: Arc::new(AtomicBool::new(true)),
            stepper: OnceLock::new(),
            camera: OnceLock::new(),
            reports: Arc::new(OnceLock::new()),
        }
    }

    /// The camera-mount reconciler (real mode only), before the boot pass.
    pub fn bind_camera(&self, camera: Arc<dyn CameraMountControl>) {
        let _ = self.camera.set(camera);
    }

    /// The composed Phase 3 hooks (`recording` and its LED status cache).
    pub fn recording(&self) -> &Arc<RecordingHooks> {
        &self.rec
    }

    /// `ledStarting`, for the LED's `signals()` closure (`:619`).
    pub fn led_starting_flag(&self) -> Arc<AtomicBool> {
        self.led_starting.clone()
    }

    /// The camera-mount change callback (`if (summary moved)
    /// requestConfigReport?.("boot")`, `:186-188`) — hand it to the
    /// reconciler. Inert until [`OpsHooks::set_report_requester`].
    pub fn camera_mount_on_change(&self) -> Arc<dyn Fn() + Send + Sync> {
        let reports = self.reports.clone();
        Arc::new(move || {
            if let Some(r) = reports.get() {
                r.request(ReportOrigin::Boot);
            }
        })
    }

    /// `requestConfigReport = (origin) => configSync.request(origin);` (`:746`).
    pub fn set_report_requester(&self, r: ReportRequester) {
        let _ = self.reports.set(r);
    }

    /// Bind the LED manager and fire `void led.start().catch(() => {})`
    /// (`:596-625`) on `spawn` — never awaited: worker startup must not
    /// wait on an indicator light.
    pub fn bind_led<S>(&self, led: Arc<dyn LedControl>, spawn: S)
    where
        S: FnOnce(BoxFut<'static, ()>),
    {
        if self.led.set(led.clone()).is_ok() {
            spawn(Box::pin(async move { led.start().await }));
        }
    }

    /// `clockStepper = createClockStepper({...}); clockStepper.start();`
    /// (`:537-554`) — the worker starts it; this keeps the status reader.
    pub fn bind_stepper(&self, stepper: Arc<dyn StepperControl>) {
        let _ = self.stepper.set(stepper);
    }
}

impl PhaseHooks for OpsHooks {
    // ---- Phase 3, delegated unchanged ----
    fn notify_connectivity(&self, online: bool) {
        self.rec.notify_connectivity(online)
    }
    fn recording_status(&self) -> RecordingStatus {
        self.rec.recording_status()
    }
    fn overlay_media(&self, media: TelemetryMedia) -> TelemetryMedia {
        self.rec.overlay_media(media)
    }
    fn reapply_recording(&self) -> Result<(), String> {
        self.rec.reapply_recording()
    }
    fn rearm_upload(&self) {
        self.rec.rearm_upload()
    }
    fn hardware_changed(&self) {
        self.rec.hardware_changed()
    }
    fn reconcile_hardware_boot(&self) -> BoxFut<'_, Result<(), String>> {
        self.rec.reconcile_hardware_boot()
    }
    fn recording_start(&self) -> BoxFut<'_, ()> {
        self.rec.recording_start()
    }
    fn note_recording_status(&self, status: &RecordingStatus) {
        self.rec.note_recording_status(status)
    }
    fn final_recording_status(&self) -> Option<RecordingStatus> {
        self.rec.final_recording_status()
    }
    fn recording_stop(&self) -> Result<(), String> {
        self.rec.recording_stop()
    }

    // ---- Phase 4 ----
    /// `void reconcileCameraMountNow()` (`:648-651` poll, `:889` config):
    /// fire-and-forget; a no-op off real mode.
    fn reconcile_camera_mount(&self) {
        if let Some(cam) = self.camera.get().cloned() {
            tokio::spawn(async move { cam.now().await });
        }
    }

    /// `await reconcileCameraMountNow()` (`:432`).
    fn reconcile_camera_mount_boot(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            if let Some(cam) = self.camera.get() {
                cam.now().await;
            }
        })
    }

    /// `cameraMount: () => cameraMountState` (`:713`).
    fn camera_mount_state(&self) -> Option<CameraMountState> {
        self.camera.get().and_then(|c| c.state())
    }

    /// `hints()` (`:719-726`): Phase 3's bootDefer/powerShed halves unchanged,
    /// plus `clockStepperArmed: clockStepper?.status().armed` (absent without
    /// a stepper, CF30).
    fn report_hints(&self) -> ReportHints {
        ReportHints {
            clock_stepper_armed: self.stepper.get().map(|s| s.armed()),
            ..self.rec.report_hints()
        }
    }

    /// Heartbeat (`:633-636`): `led.tick(); ledStarting = false;`.
    fn led_tick(&self) {
        if let Some(led) = self.led.get() {
            led.tick();
        }
        self.led_starting.store(false, Ordering::SeqCst);
    }

    fn led_config_applied(&self) {
        if let Some(led) = self.led.get() {
            led.config_applied();
        }
    }

    fn led_stop(&self) {
        if let Some(led) = self.led.get() {
            led.stop();
        }
    }
}

// ---- the leaves behind the three traits (WP1 I2, WP3 I6/I7) ----

impl LedControl for crate::ops::led::LedManager {
    fn start(&self) -> BoxFut<'_, ()> {
        crate::ops::led::LedManager::start(self)
    }
    fn tick(&self) {
        crate::ops::led::LedManager::tick(self)
    }
    fn config_applied(&self) {
        crate::ops::led::LedManager::config_applied(self)
    }
    fn stop(&self) {
        crate::ops::led::LedManager::stop(self)
    }
}

impl StepperControl for crate::ops::clock_stepper::ClockStepper {
    /// `clockStepper.status().armed`.
    fn armed(&self) -> bool {
        self.status().armed
    }
}

impl CameraMountControl for crate::ops::camera_mount::CameraMountReconciler {
    fn now(&self) -> BoxFut<'_, ()> {
        crate::ops::camera_mount::CameraMountReconciler::now(self)
    }
    fn state(&self) -> Option<CameraMountState> {
        crate::ops::camera_mount::CameraMountReconciler::state(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use xorgate_core::clock::Clock;

    struct WallZero;
    impl Clock for WallZero {
        fn wall_ms(&self) -> i64 {
            0
        }
        fn mono_ms(&self) -> u64 {
            0
        }
    }

    fn rec() -> Arc<RecordingHooks> {
        Arc::new(RecordingHooks::new(
            None,
            None,
            Arc::new(|| None),
            Arc::new(WallZero),
        ))
    }

    #[derive(Default)]
    struct FakeLed {
        log: Mutex<Vec<&'static str>>,
    }
    impl LedControl for FakeLed {
        fn start(&self) -> BoxFut<'_, ()> {
            self.log.lock().unwrap().push("start");
            Box::pin(async {})
        }
        fn tick(&self) {
            self.log.lock().unwrap().push("tick");
        }
        fn config_applied(&self) {
            self.log.lock().unwrap().push("config_applied");
        }
        fn stop(&self) {
            self.log.lock().unwrap().push("stop");
        }
    }

    struct FakeStepper(AtomicBool);
    impl StepperControl for FakeStepper {
        fn armed(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct FakeCam {
        passes: Mutex<u32>,
        state: Mutex<Option<CameraMountState>>,
    }
    impl CameraMountControl for FakeCam {
        fn now(&self) -> BoxFut<'_, ()> {
            Box::pin(async move {
                *self.passes.lock().unwrap() += 1;
                *self.state.lock().unwrap() = Some(CameraMountState {
                    at: 1,
                    cameras: vec![],
                    reboot_required: true,
                    wrote: true,
                });
            })
        }
        fn state(&self) -> Option<CameraMountState> {
            self.state.lock().unwrap().clone()
        }
    }

    /// `ledStarting` is true until the first heartbeat tick, and the tick
    /// reaches the LED before the flag drops (`led.tick(); ledStarting = false;`).
    /// Unbound LED (not built yet) = inert, the flag still drops.
    #[test]
    fn led_tick_then_starting_false() {
        let h = OpsHooks::new(rec());
        let flag = h.led_starting_flag();
        assert!(flag.load(Ordering::SeqCst));
        let led = Arc::new(FakeLed::default());
        let mut spawned = 0;
        h.bind_led(led.clone(), |_fut| spawned += 1);
        assert_eq!(spawned, 1, "led.start() fired once, not awaited");
        h.led_tick();
        assert!(!flag.load(Ordering::SeqCst));
        h.led_config_applied();
        h.led_stop();
        assert_eq!(*led.log.lock().unwrap(), ["tick", "config_applied", "stop"]);
        // A second bind is ignored (the TS builds one LedManager per boot).
        h.bind_led(Arc::new(FakeLed::default()), |_| spawned += 1);
        assert_eq!(spawned, 1);

        let h2 = OpsHooks::new(rec());
        h2.led_tick();
        h2.led_config_applied();
        h2.led_stop();
        assert!(!h2.led_starting_flag().load(Ordering::SeqCst));
    }

    /// CF30: `clockStepperArmed` = `clockStepper?.status().armed` — absent
    /// without a stepper (sim mode), the live snapshot with one; the Phase 3
    /// halves stay as RecordingHooks reports them (no manager → defaults).
    #[test]
    fn report_hints_add_clock_stepper_armed() {
        let h = OpsHooks::new(rec());
        assert_eq!(h.report_hints(), ReportHints::default());
        let s = Arc::new(FakeStepper(AtomicBool::new(true)));
        h.bind_stepper(s.clone());
        assert_eq!(h.report_hints().clock_stepper_armed, Some(true));
        s.0.store(false, Ordering::SeqCst); // NTP synced → disarmed
        let hints = h.report_hints();
        assert_eq!(hints.clock_stepper_armed, Some(false));
        assert_eq!(hints.boot_defer, None);
        assert_eq!(hints.power_shed, None);
    }

    /// Off real mode there is no reconciler: every camera-mount hook is inert
    /// (`if (config.mode !== "real") return;`) and the state stays `null`.
    #[tokio::test]
    async fn camera_mount_inert_without_reconciler() {
        let h = OpsHooks::new(rec());
        h.reconcile_camera_mount_boot().await;
        h.reconcile_camera_mount();
        assert!(h.camera_mount_state().is_none());
    }

    /// Boot pass is awaited (the first report already carries the verdict);
    /// the fire-and-forget kick runs a pass on its own task.
    #[tokio::test]
    async fn camera_mount_boot_awaited_and_kick_spawned() {
        let cam = Arc::new(FakeCam::default());
        let h = OpsHooks::new(rec());
        h.bind_camera(cam.clone());
        assert!(h.camera_mount_state().is_none());
        h.reconcile_camera_mount_boot().await;
        assert_eq!(*cam.passes.lock().unwrap(), 1);
        assert!(h.camera_mount_state().unwrap().reboot_required);
        h.reconcile_camera_mount();
        for _ in 0..50 {
            if *cam.passes.lock().unwrap() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*cam.passes.lock().unwrap(), 2);
    }

    /// `requestConfigReport` is null until the report plane exists: a change
    /// seen by the boot pass requests nothing; after binding, `boot`.
    #[tokio::test]
    async fn camera_mount_change_requests_boot_report_once_plane_is_up() {
        let h = OpsHooks::new(rec());
        let on_change = h.camera_mount_on_change();
        on_change(); // before the plane: dropped (TS: requestConfigReport === null)
        let (req, mut rx) = ReportRequester::channel();
        h.set_report_requester(req);
        assert!(rx.try_recv().is_err());
        on_change();
        assert_eq!(rx.try_recv().unwrap(), ReportOrigin::Boot);
        assert!(rx.try_recv().is_err());
    }
}
