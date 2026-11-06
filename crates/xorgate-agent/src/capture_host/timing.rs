//! Exact fragment timing in the capture host (phase-7a §Design 8.1-8.3, WP1).
//!
//! - `format-location-full` on the (one) splitmuxsink, found recursively by
//!   factory name: reads, in this order, the first sample's buffer PTS, its
//!   segment's running time for that PTS, `pipeline.base_time()`, the
//!   pipeline clock's `time()` and CLOCK_REALTIME, and turns them into one
//!   [`FragmentStart`]. It returns `None`, so splitmuxsink falls back to its
//!   `location` property and the file names stay `%05d.mp4`.
//! - `splitmuxsink-fragment-closed` (an element message, seen in the bus
//!   SYNC handler on the posting thread) gives the end of the last fragment;
//!   the bus loop prints its `XG-FRAGMENT-END` only on the pipeline's EOS
//!   (clean EOS), for the last opened id.
//! - Both callbacks run on streaming threads, under splitmuxsink's own lock:
//!   they never block, never call into splitmuxsink, take no lock the bus
//!   loop holds, do no I/O and are `catch_unwind`ed. They hand the formatted
//!   line to the bus loop as an application message ([`TIMING_MESSAGE`]); the
//!   loop (main thread) is the only writer of stdout.
//! - Clock precondition (R5), checked ONCE per run, at PLAYING: the pipeline
//!   clock is a `GstSystemClock` with `clock-type=monotonic` (CLOCK_MONOTONIC,
//!   the agent's `hrtime_ns` frame). Otherwise no timing line is emitted and
//!   one stderr line says why. "At PLAYING" is whichever comes first: the
//!   pipeline's PLAYING state-change message (bus sync handler), or the first
//!   `format-location-full`. Measured on the rig (WP1): fragment 0 is opened
//!   at its FIRST frame, ~10-20 ms after the source starts, i.e. DURING the
//!   PAUSED→PLAYING transition and before the pipeline posts its PLAYING
//!   message; the clock and `base_time` are already distributed then
//!   (GstPipeline sets both before its children go to PLAYING). Fragments
//!   N ≥ 1 open one GOP late (lag ≈ 2.0 s at 720p15).
//! - Guard (§Design 8.2): [`capture_timing::lag_in_guard`]. A rejected stamp
//!   is not emitted (that fragment keeps the legacy anchors); the first
//!   rejection of a run prints one stderr line.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;
use xorgate_core::capture_timing::{self, FragmentEnd, FragmentStart, Reject};

/// The private bus message that carries one timing event to the bus loop.
pub const TIMING_MESSAGE: &str = "XorgateCaptureTiming";
/// splitmuxsink's element message for a closed fragment (1.22).
pub const FRAGMENT_CLOSED: &str = "splitmuxsink-fragment-closed";
const SPLITMUXSINK: &str = "splitmuxsink";

const CLOCK_UNKNOWN: u8 = 0;
const CLOCK_OK: u8 = 1;
const CLOCK_BAD: u8 = 2;

/// State shared between the callbacks (streaming threads) and the bus loop.
#[derive(Debug)]
pub struct Timing {
    clock: AtomicU8,
    /// One frame period from the first sample's caps (0 = unknown).
    frame_period_ns: AtomicU64,
    /// The last `fragment_id` handed to `format-location-full` (−1 = none).
    last_open: AtomicI64,
    warned: AtomicBool,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            clock: AtomicU8::new(CLOCK_UNKNOWN),
            frame_period_ns: AtomicU64::new(0),
            last_open: AtomicI64::new(-1),
            warned: AtomicBool::new(false),
        }
    }
}

/// What the bus loop does with one [`TIMING_MESSAGE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The clock precondition failed in a callback: print this on stderr.
    ClockOff(String),
    /// Print this `XG-FRAGMENT` line now.
    Start(String),
    /// Keep this `XG-FRAGMENT-END` line (fragment `id`) until the EOS.
    End(u32, String),
    /// A stamp was refused: print this once per run on stderr.
    Rejected(String),
}

