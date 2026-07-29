//! Server-side capture admission + submission: is there GPU headroom for a capture right now,
//! and if so, submit it. The agent never sees this; it asks for a trace and the broker decides.
//!
//! Headroom is the `gpu-check` signal (allocatable − Σrequested across nodes) with a reserve
//! margin so a capture never grabs the last idle GPU out from under an interactive user
//! (admission logic / `request-trace-tooling.md` §5). GPU spend stays gated, like `order-capture
//! --execute`.

use crate::types::TraceParams;

/// A capture's resource ask, for the headroom check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuNeed {
    pub(crate) gpus: u32,
}

/// The admission verdict for one capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// Headroom exists (with the reserve margin): submit the capture.
    Admit,
    /// No headroom now: defer + retry on backoff (the loop continues on fallback knobs rather
    /// than blocking). `reason` is operator-facing.
    Defer { reason: String },
}

/// The capture backend: check headroom, and (when admitted) submit the GPU capture. Both run
/// server-side; the sandboxed agent reaches neither the cluster nor a Kueue client.
pub trait Admission {
    /// Whether a capture may be submitted now. Opportunistic and **advisory**: between the check
    /// and the pod scheduling another workload can claim the GPU, so callers must tolerate a
    /// `Pending`/`Unschedulable` capture (capture admission).
    fn check(&self, need: GpuNeed) -> anyhow::Result<AdmissionDecision>;

    /// Submit the capture for `params` and return a job handle. GPU spend is gated (see the
    /// concrete backend); a dry run returns a descriptive handle without applying anything.
    fn submit(&self, params: &TraceParams) -> anyhow::Result<String>;
}

/// Pure headroom policy: admit a capture needing `need` GPUs only if free capacity stays at or
/// above `reserve` afterward. Split from the cluster query so it is unit-testable.
pub(crate) fn decide(free_gpus: u32, need: GpuNeed, reserve: u32) -> AdmissionDecision {
    if free_gpus >= need.gpus + reserve {
        AdmissionDecision::Admit
    } else {
        AdmissionDecision::Defer {
            reason: format!(
                "{free_gpus} GPU(s) free; need {} + {reserve} reserve",
                need.gpus
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_only_above_the_reserve() {
        // 8 free, need 1, reserve 2 => 8 >= 3 => admit.
        assert_eq!(decide(8, GpuNeed { gpus: 1 }, 2), AdmissionDecision::Admit);
        // 2 free, need 1, reserve 2 => 2 < 3 => defer (don't grab the last idle GPU).
        assert!(matches!(
            decide(2, GpuNeed { gpus: 1 }, 2),
            AdmissionDecision::Defer { .. }
        ));
        // exactly at the line admits.
        assert_eq!(decide(3, GpuNeed { gpus: 1 }, 2), AdmissionDecision::Admit);
    }
}
