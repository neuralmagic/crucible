//! The admission ledger: a durable record of operator inputs (approvals, denials, steers,
//! rescopes) that resume replays so a restarted run neither drops nor double-applies them.

/// Handle to an open `state/admissions.jsonl`. Carried on
/// [`crate::loop_driver::LoopRuntime`] so the resume path can thread it without changing
/// the loop boundary. No writer exists; every runtime carries `None`.
#[allow(dead_code)]
pub(crate) struct AdmissionLedger;