impl Timing {
    /// The one splitmuxsink of the pipeline (recursive, by factory name),
    /// or `None` (bridge-only, or an argv with several).
    pub fn find_splitmuxsink(pipeline: &gst::Element) -> Option<gst::Element> {
        let bin = pipeline.downcast_ref::<gst::Bin>()?;
        let mut found = Vec::new();
        let mut it = bin.iterate_all_by_element_factory_name(SPLITMUXSINK);
        loop {
            match it.next() {
                Ok(Some(e)) => found.push(e),
                Ok(None) => break,
                Err(gst::IteratorError::Resync) => {
                    found.clear();
                    it.resync();
                }
                Err(gst::IteratorError::Error) => return None,
            }
        }
        (found.len() == 1).then(|| found.remove(0))
    }

    /// Connects `format-location-full` on `splitmux`. The handler posts the
    /// stamp to `bus` and returns `None`.
    pub fn connect(self: &Arc<Self>, splitmux: &gst::Element, pipeline: &gst::Element) {
        let me = self.clone();
        let weak = pipeline.downgrade();
        splitmux.connect("format-location-full", false, move |args| {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let id = args.get(1).and_then(|v| v.get::<u32>().ok());
                let sample = args
                    .get(2)
                    .and_then(|v| v.get::<Option<gst::Sample>>().ok());
                if let (Some(id), Some(Some(sample)), Some(p)) = (id, sample, weak.upgrade()) {
                    if let Some(ev) = me.on_fragment_opened(id, &sample, &p) {
                        post(&p, ev);
                    }
                }
            }));
            Some(None::<String>.to_value())
        });
    }

    /// The PLAYING check (R5), once per run: the bus SYNC handler calls it on
    /// the pipeline's state change to PLAYING, and the first callback calls
    /// it if that has not happened yet. `Some(line)` = the one stderr line
    /// when the precondition fails (returned to exactly one caller).
    pub fn on_playing(&self, pipeline: &gst::Element) -> Option<String> {
        if self.clock.load(Ordering::SeqCst) != CLOCK_UNKNOWN {
            return None;
        }
        let verdict = clock_precondition(pipeline.clock().as_ref());
        let v = if verdict.is_ok() { CLOCK_OK } else { CLOCK_BAD };
        if self
            .clock
            .compare_exchange(CLOCK_UNKNOWN, v, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None; // the other caller decided first
        }
        verdict
            .err()
            .map(|why| format!("xorgate-capture: exact fragment timing off for this run: {why}\n"))
    }

    fn clock_ok(&self) -> bool {
        self.clock.load(Ordering::SeqCst) == CLOCK_OK
    }

    /// `format-location-full(splitmux, fragment_id, first_sample)`.
    fn on_fragment_opened(
        &self,
        id: u32,
        sample: &gst::Sample,
        pipeline: &gst::Element,
    ) -> Option<Event> {
        self.last_open.store(i64::from(id), Ordering::SeqCst);
        if let Some(p) = sample.caps().and_then(frame_period_ns) {
            self.frame_period_ns.store(p, Ordering::SeqCst);
        }
        if let Some(line) = self.on_playing(pipeline) {
            return Some(Event::ClockOff(line));
        }
        if !self.clock_ok() {
            return None;
        }
        // §Design 8.1, in this order: PTS, running time, base time, clock
        // now, then the wall clock in the same few µs.
        let rt = sample_running_time(sample)?;
        let base = pipeline.base_time()?;
        let now = pipeline.clock()?.time();
        let wall = wall_ns();
        let frame_mono = base.nseconds().checked_add(rt.nseconds())?;
        Some(
            self.event(
                id,
                FragmentStart::from_clocks(id, frame_mono, now.nseconds(), wall)
                    .map(|s| Event::Start(s.to_line())),
            ),
        )
    }

    /// `splitmuxsink-fragment-closed` in the SYNC handler (posting thread).
    /// Measured on the rig (1.22, `async-finalize=false`): a ROTATION's
    /// message carries the running time of the NEXT fragment's first frame;
    /// the final close at EOS carries the running time of the last frame's
    /// START. Only the final one is printed (the loop keeps the latest and
    /// prints it on the pipeline's EOS), so the end is that running time plus
    /// one frame period.
    pub fn on_fragment_closed(
        &self,
        s: &gst::StructureRef,
        pipeline: &gst::Element,
    ) -> Option<Event> {
        if !self.clock_ok() {
            return None;
        }
        let rt = s.get::<Option<gst::ClockTime>>("running-time").ok()??;
        let base = pipeline.base_time()?;
        let now = pipeline.clock()?.time();
        let wall = wall_ns();
        let id = s
            .get::<Option<String>>("location")
            .ok()
            .flatten()
            .and_then(|l| fragment_id_of(&l))
            .or_else(|| u32::try_from(self.last_open.load(Ordering::SeqCst)).ok())?;
        let last_frame = base.nseconds().checked_add(rt.nseconds())?;
        let period = self.frame_period_ns.load(Ordering::SeqCst);
        Some(
            self.event(
                id,
                FragmentEnd::from_clocks(id, last_frame, period, now.nseconds(), wall)
                    .map(|e| Event::End(id, e.to_line())),
            ),
        )
    }

    fn event(&self, id: u32, r: Result<Event, Reject>) -> Event {
        match r {
            Ok(ev) => ev,
            Err(why) => Event::Rejected(reject_line(id, why)),
        }
    }

    /// The bus loop: true the first time a rejection is reported.
    pub fn first_rejection(&self) -> bool {
        !self.warned.swap(true, Ordering::SeqCst)
    }

    /// The last fragment id opened (the END line is printed only for it).
    pub fn last_open(&self) -> Option<u32> {
        u32::try_from(self.last_open.load(Ordering::SeqCst)).ok()
    }
}

