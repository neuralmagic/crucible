//! Process-wide stop signal and exit-time flush hooks.

use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

pub(crate) static STOP: AtomicBool = AtomicBool::new(false);

/// Flush hooks the run's buffered writers register, called synchronously by the signal
/// handlers right before `std::process::exit` — the one path that skips every Drop. A signal
/// handler cannot be handed run context, so this is the one piece of process-wide state that
/// cannot be threaded; everything a hook closes is run-scoped and idempotent.
static FLUSH: OnceLock<Mutex<Vec<FlushHook>>> = OnceLock::new();

/// One registered exit-time flush.
type FlushHook = Box<dyn Fn() + Send>;

/// Register `f` to run before an exit-signal leaves the process.
pub(crate) fn flush_before_exit(f: FlushHook) {
    FLUSH
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(f);
}

/// Run every registered flush hook. Safe to call with nothing registered.
pub(crate) fn flush_all_before_exit() {
    if let Some(hooks) = FLUSH.get() {
        for f in hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
        {
            f();
        }
    }
}
