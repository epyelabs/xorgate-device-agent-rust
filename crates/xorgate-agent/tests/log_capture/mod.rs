//! Per-test log capture over ONE process-wide subscriber (F-FLAKE-1).
//!
//! The old helpers installed a thread-local `tracing::subscriber::set_default`
//! per test. tracing caches each callsite's `Interest` PROCESS-wide, computed
//! from the dispatchers alive when the callsite registers (or is rebuilt).
//! With tests in parallel threads, a callsite can register while no scoped
//! dispatcher is alive, or race a `set_default`'s cache rebuild, and cache
//! `never`. From then on, that callsite's events reach no subscriber, even
//! inside a test that has one installed. That is how
//! `a_supervisor_inside_a_pending_transition_gets_the_exit_cleanup` once got
//! an empty capture of `reap_capture_at_exit` (`manager.rs:815`).
//! `rebuild_interest_cache()` after `set_default` only narrows the race.
//!
//! The fix:
//! - **One global subscriber**, installed once per test binary: a `Registry`
//!   plus a router layer, whose interest is `always` for every callsite. No
//!   scoped dispatcher ever exists, so nothing can cache `never`.
//! - **Per-thread routing:** the router hands each event to the layer the
//!   CURRENT thread installed with [`install`]; the innermost wins, as with
//!   nested `set_default`. Each test keeps its own `Capture` layer unchanged,
//!   and still sees exactly the events emitted on its own thread.

#![allow(dead_code)]

use std::cell::RefCell;
use std::sync::Once;

use tracing::Event;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

type Sink = Box<dyn Layer<Registry> + Send + Sync>;

thread_local! {
    static SINKS: RefCell<Vec<Sink>> = const { RefCell::new(Vec::new()) };
}

struct Router;

impl Layer<Registry> for Router {
    fn on_event(&self, ev: &Event<'_>, ctx: Context<'_, Registry>) {
        SINKS.with(|s| {
            // A sink that logs from inside on_event would re-enter: skip it.
            if let Ok(s) = s.try_borrow() {
                if let Some(top) = s.last() {
                    top.on_event(ev, ctx);
                }
            }
        });
    }
}

fn ensure_global() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        tracing::subscriber::set_global_default(Registry::default().with(Router))
            .expect("log_capture: a global tracing subscriber was already set in this test binary");
    });
}

/// Route this thread's events to `layer` until the guard drops.
pub fn install<L: Layer<Registry> + Send + Sync + 'static>(layer: L) -> CaptureGuard {
    ensure_global();
    // A callsite that registered before (or while) the global was installed
    // may hold a stale `never`; with only the global alive, a rebuild makes
    // every cached interest `always` again before this test emits anything.
    tracing::callsite::rebuild_interest_cache();
    SINKS.with(|s| s.borrow_mut().push(Box::new(layer)));
    CaptureGuard {
        _not_send: std::marker::PhantomData,
    }
}

/// Pops the layer [`install`] pushed (on the thread that pushed it).
pub struct CaptureGuard {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        SINKS.with(|s| {
            s.borrow_mut().pop();
        });
    }
}
