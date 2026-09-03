//! The engine's liveness beat: a timer thread that emits one short span + event per period so a
//! run that has gone quiet is distinguishable from a run that has gone dead.
//!
//! Motivation: a loop can sit `Running` and span-silent for hours (a wedged turn, a hung RPC), and
//! the pod's logs die with the pod. A beat every minute gives two independent witnesses: the OTLP
//! span (which a span-silence monitor can alert on) and a pod-log line.
//!
//! Deliberately NOT a [`crate::session::SessionEvent`]: a line per minute would bloat the session
//! log that resume replays, and the audience for liveness is the collector and `oc logs`, not the
//! replay.

use std::io::IsTerminal as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Beat period when `CRUCIBLE_HEARTBEAT_SECS` is unset.
const DEFAULT_PERIOD: Duration = Duration::from_secs(60);

/// How long the beat thread sleeps between stop-flag checks, so a stop at the end of a run joins
/// promptly instead of waiting out the remaining period.
const TICK: Duration = Duration::from_millis(250);

/// The beat's view of the run, written by the loop and read by the beat thread. Lock-free because
/// the loop must never block on telemetry: `iter` and `spent` are independent scalars and a beat
/// that catches them mid-update reports a one-iteration-stale spend, which is fine for liveness.
#[derive(Debug, Default)]
pub(crate) struct Heartbeat {
    iter: AtomicU32,
    /// Spend in thousandths of a dollar: the atomic is integral, and a tenth of a cent is finer
    /// than any turn's cost.
    spent_milli_usd: AtomicU64,
    stop: AtomicBool,
}

impl Heartbeat {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Publish the loop's current position. Called at the same points that update the control
    /// status, so the beat never reports a state the control plane has not seen.
    pub(crate) fn record(&self, iter: u32, spent_usd: f64) {
        self.iter.store(iter, Ordering::Relaxed);
        // `as` saturates at the u64 bounds, so a NaN/negative/absurd spend cannot panic here.
        let milli = (spent_usd * 1000.0).round();
        let milli = if milli.is_finite() && milli > 0.0 {
            milli as u64
        } else {
            0
        };
        self.spent_milli_usd.store(milli, Ordering::Relaxed);
    }

    /// Tell the beat thread to exit; it does so within [`TICK`].
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn iter(&self) -> u32 {
        self.iter.load(Ordering::Relaxed)
    }

