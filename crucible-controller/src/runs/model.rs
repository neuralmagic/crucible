//! The run slice's row, dispatch and result types.

use crate::model::{SortDir, sanitize_key};
use crate::wire_enum::wire_enum;

/// A task's name inside a compiled plan. Serializes as the bare string it is on the wire.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct TaskName(String);

impl TaskName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TaskName {
    fn from(name: &str) -> Self {
        Self(name.to_string())
    }
}

impl From<String> for TaskName {
    fn from(name: String) -> Self {
        Self(name)
    }
}

impl std::fmt::Display for TaskName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The live run behind a `running` issue, everything the pod-completion edge needs to fold its
/// outcome in ([`crate::runs::completion::complete_run`]): the run id, its scope (for the FK), the
/// evidence pointer the run published (a local path or S3 URI, if the launcher recorded one), and
/// its pod name.
#[derive(Debug, Clone, PartialEq)]
pub struct RunningRun {
    pub(crate) run_id: String,
    pub(crate) scope: Option<i64>,
    pub(crate) session_uri: Option<String>,
    pub(crate) pod: Option<String>,
}

/// A Tier 2 drop-box pointer to persist (`pod_artifacts`): the content digest of one artifact a
/// turn pod POSTed. Pointer only — the bytes live in the artifact chunk tables
/// ([`crate::runs::blob_store`]).
#[derive(Debug, Clone, PartialEq)]
pub struct NewPodArtifact {
    /// The uploading pod (the `{pod}` path segment).
    pub(crate) pod: String,
    /// The artifact-kind wire spelling (`scope-pack` / …), the `{kind}` path segment.
    pub(crate) kind: String,
    /// `sha256:<hex>` of the (compressed) bytes — the content address dedup + fold validate on.
    pub(crate) digest: String,
    /// The (compressed) size the approval recorded.
    pub(crate) bytes: i64,
}

/// One `pod_artifacts` row read back: the drop-box pointer the fold resolves and validates against
/// the Tier 1 manifest digest.
#[derive(Debug, Clone, PartialEq)]
pub struct PodArtifactRow {
    pub(crate) pod: String,
    pub(crate) kind: String,
    pub(crate) digest: String,
    pub(crate) bytes: i64,
    pub(crate) created_at: String,
}

/// The lifecycle of one run's MLflow export (`mlflow_exports`). A closed enum, not a raw string,
/// so the sweep can't drift a state literal past the bookkeeping layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MlflowExportState {
    /// A sweep has claimed the run and is attempting the export (or a prior attempt died mid-flight).
    Pending,
    /// The run + its traces landed on the tracking server; the idempotency stop — never re-exported.
    Exported,
    /// The last attempt failed; retried on the next sweep with the error preserved.
    Failed,
}

impl std::str::FromStr for MlflowExportState {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Ok(MlflowExportState::Pending),
            "exported" => Ok(MlflowExportState::Exported),
            "failed" => Ok(MlflowExportState::Failed),
            other => Err(format!("unknown mlflow export state: {other}")),
        }
    }
}

/// One `mlflow_exports` row read back — the export bookkeeping for one run.
#[derive(Debug, Clone, PartialEq)]
pub struct MlflowExportRow {
    pub(crate) run_id: String,
    pub(crate) state: MlflowExportState,
    pub(crate) mlflow_run_id: Option<String>,
    pub(crate) experiment_id: Option<String>,
    pub(crate) attempts: i64,
    pub(crate) error: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

/// Where a run's engine ran: a controller-owned work pod, or a supervised subprocess on the
/// controller's own machine (the config-gated local mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunDispatch {
    Pod,
    Local,
}

impl RunDispatch {
    pub fn as_str(self) -> &'static str {
        match self {
            RunDispatch::Pod => "pod",
            RunDispatch::Local => "local",
        }
    }

    /// Decode a stored value. An unrecognized one reads as `Pod`, the pre-local-mode shape of
    /// every row.
    pub fn parse(s: &str) -> Self {
        match s {
            "local" => RunDispatch::Local,
            _ => RunDispatch::Pod,
        }
    }
}

impl sqlx::Type<sqlx::Postgres> for RunDispatch {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }
    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <String as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Postgres> for RunDispatch {
    fn decode(
        value: sqlx::postgres::PgValueRef<'r>,
    ) -> ::core::result::Result<Self, sqlx::error::BoxDynError> {
        Ok(RunDispatch::parse(<&str as sqlx::Decode<
            'r,
            sqlx::Postgres,
        >>::decode(value)?))
    }
}

/// Which cluster a run's engine was dispatched onto, and the namespace that cluster resolved its
/// pod into. A spoke's namespace comes from its kubeconfig context, so it is not derivable from
/// controller config and has to be recorded at dispatch.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RunLocation {
    pub cluster: String,
    pub namespace: Option<String>,
}

impl RunLocation {
    /// The controller's own cluster, namespace unrecorded — the shape of every row written before
    /// the columns existed, and the default a local run keeps.
    pub fn hub() -> Self {
        RunLocation {
            cluster: crate::runs::clusters::HUB_CLUSTER.to_string(),
            namespace: None,
        }
    }