fn reject_line(id: u32, why: Reject) -> String {
    match why {
        Reject::Guard(lag) => format!(
            "xorgate-capture: fragment {id}: exact timing rejected (lag {lag} ns outside 0..={} ns); \
             legacy anchors for the refused fragments of this run\n",
            capture_timing::MAX_LAG_NS
        ),
        Reject::Wall(w) => format!(
            "xorgate-capture: fragment {id}: exact timing rejected (wall clock {w} ns); \
             legacy anchors for the refused fragments of this run\n"
        ),
    }
}

/// R5: a `GstSystemClock` with `clock-type=monotonic`.
pub fn clock_precondition(clock: Option<&gst::Clock>) -> Result<(), String> {
    let Some(clock) = clock else {
        return Err("the pipeline has no clock".to_string());
    };
    if clock.downcast_ref::<gst::SystemClock>().is_none() {
        return Err(format!(
            "the pipeline clock {} ({}) is not a GstSystemClock",
            clock.name(),
            clock.type_().name()
        ));
    }
    let ty = clock.property::<gst::ClockType>("clock-type");
    if ty != gst::ClockType::Monotonic {
        return Err(format!(
            "the pipeline clock {} has clock-type {ty:?}, not monotonic",
            clock.name()
        ));
    }
    Ok(())
}

/// The running time of the sample's buffer PTS in the SAMPLE's segment
/// (never the raw PTS).
pub fn sample_running_time(sample: &gst::Sample) -> Option<gst::ClockTime> {
    let pts = sample.buffer()?.pts()?;
    let seg = sample.segment()?.downcast_ref::<gst::ClockTime>()?;
    seg.to_running_time(pts)
}

/// One frame period from `framerate=N/D` caps (`D/N` s).
fn frame_period_ns(caps: &gst::CapsRef) -> Option<u64> {
    let fr = caps.structure(0)?.get::<gst::Fraction>("framerate").ok()?;
    let (n, d) = (
        u64::try_from(fr.numer()).ok()?,
        u64::try_from(fr.denom()).ok()?,
    );
    if n == 0 {
        return None;
    }
    d.checked_mul(1_000_000_000).map(|x| x / n)
}

