//! The shared stdout marker literals, the `managed-by` label, and the ingest env-var names.
//! `crucible` and `crucible-controller` both use these single definitions, so the two sides
//! cannot drift.

/// Prefix of the single-line verdict marker `crucible rank-grounded --marker` prints last, so the
/// verdict JSON survives the podman/git noise ahead of it in the turn pod's logs.
pub const VERDICT_MARKER: &str = "CRUCIBLE_VERDICT:";

/// Prefix of the single-line scope-report marker `crucible scope --propose --json --marker` prints
/// last, the terminal result of a scope turn.
pub const SCOPE_REPORT_MARKER: &str = "CRUCIBLE_SCOPE_REPORT:";

/// Prefix of the interim per-round progress marker a scope turn prints at each refine-round
/// boundary. This is a progress hint rather than a result; it is relayed live and never persisted.
pub const SCOPE_PROGRESS_MARKER: &str = "CRUCIBLE_SCOPE_PROGRESS:";

/// Prefix of the within-round activity marker the scope turn prints while an agent turn streams
/// (tool calls, text snippets, usage samples). Relayed live and never persisted.
pub const SCOPE_ACTIVITY_MARKER: &str = "CRUCIBLE_SCOPE_ACTIVITY:";

/// Prefix of the within-turn activity marker the grounded ranking turn prints while the agent
/// streams (stage narration, tool calls, agent stderr). Relayed live and never persisted.
pub const RANK_ACTIVITY_MARKER: &str = "CRUCIBLE_RANK_ACTIVITY:";

/// Prefix of the transcript marker a scope turn prints just before its report marker: base64 of
/// the gzipped session NDJSON the propose/refine/adversary turns streamed.
pub const SCOPE_TRANSCRIPT_MARKER: &str = "CRUCIBLE_SCOPE_TRANSCRIPT:";

/// Prefix of the pack marker a SURVIVING scope turn prints before its report marker: base64 of the
/// gzipped tar of the frozen pack directory (or an `{"error":…}` payload when the pack couldn't be
/// emitted whole). With `scope_executor = pod` this marker is the only channel that returns the
/// pack to the controller.
pub const SCOPE_PACK_MARKER: &str = "CRUCIBLE_SCOPE_PACK:";

/// The fragment identifying the loop-run wrapper's session delimiter line
/// (`=== SESSION (rc=$rc) ===`): everything after the LAST such line in the pod's logs is the
/// `cat` of the run's `state/session.jsonl`.
pub const RUN_SESSION_DELIMITER: &str = "SESSION (rc=";

/// The `managed-by` label key every rendered loop/turn pod carries, so `crucible ps` and the
/// controller's pod-completion watch can select the pods this system owns.
pub const MANAGED_BY_KEY: &str = "app.kubernetes.io/managed-by";

/// The `managed-by` label value.
pub const MANAGED_BY_VALUE: &str = "crucible";

/// The `key=value` label selector form of the `managed-by` label, for kube list/watch calls.
pub const MANAGED_BY_SELECTOR: &str = "app.kubernetes.io/managed-by=crucible";

/// Env var carrying the controller's Tier 2 ingest base URL into a turn pod (Tier 2 ingest).
/// Absent means no drop-box is available, and the engine falls back to marker emission.
pub const ENV_INGEST_URL: &str = "CRUCIBLE_INGEST_URL";

/// Env var carrying the filesystem path of the pod's projected ingest ServiceAccount token, which
/// the engine reads and sends as the bearer to the ingest endpoint.
pub const ENV_INGEST_TOKEN_PATH: &str = "CRUCIBLE_INGEST_TOKEN_PATH";

/// Env var carrying the turn pod's own name (downward API `metadata.name`), which the engine uses as
/// the `{pod}` path segment of its ingest POST. It MUST equal the token's bound-pod claim, so the
/// downward-API value and the TokenReview `authentication.kubernetes.io/pod-name` claim are the same
/// pod name by construction.
pub const ENV_POD_NAME: &str = "CRUCIBLE_POD_NAME";

/// Env var naming the suspended pod whose `run-session` and `run-workspace` artifacts a resumed
/// pod restores before the engine starts. Absent = a fresh run with nothing to restore.
pub const ENV_RESUME_OF: &str = "CRUCIBLE_RESUME_OF";

/// The audience the ingest projected ServiceAccount token is minted for. The ingest extractor
/// requires TokenReview to echo this audience, so the token is useless against the kube API or any
/// other service.
pub const INGEST_TOKEN_AUDIENCE: &str = "crucible-ingest";

/// The TokenReview claim carrying the bound pod's name; the ingest extractor requires it to equal
/// the `{pod}` path segment, so binding the token to the pod also scopes it to the turn.
pub const INGEST_POD_NAME_CLAIM: &str = "authentication.kubernetes.io/pod-name";
