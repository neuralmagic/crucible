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

/// Env var naming the run a pod belongs to. A controller-dispatched run carries the controller's
/// run id, which is what the UI's `/runs/{id}` page and every report link key on.
pub const ENV_RUN_NAME: &str = "CRUCIBLE_RUN_NAME";

/// Env var carrying the name a person reads a run by (`benevolent-monkey`). Report text and pages
/// show it; links keep [`ENV_RUN_NAME`], the id.
pub const ENV_RUN_DISPLAY_NAME: &str = "CRUCIBLE_RUN_DISPLAY_NAME";

/// Env var carrying the controller's base URL as a pod reaches it. A run pod uploads its evidence
/// under `<url>/api/runs/{run}/evidence/{kind}` and a rerun fetches its parent's from there.
/// Absent means the pod has no controller to deliver to.
pub const ENV_CONTROLLER_URL: &str = "CRUCIBLE_CONTROLLER_URL";

/// Env var carrying the pod token (RFC-0004:C-RUN-EVIDENCE), projected from the pod's Secret: a
/// run pod's bearer for its run's evidence and its parent's, a turn pod's for its pack and
/// transcript.
pub const ENV_POD_TOKEN: &str = "CRUCIBLE_POD_TOKEN";

/// Env var carrying the turn pod's own name (downward API `metadata.name`), the `{pod}` path
/// segment its pack and transcript upload under.
pub const ENV_POD_NAME: &str = "CRUCIBLE_POD_NAME";
