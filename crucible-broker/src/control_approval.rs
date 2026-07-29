//! Control-channel approval: the attended path.
//!
//! Unlike the draft-PR backend, this opens nothing external: it records the ask and returns a
//! synthetic handle. An operator watching the run (controller SPA `/runs/:id`, or a raw
//! `kubectl logs -f`) approves over the loop's control bridge directly, which fires the
//! re-scope (the loop already recorded the regime from the agent's marker). So the broker
//! never polls this backend (the loop's bridge resolves it) and `poll` simply reports
//! `Pending`. This needs no forge token, which makes it the token-free way to exercise the
//! full park → approve → resume chain.

use crate::approval::{ApprovalBackend, ApprovalRequest, ApprovalState};
use anyhow::Result;

/// A no-forge approval surface resolved by the operator over the loop's control bridge.
pub struct ControlApproval;

impl ApprovalBackend for ControlApproval {
    fn open(&self, req: &ApprovalRequest) -> Result<String> {
        // The handle is just the trace_id; the operator's `approve` over the control bridge carries
        // the regime (the loop recorded it from the agent's marker), so no external state is kept.
        Ok(format!("control:{}", req.trace_id.0))
    }

    fn poll(&self, _handle: &str) -> Result<ApprovalState> {
        // The control bridge resolves the approval directly (operator keystroke → rescope), so the
        // broker never waits on this backend.
        Ok(ApprovalState::Pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TraceId;

    #[test]
    fn open_returns_a_control_handle_and_poll_is_pending() {
        let a = ControlApproval;
        let req = ApprovalRequest {
            trace_id: TraceId("model=Qwen/Qwen3-0.6B;c=48".into()),
            summary: "re-scope".into(),
            est_gpus: 1,
        };
        let handle = a.open(&req).unwrap();
        assert_eq!(handle, "control:model=Qwen/Qwen3-0.6B;c=48");
        assert_eq!(a.poll(&handle).unwrap(), ApprovalState::Pending);
    }
}
