//! Process-wide stop signal.

use std::sync::atomic::AtomicBool;

pub(crate) static STOP: AtomicBool = AtomicBool::new(false);