/// The `%05d` sequence of a splitmuxsink location (`…/00012.mp4` → 12).
pub fn fragment_id_of(location: &str) -> Option<u32> {
    let name = location.rsplit('/').next()?;
    let stem = name.strip_suffix(".mp4")?;
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

/// CLOCK_REALTIME in ns.
fn wall_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Hands one event to the bus loop (non-blocking post).
pub fn post(pipeline: &gst::Element, ev: Event) {
    let (kind, id, line) = match ev {
        Event::ClockOff(l) => ("clockoff", 0u32, l),
        Event::Start(l) => ("start", 0, l),
        Event::End(id, l) => ("end", id, l),
        Event::Rejected(l) => ("rejected", 0, l),
    };
    let s = gst::Structure::builder(TIMING_MESSAGE)
        .field("kind", kind)
        .field("id", id)
        .field("line", line)
        .build();
    let _ = pipeline.post_message(gst::message::Application::builder(s).src(pipeline).build());
}

/// The bus loop's decoding of a [`TIMING_MESSAGE`].
pub fn decode(s: &gst::StructureRef) -> Option<Event> {
    let line = s.get::<String>("line").ok()?;
    match s.get::<&str>("kind").ok()? {
        "clockoff" => Some(Event::ClockOff(line)),
        "start" => Some(Event::Start(line)),
        "end" => Some(Event::End(s.get::<u32>("id").ok()?, line)),
        "rejected" => Some(Event::Rejected(line)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        gst::init().unwrap();
    }

    /// The reviewer focus "PTS → running time uses the sample's segment, not
    /// raw PTS": a segment that starts at 5 s with base 1 s maps PTS 7 s to
    /// running time 3 s.
    #[test]
    fn running_time_comes_from_the_sample_segment() {
        init();
        let mut buf = gst::Buffer::new();
        buf.get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_seconds(7));
        let mut seg = gst::FormattedSegment::<gst::ClockTime>::new();
        seg.set_start(gst::ClockTime::from_seconds(5));
        seg.set_base(gst::ClockTime::from_seconds(1));
        let caps = gst::Caps::builder("video/x-raw")
            .field("framerate", gst::Fraction::new(15, 1))
            .build();
        let sample = gst::Sample::builder()
            .buffer(&buf)
            .segment(&seg)
            .caps(&caps)
            .build();
        assert_eq!(
            sample_running_time(&sample),
            Some(gst::ClockTime::from_seconds(3))
        );
        assert_eq!(frame_period_ns(sample.caps().unwrap()), Some(66_666_666));
        // No PTS → no stamp.
        let none = gst::Sample::builder()
            .buffer(&gst::Buffer::new())
            .segment(&seg)
            .build();
        assert_eq!(sample_running_time(&none), None);
    }

    #[test]
    fn the_clock_precondition_wants_a_monotonic_system_clock() {
        init();
        assert_eq!(
            clock_precondition(Some(&gst::SystemClock::obtain())),
            Ok(())
        );
        let rt: gst::Clock = gst::glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Realtime)
            .build()
            .upcast();
        let e = clock_precondition(Some(&rt)).unwrap_err();
        assert!(e.contains("not monotonic"), "{e}");
        assert!(clock_precondition(None).is_err());
    }

    #[test]
    fn fragment_ids_parse_from_the_percent_05d_location() {
        assert_eq!(fragment_id_of("/r/v/cam0/s/00000.mp4"), Some(0));
        assert_eq!(fragment_id_of("/r/v/cam0/s/00042.mp4"), Some(42));
        assert_eq!(fragment_id_of("00007.mp4"), Some(7));
        assert_eq!(fragment_id_of("/x/clip.mp4"), None);
        assert_eq!(fragment_id_of("/x/00001.mkv"), None);
    }

    /// A live run end to end on core elements: the stamp follows the PLAYING
    /// precondition, the reject path, and the bus-message round trip.
    #[test]
    fn opened_and_closed_stamps_round_trip_through_the_bus_message() {
        init();
        let pipeline: gst::Element = gst::parse::launch("fakesrc is-live=true ! fakesink")
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap()
            .upcast();
        pipeline.set_state(gst::State::Playing).unwrap();
        let _ = pipeline.state(gst::ClockTime::from_seconds(5));
        let t = Arc::new(Timing::default());

        let mut buf = gst::Buffer::new();
        buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        let seg = gst::FormattedSegment::<gst::ClockTime>::new();
        let caps = gst::Caps::builder("video/x-raw")
            .field("framerate", gst::Fraction::new(30, 1))
            .build();
        let sample = gst::Sample::builder()
            .buffer(&buf)
            .segment(&seg)
            .caps(&caps)
            .build();

        // The first callback runs the PLAYING check itself (fragment 0 opens
        // before the pipeline's PLAYING message) and stamps.
        let ev0 = t.on_fragment_opened(0, &sample, &pipeline).unwrap();
        assert!(
            matches!(&ev0, Event::Start(l) if l.contains("\"id\":0,")),
            "{ev0:?}"
        );
        assert_eq!(t.last_open(), Some(0));
        assert_eq!(t.on_playing(&pipeline), None, "checked once");
        // Running time 0 = base_time: lag = time since PLAYING (< 7 s).
        let ev = t.on_fragment_opened(1, &sample, &pipeline).unwrap();
        let Event::Start(line) = &ev else {
            panic!("{ev:?}")
        };
        let Some(Ok(capture_timing::TimingLine::Start(s))) = capture_timing::parse_line(line)
        else {
            panic!("{line}")
        };
        assert_eq!(s.id, 1);
        assert!(s.lag_ms < 7_000, "{s:?}");
        assert_eq!(s.frame_mono_ns, pipeline.base_time().unwrap().nseconds());

        // A sample whose PTS is 10 s in the future: negative lag → rejected.
        let mut late = gst::Buffer::new();
        late.get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_seconds(10));
        let future = gst::Sample::builder().buffer(&late).segment(&seg).build();
        let rej = t.on_fragment_opened(2, &future, &pipeline).unwrap();
        assert!(
            matches!(&rej, Event::Rejected(l) if l.contains("fragment 2")),
            "{rej:?}"
        );
        assert!(t.first_rejection());
        assert!(!t.first_rejection());

        // The end (+ one 33 ms frame) must lie in the past of the clock.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let closed = gst::Structure::builder(FRAGMENT_CLOSED)
            .field("location", "/v/00002.mp4")
            .field("running-time", gst::ClockTime::ZERO)
            .build();
        let ev = t.on_fragment_closed(&closed, &pipeline).unwrap();
        let Event::End(2, line) = &ev else {
            panic!("{ev:?}")
        };
        let Some(Ok(capture_timing::TimingLine::End(e))) = capture_timing::parse_line(line) else {
            panic!("{line}")
        };
        // + one frame period at 30 fps.
        assert_eq!(
            e.end_mono_ns,
            pipeline.base_time().unwrap().nseconds() + 33_333_333
        );

        // The bus message round trip.
        let bus = pipeline.bus().unwrap();
        post(&pipeline, ev.clone());
        let msg = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(2),
                &[gst::MessageType::Application],
            )
            .unwrap();
        let gst::MessageView::Application(a) = msg.view() else {
            unreachable!()
        };
        assert_eq!(decode(a.structure().unwrap()), Some(ev));
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn a_non_monotonic_clock_turns_timing_off_with_one_line() {
        init();
        let pipeline: gst::Pipeline = gst::parse::launch("fakesrc is-live=true ! fakesink")
            .unwrap()
            .downcast()
            .unwrap();
        let rt: gst::Clock = gst::glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Realtime)
            .build()
            .upcast();
        pipeline.use_clock(Some(&rt));
        pipeline.set_state(gst::State::Playing).unwrap();
        let _ = pipeline.state(gst::ClockTime::from_seconds(5));
        let p: gst::Element = pipeline.clone().upcast();
        let mut buf = gst::Buffer::new();
        buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        let sample = gst::Sample::builder()
            .buffer(&buf)
            .segment(&gst::FormattedSegment::<gst::ClockTime>::new())
            .build();
        // Checked by the PLAYING message: one line, then nothing.
        let t = Timing::default();
        let line = t.on_playing(&p).expect("one line");
        assert!(
            line.starts_with("xorgate-capture: exact fragment timing off"),
            "{line}"
        );
        assert_eq!(t.on_playing(&p), None, "once");
        assert_eq!(t.on_fragment_opened(0, &sample, &p), None);
        // Checked by the first callback instead: the line travels as an
        // event (the loop prints it); later callbacks stay silent.
        let t = Timing::default();
        let ev = t.on_fragment_opened(0, &sample, &p).unwrap();
        assert!(
            matches!(&ev, Event::ClockOff(l) if l.contains("not monotonic")),
            "{ev:?}"
        );
        assert_eq!(t.on_fragment_opened(1, &sample, &p), None);
        assert_eq!(t.on_playing(&p), None);
        pipeline.set_state(gst::State::Null).unwrap();
    }
}
