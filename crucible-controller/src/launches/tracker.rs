//! Tracker-agnostic issue-tracker types: the normalized issue/hit shapes and the create-side
//! [`IssueEmitter`] boundary. Tracker-specific code (Jira REST, GitHub REST) maps its native
//! payloads onto these; nothing here assumes a specific tracker, instance URL, or project naming.
//!
//! Issues are named by a tracker-local `id` (`PROJ-123`, `owner/repo#42`), which each tracker maps
//! to the namespaced `issues.key` form that [`crate::issues::model::InputKind::from_parts`] parses back.

use crate::daemon::queue::BoxFuture;
use crate::wire_enum::wire_enum;
use anyhow::Result;
use std::collections::BTreeMap;

/// One upstream issue, normalized. `body` is plain text — impls that only get rich text must
/// down-convert. Anything outside the common fields rides in `extra_fields` so the mapping is
/// lossless without per-tracker types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerIssue {
    /// Tracker-local id (`PROJ-123`, `owner/repo#42`).
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) body: Option<String>,
    pub(crate) labels: Vec<String>,
    /// Human-facing link to the issue.
    pub(crate) url: String,
    /// Last-modified timestamp in the tracker's native string form; feeds the search watermark.
    pub(crate) updated_at: String,
    /// `false` once closed/resolved upstream.
    pub(crate) open: bool,
    /// Tracker-specific fields the common shape has no slot for (issue type, priority, epic link,
    /// …), keyed by the tracker's own field names.
    pub(crate) extra_fields: BTreeMap<String, serde_json::Value>,
}

/// A search hit: enough to decide "changed since last sweep", hydrated lazily via a full issue
/// fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerHit {
    pub(crate) id: String,
    /// The sweep's next watermark is the max of these.
    pub(crate) updated_at: String,
}

/// Where an emitted issue sits in the tracker's hierarchy. Deliberately not "epic"/"story":
/// each impl maps these onto its own model (Jira: epic + child via `parent`; GitHub: umbrella
/// tracking issue + task-listed issues).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmittedKind {
    /// Groups an experiment's review trail (the "epic").
    Container,
    /// One reviewable work item under a container.
    Task,
}

/// An issue to emit. `body` is plain text — impls up-convert. `extra_fields` carries
/// deploy-config extras (custom fields etc.) merged into whatever the impl builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTrackerIssue {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) labels: Vec<String>,
    pub(crate) kind: EmittedKind,
    /// Tracker-local id of the container this belongs under; `None` for the container itself.
    pub(crate) parent: Option<String>,
    pub(crate) extra_fields: BTreeMap<String, serde_json::Value>,
}

/// The create-side tracker capability, kept separate from the read/comment primitives so a
/// read-only deploy (or a tracker without a sane creation model) simply doesn't provide one.
pub trait IssueEmitter: Send + Sync {
    /// Create the issue; returns its tracker-local id (feedable to the fetch/comment primitives and
    /// usable as a child's `parent`).
    fn create_issue(&self, issue: NewTrackerIssue) -> BoxFuture<Result<String>>;
    /// Rewrite an already-emitted issue in place (same ledger identity, refreshed content). A
    /// re-emission must patch, not skip: corrected titles/links otherwise never reach the tracker.
    fn update_issue(&self, id: &str, issue: NewTrackerIssue) -> BoxFuture<Result<()>>;
    /// Attach a web link to the issue's links panel (Jira remote link). Keyed by `url`
    /// server-side (`globalId`), so replays update the existing link instead of stacking dupes.
    fn add_web_link(&self, id: &str, url: &str, title: &str) -> BoxFuture<Result<()>>;
}

/// Why a tracker rejected a watch query before it was stored. These are validation failures with
/// a reader at the API boundary, so each variant owns the message shown in the 422 response.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TrackerQueryError {
    #[error("watch query is empty")]
    Empty,
    #[error("watch query must not contain ORDER BY (ordering is fixed to `updated ASC`)")]
    OwnOrdering,
}

/// Which tracker a watch sweeps. One entry per tracker the controller can be configured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, strum::EnumIter)]
pub(crate) enum TrackerKind {
    Jira,
}

wire_enum!(TrackerKind, "tracker", both, {
    TrackerKind::Jira => "jira",
});

/// The read-side search capability a watch sweeps through: a query in the tracker's native
/// language at or after a watermark, hits oldest-first. Kept apart from [`IssueEmitter`] so a
/// deploy with read-only credentials can still watch.
pub trait TrackerSearch: Send + Sync {
    /// Refuse a query the sweep could never run (its own ordering, a syntax the impl rejects up
    /// front), before it is stored.
    fn check_query(&self, query: &str) -> Result<(), TrackerQueryError>;
    /// Hits at or after `watermark` (the tracker's native update-time string, or RFC 3339),
    /// oldest-first; the whole query when `None`.
    fn search(&self, query: &str, watermark: Option<&str>) -> BoxFuture<Result<Vec<TrackerHit>>>;
}

/// The trackers this deployment holds credentials for, by kind. A watch save refuses a kind that
/// is not here, and the sweep skips one that went away.
#[derive(Clone, Default)]
pub struct Trackers {
    by_kind: BTreeMap<TrackerKind, std::sync::Arc<dyn TrackerSearch>>,
}

impl Trackers {
    pub(crate) fn with(
        mut self,
        kind: TrackerKind,
        tracker: std::sync::Arc<dyn TrackerSearch>,
    ) -> Self {
        self.by_kind.insert(kind, tracker);
        self
    }

    pub(crate) fn get(&self, kind: TrackerKind) -> Option<&std::sync::Arc<dyn TrackerSearch>> {
        self.by_kind.get(&kind)
    }

    /// The kinds a watch may name here.
    pub(crate) fn configured(&self) -> Vec<TrackerKind> {
        self.by_kind.keys().copied().collect()
    }
}
