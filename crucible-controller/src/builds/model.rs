//! The builds slice's row and query types.

use crate::wire_enum::wire_enum;

/// The lifecycle of one declared image build (`builds`). A closed enum, not a raw string, so a
/// state literal can't drift past the bookkeeping layer (the [`MlflowExportState`] discipline).
/// `pending` → `dispatched` → (`succeeded` | `failed` | `timed-out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum BuildState {
    /// A row exists (the scope declared this build) but nothing was dispatched yet.
    Pending,
    /// The backend accepted the build; `dispatch_id` identifies the Job/run reconcile polls.
    Dispatched,
    /// The image was pushed and its digest pinned onto the row (`digest_ref`) — the run unblocks.
    Succeeded,
    /// The build failed; `evidence_url` points at the build log. The blocked issue parks.
    Failed,
    /// The build exceeded its `timeout_secs` without going terminal; the blocked issue parks.
    TimedOut,
}

impl BuildState {
    /// A terminal state needs no further poll (succeeded pins the digest, failed/timed-out parks).
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            BuildState::Succeeded | BuildState::Failed | BuildState::TimedOut
        )
    }
}

/// Which backend runs a declared build. Stored as the TEXT `builds.backend`, matching the
/// manifest's `[build.<name>].backend` spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum BuildBackendKind {
    /// A detached rootless-buildah Job on the cluster.
    Cluster,
    /// A `workflow_dispatch` against a repo's GitHub Actions build workflow (M2).
    GithubActions,
}

/// One `builds` row to insert (a freshly declared build entering `pending`). The DB stamps
/// `created_at` and defaults `state`.
#[derive(Debug, Clone)]
pub struct NewBuild {
    /// The scope (pack) this build blocks.
    pub(crate) scope: Option<i64>,
    pub(crate) name: String,
    pub(crate) image: String,
    pub(crate) tag: String,
    pub(crate) context_digest: String,
    pub(crate) backend: BuildBackendKind,
    pub(crate) timeout_secs: i64,
}

/// One `builds` row read back in full, decoded into its strong `state`/`backend` types.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct BuildRow {
    pub(crate) id: i64,
    pub(crate) scope: Option<i64>,
    pub(crate) name: String,
    pub(crate) image: String,
    pub(crate) tag: String,
    pub(crate) context_digest: String,
    pub(crate) backend: BuildBackendKind,
    pub(crate) state: BuildState,
    pub(crate) dispatch_id: Option<String>,
    pub(crate) digest_ref: Option<String>,
    pub(crate) evidence_url: Option<String>,
    /// Failed dispatch attempts charged against this row (the reconcile driver parks after a small
    /// cap so a permanently-undispatchable build can't wedge the issue at `building`).
    pub(crate) dispatch_attempts: i64,
    pub(crate) timeout_secs: i64,
    pub(crate) created_at: String,
    pub(crate) dispatched_at: Option<String>,
    pub(crate) finished_at: Option<String>,
}

/// The `state=`/`backend=`/`issue=` filter set + paging for the builds ledger — shared by
/// `GET /api/builds` (and the builds UI page) so both call one
/// [`crate::builds::store::list_builds_page`]. Ordering is fixed newest-first (id DESC); builds carry no
/// sort choice today.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildQuery {
    pub(crate) state: Option<BuildState>,
    pub(crate) backend: Option<BuildBackendKind>,
    /// Restrict to one issue's builds (via `builds.scope → scopes.issue`).
    pub(crate) issue: Option<String>,
    pub(crate) limit: i64,
    pub(crate) offset: i64,
}

/// One builds-ledger row joined out to the issue it blocks (`builds.scope → scopes.issue →
/// issues.repo`), so the builds page can render the issue backlink + repo without a second lookup.
/// `issue_key`/`repo` are `None` for a build with no scope (a CLI-invoked build the controller
/// recorded without an issue).
#[derive(Debug, Clone, PartialEq)]
pub struct BuildListRow {
    pub(crate) row: BuildRow,
    pub(crate) issue_key: Option<String>,
    pub(crate) repo: Option<String>,
}

wire_enum!(BuildState, "build state", both, {
    BuildState::Pending => "pending",
    BuildState::Dispatched => "dispatched",
    BuildState::Succeeded => "succeeded",
    BuildState::Failed => "failed",
    BuildState::TimedOut => "timed-out",
});

// Matches the manifest's `[build.<name>].backend` spelling.
wire_enum!(BuildBackendKind, "build backend", both, {
    BuildBackendKind::Cluster => "cluster",
    BuildBackendKind::GithubActions => "github-actions",
});