    pub fn new(cluster: impl Into<String>, namespace: Option<String>) -> Self {
        RunLocation {
            cluster: cluster.into(),
            namespace,
        }
    }

    /// Whether this run sits on the controller's own cluster, where the pod IP is directly
    /// dialable.
    pub fn is_hub(&self) -> bool {
        self.cluster == crate::runs::clusters::HUB_CLUSTER
    }
}

/// A `runs` row to record (per-run summary; per-candidate detail lives in
/// [`NewCandidate`]). `status` is the run's own lifecycle (running/done/…), left as a string
/// here.
#[derive(Debug, Clone)]
pub struct NewRun {
    pub run_id: String,
    pub scope: Option<i64>,
    /// The issue the run belongs to. `None` lets the insert derive it from `scope`.
    pub issue: Option<String>,
    pub identity_digest: Option<String>,
    pub status: String,
    pub pod: Option<String>,
    pub session_uri: Option<String>,
    pub best_score: Option<f64>,
    pub cost_usd: Option<f64>,
}

/// A `candidates` row — one wide-round lane or deep-loop iteration (per-candidate
/// granularity, the source of wide-round resume and the full provenance graph).
#[derive(Debug, Clone)]
pub struct NewCandidate {
    pub run_id: String,
    pub kind: Option<String>,
    pub lane: Option<i64>,
    pub iter: Option<i64>,
    pub score: Option<f64>,
    pub decision: Option<String>,
    pub worktree: Option<String>,
    pub sandbox: Option<String>,
    pub pr_url: Option<String>,
    /// The head branch this candidate's PR was opened from (`autoresearch/<run_id>/<candidate>`).
    pub branch: Option<String>,
}

/// One `runs` row read back in full ([`crate::daemon::rebuild`]'s row-by-row diff needs every column;
/// [`NewRun`] is write-only).
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Run {
    pub run_id: String,
    /// Where the engine ran. Written once at dispatch; the completion ingest never touches it.
    pub dispatch: RunDispatch,
    /// Which cluster and namespace it ran on. Written once at dispatch, beside `dispatch`.
    #[sqlx(flatten)]
    pub location: RunLocation,
    pub(crate) scope: Option<i64>,
    pub(crate) issue: Option<String>,
    pub identity_digest: Option<String>,
    pub status: String,
    pub(crate) pod: Option<String>,
    pub(crate) session_uri: Option<String>,
    pub best_score: Option<f64>,
    pub cost_usd: Option<f64>,
    /// The sandbox image the run was preflighted on (C-IDENTITY). Written once at dispatch.
    #[sqlx(flatten)]
    pub image: RunImage,
}

/// A run's image provenance: what the manifest named, what the catalog resolved it to, and
/// whether the launch went through on the unverified-image override.
#[derive(Debug, Clone, Default, PartialEq, sqlx::FromRow)]
pub struct RunImage {
    #[sqlx(rename = "image_ref")]
    pub reference: Option<String>,
    #[sqlx(rename = "image_digest")]
    pub digest: Option<String>,
    pub capability_digest: Option<String>,
    #[sqlx(rename = "image_override")]
    pub overridden: bool,
}

/// One `candidates` row read back in full. The table has no primary key; rebuild/drift key on
/// `(run_id, kind, lane, iter)`, which a run never repeats.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Candidate {
    pub(crate) run_id: String,
    pub(crate) kind: Option<String>,
    pub(crate) lane: Option<i64>,
    pub(crate) iter: Option<i64>,
    pub score: Option<f64>,
    pub decision: Option<String>,
    pub(crate) worktree: Option<String>,
    pub(crate) sandbox: Option<String>,
    pub(crate) pr_url: Option<String>,
    /// The head branch this candidate's PR was opened from (`autoresearch/<run_id>/<candidate>`).
    pub(crate) branch: Option<String>,
}

/// One admitted work graph, as folded from a run's `plan_admitted` event. `graph_json` is the
/// event's `tasks` array verbatim; the API layer re-parses it into typed nodes.
#[derive(Debug, Clone, PartialEq)]
pub struct RunPlan {
    pub plan_version: i64,
    pub graph_json: String,
}

/// One terminal task attempt, folded from a run's `task_result` event.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskResult {
    pub iter: i64,
    pub task: String,
    /// `pass`/`fail`/`transport`/`skipped`/`blocked`/`truncated` (the wire's own vocabulary — the
    /// controller stores it as written rather than narrowing an enum it doesn't own).
    pub status: String,
    pub note: String,
    pub cost_usd: Option<f64>,
    pub secs: Option<f64>,
    /// Present exactly when `status` is `blocked`.
    pub blocked: Option<crucible_contract::TaskBlocked>,
}

/// The sort column for `GET /api/runs` (the leaderboard). `Created` orders by `run_id`, whose
/// leading `YYYYMMDDTHHMMSSZ` stamp is the run's creation time — reverse-lexical puts the newest
/// run on top (there is no `created_at` column on `runs`; the id carries the time).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, strum::EnumIter)]
pub enum RunSort {
    BestScore,
    Cost,
    #[default]
    Created,
}

