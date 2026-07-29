//! Pluggable human-in-the-loop approval. The broker (server-side) holds the forge
//! token / control channel; the agent never does. A judge-changing grant opens an approval ask
//! and the loop continues or parks until it resolves; humans approve a batch out-of-band.

use crate::types::TraceId;

/// Which backend surfaces a request that needs approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalChannel {
    /// Headless default: push an `agentic/<run_id>` branch + open a **draft PR** on the (fork)
    /// repo; approve via a `/approve-capture` slash-command comment or marking it ready. Gated
    /// by CODEOWNERS, and it doubles as the durable state record.
    DraftPr,
    /// Attended: surface the ask over the control bridge; an operator approves with a keystroke
    /// (the steer/stop path).
    ControlChannel,
}

/// The ask presented to a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub(crate) trace_id: TraceId,
    /// One-line description of what's being requested (params / regime change).
    pub(crate) summary: String,
    pub(crate) est_gpus: u32,
}

/// Where an opened approval stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
}

/// Open an approval ask and poll it. Returns an opaque handle (a PR number, a control-channel
/// request id) the broker stores and polls. Concrete backends (draft-PR, control-bridge) are
/// later slices.
pub trait ApprovalBackend {
    fn open(&self, req: &ApprovalRequest) -> anyhow::Result<String>;
    fn poll(&self, handle: &str) -> anyhow::Result<ApprovalState>;
}
