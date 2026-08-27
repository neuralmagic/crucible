//! The turn-result wire contract.
//!
//! One crate that `crucible`, `crucible-controller`, and `crucible-broker` all depend on, so the
//! termination-message envelope, the ingest bodies, and the session-log types are serde
//! round-trips of the *same* types on both sides, which makes schema drift a compile error.
//! The crate depends on serde only, with no async runtime and no kube client.

pub mod admission;
pub mod artifact;
pub mod ask;
pub mod envelope;
pub mod event;
pub mod identity;
pub mod json;
pub mod markers;
pub mod refine;
pub mod report;
pub mod scope;
pub mod session;
pub mod tier;
pub mod verdict;

/// The controller/engine contract version, as semver. Bump it on any change to a typed document
/// that crosses the controller/engine boundary: the termination envelope, the ingest bodies, the
/// admission and session wire types, the identity digests. The engine prints it via
/// `crucible --contract-version` and the runtime image carries it as the
/// `io.crucible.contract-version` OCI label, so a deployed image can be matched against the
/// controller it talks to without a probe.
pub const CONTRACT_VERSION: &str = "1.2.0";

pub use admission::{
    ADMISSION_WIRE_VERSION, AdmissionEvent, AdmissionKey, AdmissionOutcome, AdmittedInput,
    SteerSource,
};
pub use artifact::{
    ArtifactKind, ArtifactRef, IngestError, IngestPath, IngestResponse, content_digest,
};
pub use ask::{Ask, AskKey, AskKeyError};
pub use envelope::{Envelope, EnvelopeKind, SCHEMA_VERSION, TERMINATION_MESSAGE_CAP, Usage};
pub use event::{AgentEvent, ModelUsage, RawStream, Tokens};
pub use identity::{
    ComponentIdentity, FORMAT_VERSION as IDENTITY_FORMAT_VERSION, RigIdentity, RunIdentity,
};
pub use markers::{
    ENV_INGEST_TOKEN_PATH, ENV_INGEST_URL, ENV_POD_NAME, INGEST_POD_NAME_CLAIM,
    INGEST_TOKEN_AUDIENCE, MANAGED_BY_KEY, MANAGED_BY_SELECTOR, MANAGED_BY_VALUE,
    RANK_ACTIVITY_MARKER, RUN_SESSION_DELIMITER, SCOPE_ACTIVITY_MARKER, SCOPE_PACK_MARKER,
    SCOPE_PROGRESS_MARKER, SCOPE_REPORT_MARKER, SCOPE_TRANSCRIPT_MARKER, VERDICT_MARKER,
};
pub use refine::{
    Attack, AttackKind, ControlEvidence, FailureEvidence, ReadingEvidence, RoundKind, RoundOutcome,
    RoundRecord, SelftestEvidence, parse_rounds, render_rounds_json,
};
pub use report::{REPORT_FILE, ReportResult, RunReport, TaskReport};
pub use scope::{ScopeReport, StageName, StageResult};
pub use session::{PrLinkWire, RowWire, SessionEvent, WIRE_VERSION, decode, encode};
pub use tier::{Disposition, Tier, TierParseError};
pub use verdict::{GroundedErrorKind, GroundedVerdict};