impl RunSort {
    /// The `runs`/join column this key orders by (never string-interpolated from user input — the
    /// match closes the set, keeping the dynamic ORDER BY injection-safe).
    pub(crate) fn order_col(self) -> &'static str {
        match self {
            RunSort::BestScore => "r.best_score",
            RunSort::Cost => "r.cost_usd",
            // The id is time-first, so lexical order == chronological order.
            RunSort::Created => "r.run_id",
        }
    }
}

/// The `status=`/`repo=`/`dispatch_target=` filter set + sorting and paging for the runs leaderboard — shared by
/// `GET /api/runs` (and any future UI runs page) so both call one [`crate::runs::store::list_runs_page`].
#[derive(Debug, Clone, PartialEq)]
pub struct RunQuery {
    pub(crate) status: Option<String>,
    pub(crate) repo: Option<String>,
    pub(crate) dispatch_target: Option<String>,
    /// `autoresearch` keeps only scoped runs, `playbook` only launch-keyed ones; absent means both.
    pub(crate) kind: Option<RunKindFilter>,
    pub(crate) sort: RunSort,
    pub(crate) dir: SortDir,
    pub(crate) limit: i64,
    pub(crate) offset: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum RunKindFilter {
    Autoresearch,
    Playbook,
}

wire_enum!(RunKindFilter, "run kind", both, {
    RunKindFilter::Autoresearch => "autoresearch",
    RunKindFilter::Playbook => "playbook",
});

/// One leaderboard row: a `runs` row joined out to the issue key + repo it belongs to (via
/// `runs.scope → scopes.issue → issues.repo`), with the creation stamp derived from the run id.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub(crate) run_id: String,
    pub(crate) issue_key: Option<String>,
    pub(crate) repo: Option<String>,
    pub(crate) status: String,
    pub(crate) best_score: Option<f64>,
    pub(crate) cost_usd: Option<f64>,
    /// The kept-candidate PR url for this run, or `None` when the run kept no PR (mirrors the issue
    /// surfaces' PR chip; lets the runs leaderboard link straight out to the opened draft).
    pub(crate) pr_url: Option<String>,
    /// Task attempts that ended `transport`: lost to infrastructure, not to a verdict.
    pub(crate) transport_losses: i64,
    /// `YYYYMMDDTHHMMSSZ` parsed off the run id (an ISO-ish RFC3339 UTC stamp), or `None` when the
    /// id isn't a time-first run id (a bare local goal-slug id).
    pub(crate) created: Option<String>,
}

/// Parse the leading `YYYYMMDDTHHMMSSZ` stamp of a time-first run id into an RFC3339 UTC timestamp
/// (`YYYY-MM-DDTHH:MM:SSZ`), or `None` when the id doesn't start with one.
pub(crate) fn run_id_created(run_id: &str) -> Option<String> {
    let b = run_id.as_bytes();
    if b.len() < 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| run_id[r].bytes().all(|c| c.is_ascii_digit());
    if !digits(0..8) || !digits(9..15) {
        return None;
    }
    Some(format!(
        "{}-{}-{}T{}:{}:{}Z",
        &run_id[0..4],
        &run_id[4..6],
        &run_id[6..8],
        &run_id[9..11],
        &run_id[11..13],
        &run_id[13..15],
    ))
}

/// Task-name fragments that mark the step spending an output of a given kind; a compiled plan
/// declares no such link, so the attachment goes by pack vocabulary. Ordered: the first fragment a
/// plan carries wins.
const PRODUCERS: &[(&str, &[&str])] = &[
    ("draft-pr", &["publish", "report", "pr"]),
    ("tracker-comment", &["publish", "report", "comment"]),
    ("chat-message", &["publish", "report", "notify"]),
    ("image-push", &["push", "build"]),
    ("deploy", &["deploy", "rollout"]),
    ("workflow-dispatch", &["dispatch", "trigger"]),
];

/// The task an output of `kind` hangs off, chosen from the plan's task names. `None` attaches it
/// to the graph's sink instead; a bound is never dropped.
pub fn producing_task<'a>(kind: &str, tasks: &'a [TaskName]) -> Option<&'a TaskName> {
    let fragments = PRODUCERS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, f)| *f)?;
    fragments.iter().find_map(|fragment| {
        tasks
            .iter()
            .find(|name| name.as_str().to_ascii_lowercase().contains(fragment))
    })
}

/// A run id for a launched pod: the sanitized key plus a second-resolution stamp (unique per launch
/// of an issue; a re-launch after a crash gets a fresh id). Also the id `db adopt --issue` mints,
/// so adopted and dispatched runs for one issue share a shape.
pub fn new_run_id(key: &str) -> String {
    format!(
        "{}-{}",
        sanitize_key(key),
        jiff::Timestamp::now().as_second()
    )
}

wire_enum!(RunSort, "run sort key", parse_only, {
    RunSort::BestScore => "best_score",
    RunSort::Cost => "cost",
    RunSort::Created => "created",
});