    pub(crate) fn spent_usd(&self) -> f64 {
        self.spent_milli_usd.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// One beat: a span for the OTLP layer (what a span-silence monitor watches) and an event
    /// inside it. The engine installs no fmt layer on purpose (it narrates through the sink), so
    /// the pod-log witness is an explicit `eprintln!` rather than a subscriber's doing.
    ///
    /// `to_stderr` is off for an interactive console, where a line a minute is just noise; the
    /// span goes out either way.
    fn beat(&self, to_stderr: bool) {
        let iter = self.iter();
        let spent_usd = self.spent_usd();
        tracing::info_span!("heartbeat", iter, spent_usd).in_scope(|| {
            tracing::info!(target: "crucible::heartbeat", iter, spent_usd, "alive");
        });
        if to_stderr {
            eprintln!("[crucible] heartbeat: iter {iter}, spent ${spent_usd:.4}");
        }
    }
}

/// The configured beat period: [`DEFAULT_PERIOD`], overridden by `CRUCIBLE_HEARTBEAT_SECS`.
/// Only an explicit `0` disables the beat; a typo falls back to the default rather than silently
/// turning liveness reporting off, which is the failure mode a monitor cannot distinguish from a
/// dead run.
pub(crate) fn period_from_env() -> Option<Duration> {
    let Ok(raw) = std::env::var("CRUCIBLE_HEARTBEAT_SECS") else {
        return Some(DEFAULT_PERIOD);
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(e) => {
            eprintln!(
                "[crucible] CRUCIBLE_HEARTBEAT_SECS={raw:?} is not a number ({e}); using the {}s default",
                DEFAULT_PERIOD.as_secs()
            );
            Some(DEFAULT_PERIOD)
        }
    }
}

/// Owns the beat thread and the guarantee that it is stopped and joined before the run span it
/// holds is dropped. The thread holds a clone of that span, so a beat outliving the run would keep
/// the span open and unexported.
///
/// Drop covers the caller's error returns; the success path exits through `process::exit`, which
/// runs no destructors, so it drops the guard explicitly before flushing.
pub(crate) struct BeatGuard {
    hb: Arc<Heartbeat>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl BeatGuard {
    /// The shared position the loop writes to.
    pub(crate) fn beat(&self) -> Arc<Heartbeat> {
        Arc::clone(&self.hb)
    }
}

impl Drop for BeatGuard {
    fn drop(&mut self) {
        self.hb.stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Start the beat under `parent` and hand back the guard that shuts it down.
pub(crate) fn start(period: Duration, parent: Option<tracing::Span>) -> BeatGuard {
    let hb = Arc::new(Heartbeat::new());
    let handle = spawn(Arc::clone(&hb), period, parent);
    BeatGuard {
        hb,
        handle: Some(handle),
    }
}

/// Start the beat thread. It runs until [`Heartbeat::stop`], so the caller must stop it before
/// joining; nothing inside can panic, so a lost join is a leaked sleep, never a poisoned run.
///
/// `parent` is the run span the beats nest under. The caller must stop and join the beat before
/// dropping that span: the thread holds a clone, which keeps the span (and its exported duration)
/// open.
pub(crate) fn spawn(
    hb: Arc<Heartbeat>,
    period: Duration,
    parent: Option<tracing::Span>,
) -> std::thread::JoinHandle<()> {
    // Both of tracing's contexts are thread-locals a fresh thread does not inherit: the subscriber
    // (carried over as a Dispatch clone) and the current-span stack (re-entered from `parent`).
    let dispatch = tracing::dispatcher::get_default(tracing::Dispatch::clone);
    // A pod's stderr IS its log; an interactive terminal does not want a line a minute. Sampled
    // once, on the parent's thread, because the answer cannot change mid-run.
    let to_stderr = !std::io::stderr().is_terminal();
    std::thread::spawn(move || {
        let _dispatch = tracing::dispatcher::set_default(&dispatch);
        let _parent = parent.as_ref().map(tracing::Span::enter);
        while !hb.stopped() {
            if !sleep_until_due(&hb, period) {
                return;
            }
            hb.beat(to_stderr);
        }
    })
}

/// Sleep out one period in [`TICK`] slices. `false` means a stop arrived; the caller must not beat.
fn sleep_until_due(hb: &Heartbeat, period: Duration) -> bool {
    let mut left = period;
    while !left.is_zero() {
        if hb.stopped() {
            return false;
        }
        let slice = TICK.min(left);
        std::thread::sleep(slice);
        left -= slice;
    }
    !hb.stopped()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    /// One captured heartbeat event's fields.
    #[derive(Debug, Clone, PartialEq)]
    struct Beat {
        iter: u64,
        spent_usd: f64,
        message: String,
    }

    #[derive(Default)]
    struct BeatVisitor {
        iter: u64,
        spent_usd: f64,
        message: String,
    }

    impl Visit for BeatVisitor {
        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "iter" {
                self.iter = value;
            }
        }
        fn record_f64(&mut self, field: &Field, value: f64) {
            if field.name() == "spent_usd" {
                self.spent_usd = value;
            }
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = format!("{value:?}");
            }
        }
    }

    /// A real subscriber layer that records every `crucible::heartbeat` event and every opened
    /// span name. No mocks: the beats travel the actual tracing pipeline.
    #[derive(Clone, Default)]
    struct Capture {
        beats: Arc<Mutex<Vec<Beat>>>,
        spans: Arc<Mutex<Vec<String>>>,
    }

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::Id,
            _ctx: Context<'_, S>,
        ) {
            if let Ok(mut s) = self.spans.lock() {
                s.push(attrs.metadata().name().to_string());
            }
        }

        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            if event.metadata().target() != "crucible::heartbeat" {
                return;
            }
            let mut v = BeatVisitor::default();
            event.record(&mut v);
            if let Ok(mut b) = self.beats.lock() {
                b.push(Beat {
                    iter: v.iter,
                    spent_usd: v.spent_usd,
                    message: v.message,
                });
            }
        }
    }

    #[test]
    fn beats_carry_iter_and_spend_and_stop_joins() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let hb = Arc::new(Heartbeat::new());
        hb.record(7, 23.4567);

        let handle = {
            let _guard = tracing::subscriber::set_default(subscriber);
            let handle = spawn(Arc::clone(&hb), Duration::from_millis(300), None);
            // Two full periods plus slack; the beat thread checks the stop flag every 250ms.
            std::thread::sleep(Duration::from_millis(900));
            hb.stop();
            handle
        };
        handle.join().expect("beat thread joins cleanly");

        let beats = capture.beats.lock().unwrap().clone();
        assert!(beats.len() >= 2, "expected >=2 beats, got {beats:?}");
        for b in &beats {
            assert_eq!(b.iter, 7);
            assert!(
                (b.spent_usd - 23.457).abs() < 1e-9,
                "spend was {}",
                b.spent_usd
            );
            assert!(b.message.contains("alive"), "message was {}", b.message);
        }
        let spans = capture.spans.lock().unwrap().clone();
        assert_eq!(spans.len(), beats.len(), "one span per beat: {spans:?}");
        assert!(spans.iter().all(|n| n == "heartbeat"), "{spans:?}");
    }

    #[test]
    fn stop_before_the_first_period_emits_nothing() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let hb = Arc::new(Heartbeat::new());
        let handle = {
            let _guard = tracing::subscriber::set_default(subscriber);
            let handle = spawn(Arc::clone(&hb), Duration::from_secs(3600), None);
            hb.stop();
            handle
        };
        handle.join().expect("beat thread joins cleanly");
        assert!(capture.beats.lock().unwrap().is_empty());
    }

    #[test]
    fn record_clamps_a_nonsense_spend() {
        let hb = Heartbeat::new();
        hb.record(3, f64::NAN);
        assert_eq!(hb.spent_usd(), 0.0);
        hb.record(3, -5.0);
        assert_eq!(hb.spent_usd(), 0.0);
        hb.record(3, 1.2345);
        assert!((hb.spent_usd() - 1.234).abs() < 1e-9 || (hb.spent_usd() - 1.235).abs() < 1e-9);
        assert_eq!(hb.iter(), 3);
    }

    #[test]
    fn period_from_env_defaults_and_disables() {
        // The env var is process-global, so this test owns it start to finish.
        unsafe { std::env::remove_var("CRUCIBLE_HEARTBEAT_SECS") };
        assert_eq!(period_from_env(), Some(DEFAULT_PERIOD));
        unsafe { std::env::set_var("CRUCIBLE_HEARTBEAT_SECS", "5") };
        assert_eq!(period_from_env(), Some(Duration::from_secs(5)));
        unsafe { std::env::set_var("CRUCIBLE_HEARTBEAT_SECS", "0") };
        assert_eq!(period_from_env(), None);
        // A typo must not disable liveness reporting.
        unsafe { std::env::set_var("CRUCIBLE_HEARTBEAT_SECS", "nonsense") };
        assert_eq!(period_from_env(), Some(DEFAULT_PERIOD));
        unsafe { std::env::remove_var("CRUCIBLE_HEARTBEAT_SECS") };
    }

    /// The guard's Drop is the only cleanup the error paths get, so it must stop and join.
    #[test]
    fn dropping_the_guard_stops_and_joins_the_thread() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let hb = {
            let _guard = tracing::subscriber::set_default(subscriber);
            let guard = start(Duration::from_millis(200), None);
            let hb = guard.beat();
            hb.record(4, 1.5);
            std::thread::sleep(Duration::from_millis(500));
            drop(guard);
            hb
        };
        let after_drop = capture.beats.lock().unwrap().len();
        assert!(after_drop >= 1, "expected a beat before the drop");
        assert!(hb.stopped(), "the guard must set the stop flag");
        // Nothing beats after the join, however long we wait.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(capture.beats.lock().unwrap().len(), after_drop);
    }
}
