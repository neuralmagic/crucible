//! The issue slice's row and query types.

use crate::model::{ParkReason, ParkedBy, SortDir, Status};
use crate::wire_enum::wire_enum;

use anyhow::{Context, Result};
use crucible_contract::Tier;

/// The source an issue came from, persisted via `issues.input_kind` plus the existing
/// `key`/`repo` columns. `Unknown` is the inert fallback for a tag this binary doesn't
/// recognize, so an unfamiliar row degrades instead of failing the read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputKind {
    /// `key` = `"owner/repo#N"`.
    GitHub {
        owner: String,
        repo: String,
        number: u64,
    },
    /// `key` = `"scenario:{id}"`, a human-adopted free-text problem framing.
    Scenario { id: String },
    /// `key` = `"jira:{site}:{PROJ-N}"`, a human-adopted Jira issue. The controller fetches the
    /// issue's title/body once at adopt time and stores them like a scenario's — the loop never
    /// touches Jira, so this behaves as a non-upstream adopted item (no live re-fetch in v1).
    Jira {
        site: String,
        project: String,
        number: u64,
    },
    /// `key` = `"playbook:{playbook}:{uuidv7}"`, one launch of a registered playbook pack. Its
    /// param values and launcher ceilings live on a `playbook_launches` row, re-read by key at
    /// dispatch — the pack already exists, so the row never runs a scope turn.
    Playbook { playbook: String, launch: String },
    /// Inert: never fetched, never published. A row whose tag this binary doesn't recognize (or
    /// whose key didn't parse for its own tag) degrades to this instead of failing the read.
    Unknown { tag: String },
    // future: Forge { host, owner, repo, number }
}

impl InputKind {
    /// The DB discriminant vocabulary. Stable strings, never renamed — the tests pin these
    /// spellings against the serialized DTO `type` field.
    pub(crate) fn tag(&self) -> &str {
        match self {
            InputKind::GitHub { .. } => "github",
            InputKind::Scenario { .. } => "scenario",
            InputKind::Jira { .. } => "jira",
            InputKind::Playbook { .. } => "playbook",
            InputKind::Unknown { tag } => tag,
        }
    }

    /// Reconstruct from the stored tag + key. NEVER errors — an unparseable github key or an
    /// unknown tag becomes `Unknown`, so a single bad (or forward-rolled) row can't bail the sweep.
    pub(crate) fn from_parts(tag: &str, key: &str) -> Self {
        match tag {
            "github" => match split_github_key(key) {
                Some((owner, repo, number)) => InputKind::GitHub {
                    owner,
                    repo,
                    number,
                },
                None => InputKind::Unknown {
                    tag: tag.to_string(),
                },
            },
            "scenario" => match key.strip_prefix("scenario:") {
                Some(id) if !id.is_empty() => InputKind::Scenario { id: id.to_string() },
                _ => InputKind::Unknown {
                    tag: tag.to_string(),
                },
            },
            "jira" => match split_jira_key(key) {
                Some((site, project, number)) => InputKind::Jira {
                    site,
                    project,
                    number,
                },
                None => InputKind::Unknown {
                    tag: tag.to_string(),
                },
            },
            "playbook" => match split_playbook_key(key) {
                Some((playbook, launch)) => InputKind::Playbook { playbook, launch },
                None => InputKind::Unknown {
                    tag: tag.to_string(),
                },
            },
            other => InputKind::Unknown {
                tag: other.to_string(),
            },
        }
    }

    /// The stored `input_kind` tags whose kind has a live upstream. Kept in lockstep with
    /// [`InputKind::has_upstream`] by `input_kind_upstream_tags_match_has_upstream`.
    ///
    /// Filters that read `upstream_updated_at` need this: only these rows ever get the stamp, so a
    /// filter that requires one has to spare the rest instead of silently emptying them out.
    pub(crate) const UPSTREAM_TAGS: &'static [&'static str] = &["github"];

    /// Whether this kind has a live upstream (fetchable title/body/labels, staleness/close
    /// detection). `false` routes the row past reconcile's autopilot gates.
    pub(crate) fn has_upstream(&self) -> bool {
        matches!(self, InputKind::GitHub { .. })
    }

    /// Can receive a published PR link back on the source item, and gets a draft-PR approval gate.
    pub(crate) fn accepts_pr_backlink(&self) -> bool {
        matches!(self, InputKind::GitHub { .. })
    }
}

/// Split `owner/repo#N` into its owner, repo, and issue number. `None` on anything that doesn't
/// parse — the caller ([`InputKind::from_parts`]) folds that into `Unknown` rather than erroring.
fn split_github_key(key: &str) -> Option<(String, String, u64)> {
    let (slug, num) = key.rsplit_once('#')?;
    let number = num.parse().ok()?;
    let (owner, repo) = slug.split_once('/')?;
    Some((owner.to_string(), repo.to_string(), number))
}

/// Split `jira:{site}:{PROJ-N}` into its site, project key, and issue number. The site (a Jira
/// Cloud host label like `example`) can't itself contain a colon, so the first colon after the
/// `jira:` prefix bounds it; the trailing `PROJ-N` splits on its last `-`. `None` on anything that
/// doesn't parse — [`InputKind::from_parts`] folds that into `Unknown` rather than erroring.
fn split_jira_key(key: &str) -> Option<(String, String, u64)> {
    let rest = key.strip_prefix("jira:")?;
    let (site, issue) = rest.split_once(':')?;
    if site.is_empty() {
        return None;
    }
    let (project, num) = issue.rsplit_once('-')?;
    let number = num.parse().ok()?;
    if project.is_empty() {
        return None;
    }
    Some((site.to_string(), project.to_string(), number))
}

/// Split `playbook:{playbook}:{uuidv7}` into its registry id and launch id. Registry ids are
/// validated lowercase slugs ([`crate::playbooks::registry::validate_id`]), so neither half can carry the
/// delimiter. `None` on anything that doesn't parse — [`InputKind::from_parts`] folds that into
/// `Unknown` rather than erroring.
fn split_playbook_key(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("playbook:")?;
    let (playbook, launch) = rest.split_once(':')?;
    if playbook.is_empty() || launch.is_empty() {
        return None;
    }
    Some((playbook.to_string(), launch.to_string()))
}

/// The `upstream=` filter: an issue's current state on GitHub, derived from the retired shape —
/// a closed upstream issue is always `(parked, machine, "upstream closed")` here, and untracked
/// closed issues never get a row at all, so the park shape is the full closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum UpstreamState {
    Open,
    Closed,
}

/// Which `new` issues a bulk force-re-rank clears the rank cache for
/// (`POST /api/issues/rerank`). Scoped to `status = 'new'` rows because the reconcile sweep only
/// ranks at the `new` stage — clearing anything else would be a silent no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankScope {
    /// Every `new` issue, ranked or not (a ranker model/prompt change invalidates everything).
    All,
    /// Every `new` issue whose standing verdict is this tier.
    Tier(Tier),
    /// Every `new` issue with no verdict at all (`tier IS NULL` — earlier rank attempts failed).
    Unranked,
}

impl RerankScope {
    /// The human spelling for the audit event and the ack body.
    pub(crate) fn describe(self) -> String {
        match self {
            RerankScope::All => "all".to_string(),
            RerankScope::Tier(t) => format!("tier {}", t.as_str()),
            RerankScope::Unranked => "unranked".to_string(),
        }
    }
}

/// One `issues` row, decoded into strong types (`status`, `parked_by`).
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub(crate) key: String,
    pub(crate) repo: String,
    /// The source this issue came from (`issues.input_kind`), decoded via [`InputKind::from_parts`].
    pub(crate) kind: InputKind,
    pub tier: Option<String>,
    pub status: Status,
    pub(crate) priority: i64,
    pub(crate) evidence_url: Option<String>,
    pub(crate) parked_reason: Option<String>,
    pub(crate) parked_by: Option<ParkedBy>,
    pub(crate) updated_at: String,
    /// The cache key stage 2 last confirmed against (a hash of title+body+labels), or `None`
    /// before the issue has ever been ranked.
    pub(crate) ranked_content_hash: Option<String>,
    /// The content hash the pre-scope grounded gate (migration 0006) last confirmed against —
    /// `None` before a grounded verdict has ever been recorded for this issue. A reconcile whose
    /// `ranked_content_hash` matches this is a cache hit: the gate never re-spends on unchanged
    /// content. See [`crate::issues::reconcile`]'s pre-scope confirmation gate.
    pub(crate) grounded_content_hash: Option<String>,
    /// The upstream issue's title (migration 0004). `None` until the row's first triage sweep
    /// after the column existed, or a `crucible-controller triage --full` backfill.
    pub(crate) title: Option<String>,
    /// The upstream issue's author login (migration 0004). Same backfill story as `title`.
    pub(crate) author: Option<String>,
    /// The upstream issue's body markdown (migration 0011). Same backfill story as `title`;
    /// display-only — the ranker's content hash still comes from its own live fetch.
    pub(crate) body: Option<String>,
    /// The upstream issue's label names (migration 0004; stored as a serde_json array string,
    /// decoded here — NULL/empty both read back as an empty vec). Same backfill story as `title`.
    pub(crate) labels: Vec<String>,
    /// The upstream issue's last activity time (migration 0007, RFC3339 from GitHub's `updated_at`).
    /// NULL until the row's first triage sweep after the column existed, or a `--full` backfill.
    pub(crate) upstream_updated_at: Option<String>,
    /// ScopeNow override: a human's justification for bypassing all gates. `Some` = pending
    /// ScopeNow, `None` = normal autopilot flow. Cleared after the scope dispatch completes.
    pub(crate) scope_now_justification: Option<String>,
    /// ScopeNow override: optional cost ceiling the human specified.
    pub(crate) scope_now_max_cost: Option<f64>,
    /// Redispatch override: a human's justification for re-running an already-approved pack (a new
    /// loop run off the SAME stored pack, no new scope turn or approval). `Some` = a pending redispatch
    /// the awaiting-approval reconcile honors past the autopilot pause; `None` = normal flow. Cleared
    /// once the re-dispatched run launches.
    pub(crate) redispatch_justification: Option<String>,
    /// The branch/tag every turn pod must clone `repo` at (migration 0028), forwarded to the turn
    /// as `--repo-ref`. Only scenario adoption sets it — a github/jira row has no way to name a
    /// ref, so it reads NULL and the clone takes the repo's default branch.
    pub(crate) git_ref: Option<String>,
    /// The NAME of the broker codegen tool contract this item is measured under (migration 0029),
    /// resolved against `ControllerCfg::broker_contracts` at each dispatch. `Some` ⇒ GPU-measured:
    /// the scope turn renders with `--broker-measure` and the loop pod carries the contract as
    /// `BROKER_CODEGEN_TOOLS_OVERLAY`. `None` (every github/jira row) ⇒ the pack measures locally.
    pub(crate) codegen_contract: Option<String>,
    /// The ranker's affinity verdict (`perf` | `perf-adjacent` | `unrelated`); `None` = ranked
    /// before the column existed or never ranked.
    pub(crate) affinity: Option<String>,
    /// The cluster every pod this issue dispatches must land on (migration 0025), chosen at launch
    /// from the set the launcher was authorized for. `None` = the controller's configured default,
    /// resolved at each dispatch.
    pub(crate) dispatch_target: Option<String>,
    /// The model provider every dispatch of this issue runs against (migration 0029), pinned at
    /// launch. `None` = whatever [`crate::playbooks::providers::resolve_dispatch`] answers at dispatch time,
    /// which with an empty registry is nothing at all.
    pub(crate) agent_provider: Option<String>,
    /// The model to ask that provider for; `None` takes the provider's own default. Never set
    /// without `agent_provider`.
    pub(crate) agent_model: Option<String>,
}

impl Issue {
    /// Parse `parked_reason` (if any) into its typed [`ParkReason`] — the boundary callers use to
    /// branch on WHICH reason a row carries instead of matching its stored text.
    pub(crate) fn park_reason(&self) -> Option<ParkReason> {
        self.parked_reason.as_deref().map(ParkReason::parse)
    }
}

/// Discovery metadata for [`crate::client::Db::upsert_issue`]: what triage knows about an issue
/// independent of its lifecycle and its tier. An upsert sets these columns and never disturbs
/// `status`, the park fields, or `tier` — triage is pure discovery: tiering is the
/// ranker's job alone, done at reconcile time, never re-derived or clobbered by a triage sweep.
#[derive(Debug, Clone)]
pub struct NewIssue {
    pub key: String,
    pub repo: String,
    pub priority: i64,
    pub evidence_url: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub body: Option<String>,
    /// Serialized to a JSON array string on write; an empty vec is stored as NULL.
    pub labels: Vec<String>,
    /// The upstream issue's last activity time (RFC3339, from GitHub's `updated_at`).
    pub upstream_updated_at: Option<String>,
}

/// One `issue_comments` row: the local mirror of one upstream GitHub comment (migration 0011).
/// `id` is GitHub's comment id (the upsert key); a discovery sweep replaces an issue's whole set,
/// deleting rows whose comment vanished upstream.
#[derive(Debug, Clone, PartialEq)]
pub struct IssueComment {
    pub(crate) id: i64,
    pub(crate) issue_key: String,
    /// The comment author's login, or `None` for a deleted account (GitHub omits `user`).
    pub(crate) author: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) body: String,
}

/// One `scopes` row: the frozen pack an issue produced, its check outcome, and the approval-gate
/// state. `approved_at` being set is the approval signal reconcile waits on
/// before it launches a run; the approval watch is what sets it.
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub id: i64,
    pub(crate) issue: String,
    pub pack_digest: Option<String>,
    pub(crate) check_outcome: Option<String>,
    pub(crate) stale: bool,
    pub approval_pr: Option<String>,
    pub approved_by: Option<String>,
    pub(crate) approved_at: Option<String>,
    /// The upstream issue's content hash captured the first time a approval reconcile saw this scope.
    /// A later reconcile reading a different hash knows the goal drifted
    /// after freeze — the staleness signal. `None` until the first observation baselines it.
    pub(crate) frozen_issue_hash: Option<String>,
    /// The id of the single "upstream changed" comment posted on the approval PR, so a refresh
    /// edits it in place rather than spamming a new comment each poll. `None` until one is posted.
    pub(crate) stale_comment_id: Option<String>,
    /// What a run of this pack may write and reach, extracted when the pack froze. `None` is
    /// absent-legacy: no enforcement reads it.
    pub exposure: Option<crate::playbooks::exposure::Exposure>,
    pub exposure_digest: Option<String>,
    /// The exposure digest the recorded approval bound to. Differs from `exposure_digest` only if
    /// the pack was re-frozen after the approval landed.
    pub approved_exposure_digest: Option<String>,
}

impl Scope {
    /// The approval is open when a human has recorded an approval. Until then a
    /// `scoped`/`awaiting-approval` row waits — reconcile never launches an unapproved pack.
    pub fn is_approved(&self) -> bool {
        self.approved_at.is_some()
    }
}

/// One `awaiting-approval` issue paired with its open approval PR (the approval poll: the rows
/// whose approval might have just been opened by a human). `repo`/`number` are parsed from `key` by the
/// poll; `approval_pr` is the PR the poll checks for the approval signal.
#[derive(Debug, Clone, PartialEq)]
pub struct AwaitingApproval {
    pub(crate) key: String,
    pub(crate) repo: String,
    pub(crate) scope_id: i64,
    pub(crate) approval_pr: String,
    pub(crate) stale: bool,
    /// The digest of the exposure this revision discloses; `None` for a row that stored none.
    pub(crate) exposure_digest: Option<String>,
}

/// One kept-candidate draft PR: a `candidates` row with `decision = 'keep'` and a
/// `pr_url`, tied back to the issue that produced it. The review-comment poll watches these and
/// reseeds fresh human comments into the issue's next run.
#[derive(Debug, Clone, PartialEq)]
pub struct KeptPr {
    pub(crate) issue: String,
    pub(crate) pr_url: String,
}

/// A surviving pack to record after a scope proposal passed check + selftest ([`crate::issues::reconcile`]).
#[derive(Debug, Clone)]
pub struct NewScope {
    pub issue: String,
    pub pack_digest: Option<String>,
    pub check_outcome: Option<String>,
}

/// Immutable provenance for an administrator-supplied autoresearch pack entering at the
/// approved-scope boundary.
#[derive(Debug, Clone)]
pub(crate) struct NewDirectPack<'a> {
    pub key: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub repo: &'a str,
    pub git_ref: &'a str,
    pub pack_digest: &'a str,
    pub created_by: &'a str,
}

/// A scope turn's structured report to persist (`scope_reports`): the exact
/// `crucible scope --json` object, verbatim, plus the dispatch context the UI links back to
/// (which pod ran it, did the pack survive). Written on every turn that produced a report —
/// success or failure — so a parked issue keeps its evidence.
#[derive(Debug, Clone)]
pub struct NewScopeReport {
    pub(crate) issue_key: String,
    /// The scope work pod, or `None` for the local subprocess executor.
    pub(crate) pod_name: Option<String>,
    pub(crate) survived: bool,
    pub(crate) report_json: String,
}

/// One `scope_reports` row read back; the latest per issue is what
/// `GET /api/issues/{key}/scope-report` serves.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeReportRow {
    pub(crate) id: i64,
    pub(crate) issue_key: String,
    pub(crate) pod_name: Option<String>,
    pub(crate) survived: bool,
    pub(crate) report_json: String,
    pub(crate) created_at: String,
}

/// A scope turn's preserved agent transcript to persist (`scope_transcripts`): the session NDJSON
/// the propose/refine/adversary turns streamed, gzipped exactly as the engine delivered it (over
/// the pod-log marker line or the local `--transcript-out` file).
#[derive(Debug, Clone)]
pub struct NewScopeTranscript {
    pub(crate) scope_report_id: i64,
    pub(crate) issue_key: String,
    pub(crate) transcript_gz: Vec<u8>,
}

/// One `scope_transcripts` row read back; the latest per issue is what
/// `GET /api/issues/{key}/scope-transcript` serves (decompressed).
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeTranscriptRow {
    pub(crate) id: i64,
    pub(crate) scope_report_id: i64,
    pub(crate) issue_key: String,
    pub(crate) transcript_gz: Vec<u8>,
    pub(crate) created_at: String,
}

/// The columns the issues table can be sorted by (the `/`  and `/api/issues` `sort=` param).
/// A closed enum, not a raw string, so [`crate::issues::store::list_issues_filtered`] can whitelist
/// the ORDER BY column instead of interpolating user input into SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, strum::EnumIter)]
pub enum SortKey {
    #[default]
    Updated,
    /// Upstream activity time (`upstream_updated_at`) — GitHub's clock, not ours.
    Upstream,
    Tier,
    Priority,
    Title,
}

/// The filterable input-kind discriminants for [`IssueQuery`]: deserialized from the `?kind=` value
/// and matched against the stored `input_kind` tag. Non-exhaustive so a new [`InputKind`] arm
/// becomes a filter option by adding it here deliberately, not by silently accepting a stray string.
/// `Unknown` isn't offered — it has no stable tag to filter on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[non_exhaustive]
pub enum IssueKind {
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "scenario")]
    Scenario,
    #[serde(rename = "jira")]
    Jira,
    #[serde(rename = "playbook")]
    Playbook,
}

impl IssueKind {
    /// The stored `input_kind` tag this filter matches — kept in lockstep with [`InputKind::tag`].
    pub(crate) fn tag(self) -> &'static str {
        match self {
            IssueKind::GitHub => "github",
            IssueKind::Scenario => "scenario",
            IssueKind::Jira => "jira",
            IssueKind::Playbook => "playbook",
        }
    }
}

/// The `status=`/`tier=`/`repo=` filter set + `sort=`/`dir=` — shared by the `/` UI page and
/// `GET /api/issues` so both call the same [`crate::issues::store::list_issues_filtered`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IssueQuery {
    pub(crate) status: Option<Status>,
    pub(crate) tier: Option<String>,
    /// Keep rows with this ranker affinity verdict (`perf` | `perf-adjacent` | `unrelated`).
    pub(crate) affinity: Option<String>,
    pub(crate) repo: Option<String>,
    /// Keep rows carrying this label (exact label-name match against the stored label set).
    pub(crate) label: Option<String>,
    /// Keep rows of this input kind ([`IssueKind`]), matched against the stored `input_kind` tag.
    pub(crate) kind: Option<IssueKind>,
    /// Drop rows of this input kind. The issues board excludes `playbook` rows this way — a
    /// launch lives on the runs rail, not the triage queue.
    pub(crate) exclude_kind: Option<IssueKind>,
    /// Keep rows whose upstream issue is still open / has been closed (see [`UpstreamState`]).
    pub(crate) upstream: Option<UpstreamState>,
    /// Keep rows with upstream activity at or after this RFC 3339 stamp; rows with no
    /// `upstream_updated_at` (pre-backfill relics) are excluded when set.
    pub(crate) upstream_since: Option<String>,
    pub(crate) sort: SortKey,
    pub(crate) dir: SortDir,
}

/// Per-repository health: issue counts by status + the poll watermark + the runtime watch state
/// (Lane O3).
#[derive(Debug, Clone, PartialEq)]
pub struct RepoHealth {
    pub(crate) repo: String,
    pub(crate) new: i64,
    pub(crate) scoped: i64,
    pub(crate) awaiting_approval: i64,
    pub(crate) running: i64,
    pub(crate) pr_open: i64,
    pub(crate) parked: i64,
    pub(crate) done: i64,
    pub(crate) total: i64,
    pub(crate) watermark: Option<String>,
    /// Discovery iterates this repo iff `watched && !paused` (see `Db::watched_repos`).
    pub(crate) watched: bool,
    /// A watched repo temporarily skipped by discovery, without unwatching it.
    pub(crate) paused: bool,
    /// `"env"` for the boot-time seed, else the admin login that added it via the API.
    pub(crate) added_by: Option<String>,
    /// RFC3339 UTC stamp of when the row was added/seeded.
    pub(crate) added_at: Option<String>,
}

/// One `repos` row's watch state alone (no issue-count join) — what the pause/resume/unwatch API
/// handlers need to decide a 404 vs the state transition.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoWatch {
    pub(crate) repo: String,
    pub(crate) watched: bool,
    pub(crate) paused: bool,
    pub(crate) added_by: Option<String>,
    pub(crate) added_at: Option<String>,
}

/// `owner/repo#N` -> `(owner/repo, N)`, the `scope.rs` `parse_issue` sibling for a key already
/// known to be well-formed (every issue in the DB entered through triage's own upsert).
pub(crate) fn split_issue_key(key: &str) -> Result<(String, u64)> {
    let (repo, number) = key
        .rsplit_once('#')
        .with_context(|| format!("issue key must be owner/repo#N, got {key:?}"))?;
    let number: u64 = number
        .parse()
        .with_context(|| format!("issue key number isn't an integer: {key:?}"))?;
    Ok((repo.to_string(), number))
}

wire_enum!(UpstreamState, "upstream state", parse_only, {
    UpstreamState::Open => "open",
    UpstreamState::Closed => "closed",
});

// The `sort=` query param.
wire_enum!(SortKey, "sort key", parse_only, {
    SortKey::Updated => "updated",
    SortKey::Upstream => "upstream",
    SortKey::Tier => "tier",
    SortKey::Priority => "priority",
    SortKey::Title => "title",
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_issue_key_parses_repo_and_number() -> Result<()> {
        assert_eq!(
            split_issue_key("owner/repo#42")?,
            ("owner/repo".to_string(), 42)
        );
        assert!(split_issue_key("no-hash").is_err());
        assert!(split_issue_key("owner/repo#x").is_err());
        Ok(())
    }

    /// `operations::core::funnel_counts`'s `parked_reason LIKE 'stale%'` aggregate counts exactly
    /// these two variants' renderings without parsing every row in Rust — this guards that the SQL
    /// literal and the enum's `Display` can't silently drift apart.
    #[test]
    fn stale_variants_render_with_the_sql_likes_stale_prefix() {
        assert!(
            ParkReason::StaleRankHorizon { days: 30 }
                .to_string()
                .starts_with("stale")
        );
        assert!(
            ParkReason::StaleAlreadyImplemented {
                rationale: "x".to_string()
            }
            .to_string()
            .starts_with("stale")
        );
    }

    #[test]
    fn park_reason_round_trips_the_recognized_variants() {
        for (reason, rendered) in [
            (ParkReason::UpstreamClosed, "upstream closed"),
            (
                ParkReason::StaleRankHorizon { days: 30 },
                "stale: no upstream activity in 30 days",
            ),
            (
                ParkReason::StaleAlreadyImplemented {
                    rationale: String::new(),
                },
                "stale per grounded ranker: already implemented",
            ),
            (ParkReason::Unscopeable, "unscopeable per ranker"),
            (
                ParkReason::UnsupportedTurnOption {
                    option: "needs --broker-measure".to_string(),
                },
                "unsupported turn option: needs --broker-measure",
            ),
            (
                ParkReason::ScopeProducedNoPack,
                "scope produced no frozen pack",
            ),
            (
                ParkReason::ImageBuildFailed { evidence: None },
                "image build failed: no build-log pointer published",
            ),
            (
                ParkReason::ImageBuildFailed {
                    evidence: Some("https://example/log".to_string()),
                },
                "image build failed: https://example/log",
            ),
            (
                ParkReason::ContractRejected {
                    image: "quay.io/x/loop@sha256:abc".to_string(),
                    engine_version: "unknown".to_string(),
                    controller_version: "1.0.0".to_string(),
                },
                "contract rejection: quay.io/x/loop@sha256:abc carries engine contract unknown, \
                 this controller is built against 1.0.0",
            ),
        ] {
            let text = reason.to_string();
            assert_eq!(text, rendered);
            assert_eq!(ParkReason::parse(&text), reason);
        }
    }

    /// The launch page's secrets call-out reads `secrets_refusal`, so a persisted or replayed
    /// reason has to come back as the variant that carries one, not as `Legacy`.
    #[test]
    fn a_secrets_park_round_trips_and_yields_its_refusal() {
        let reason = ParkReason::SecretsUnresolved {
            detail: "the pack declares secret pr_token, and repo owner/repo has no binding for it"
                .to_string(),
        };
        let text = reason.to_string();
        assert_eq!(
            text,
            "secrets: the pack declares secret pr_token, and repo owner/repo has no binding for it"
        );
        let parsed = ParkReason::parse(&text);
        assert_eq!(parsed, reason);
        assert_eq!(
            parsed.secrets_refusal(),
            Some("the pack declares secret pr_token, and repo owner/repo has no binding for it")
        );
    }

    #[test]
    fn an_unowned_secret_refusal_names_the_secret_and_its_owner() {
        let text = ParkReason::SecretsUnresolved {
            detail:
                "secret pr_token is bound to repo owner/repo and owned by group:/groups/team-x; \
                     the launcher is not group:/groups/team-x and holds no group that is"
                    .to_string(),
        }
        .to_string();
        let refusal = ParkReason::parse(&text)
            .secrets_refusal()
            .expect("a secrets park carries its refusal")
            .to_string();
        assert!(refusal.contains("pr_token"), "{refusal}");
        assert!(refusal.contains("group:/groups/team-x"), "{refusal}");
    }

    /// Only an unresolved-secrets park is the launcher's binding to fix, so nothing else may light
    /// the bind-it-here call-out.
    #[test]
    fn only_an_unresolved_secrets_park_carries_a_refusal() {
        assert_eq!(ParkReason::parse("no repro").secrets_refusal(), None);
        assert_eq!(ParkReason::UpstreamClosed.secrets_refusal(), None);
    }

    #[test]
    fn park_reason_unrecognized_text_falls_back_to_legacy() {
        assert_eq!(
            ParkReason::parse("no repro"),
            ParkReason::Legacy("no repro".to_string())
        );
        assert!(!ParkReason::parse("no repro").auto_unparkable_on_activity());
        assert!(!ParkReason::parse("no repro").is_image_build_failure());
        assert!(!ParkReason::parse("no repro").is_stale_closable());
    }

    #[test]
    fn input_kind_github_round_trips_and_has_upstream() {
        let kind = InputKind::from_parts("github", "neuralmagic/crucible#42");
        assert_eq!(
            kind,
            InputKind::GitHub {
                owner: "neuralmagic".to_string(),
                repo: "crucible".to_string(),
                number: 42,
            }
        );
        assert_eq!(kind.tag(), "github");
        assert!(kind.has_upstream());
        assert!(kind.accepts_pr_backlink());
    }

    #[test]
    fn input_kind_scenario_round_trips_and_has_no_upstream() {
        let kind = InputKind::from_parts("scenario", "scenario:01hz3q3q3q3q3q3q3q3q3q3q3q");
        assert_eq!(
            kind,
            InputKind::Scenario {
                id: "01hz3q3q3q3q3q3q3q3q3q3q3q".to_string(),
            }
        );
        assert_eq!(kind.tag(), "scenario");
        assert!(!kind.has_upstream());
        assert!(!kind.accepts_pr_backlink());
    }

    /// `UPSTREAM_TAGS` drives a SQL clause, so it can drift from `has_upstream` silently. Pin it
    /// against a sample of every filterable kind.
    #[test]
    fn input_kind_upstream_tags_match_has_upstream() {
        let samples = [
            InputKind::from_parts("github", "owner/repo#1"),
            InputKind::from_parts("scenario", "scenario:abc"),
            InputKind::from_parts("jira", "jira:example:ACME-1"),
            InputKind::from_parts("playbook", "playbook:survey:0199c0de-7c2c-71a5-8000-1"),
        ];
        for kind in samples {
            assert_eq!(
                InputKind::UPSTREAM_TAGS.contains(&kind.tag()),
                kind.has_upstream(),
                "{:?}: UPSTREAM_TAGS disagrees with has_upstream",
                kind
            );
        }
    }

    #[test]
    fn input_kind_jira_round_trips_and_has_no_upstream() {
        let kind = InputKind::from_parts("jira", "jira:example:ACME-1234");
        assert_eq!(
            kind,
            InputKind::Jira {
                site: "example".to_string(),
                project: "ACME".to_string(),
                number: 1234,
            }
        );
        assert_eq!(kind.tag(), "jira");
        // Adopt-driven: the body is captured once at adopt time, so a Jira row flows the scenario
        // scope path (no live re-fetch) and gets the UI-native approval gate, not a draft-PR one.
        assert!(!kind.has_upstream());
        assert!(!kind.accepts_pr_backlink());
    }

    /// A launch key round-trips through the stored tag + key, and stays outside every
    /// upstream-shaped gate: there is nothing to re-fetch and nothing to link a PR back to.
    #[test]
    fn input_kind_playbook_round_trips_and_has_no_upstream() {
        let kind = InputKind::from_parts("playbook", "playbook:survey:0199c0de-7c2c-71a5-8000-1");
        assert_eq!(
            kind,
            InputKind::Playbook {
                playbook: "survey".to_string(),
                launch: "0199c0de-7c2c-71a5-8000-1".to_string(),
            }
        );
        assert_eq!(kind.tag(), "playbook");
        assert!(!kind.has_upstream());
        assert!(!kind.accepts_pr_backlink());
    }

    #[test]
    fn input_kind_unparseable_playbook_key_falls_back_to_unknown() {
        for bad_key in [
            "playbook:",
            "playbook:survey",
            "playbook:survey:",
            "playbook::0199c0de",
            "survey:0199c0de",
            "",
        ] {
            assert_eq!(
                InputKind::from_parts("playbook", bad_key),
                InputKind::Unknown {
                    tag: "playbook".to_string()
                },
                "{bad_key:?} should degrade to Unknown"
            );
        }
    }

    #[test]
    fn input_kind_unrecognized_tag_falls_back_to_unknown() {
        let kind = InputKind::from_parts("gitlab", "gitlab:group/proj!7");
        assert_eq!(
            kind,
            InputKind::Unknown {
                tag: "gitlab".to_string()
            }
        );
        assert_eq!(kind.tag(), "gitlab");
        assert!(!kind.has_upstream());
        assert!(!kind.accepts_pr_backlink());
    }

    #[test]
    fn input_kind_unparseable_jira_key_falls_back_to_unknown() {
        for bad_key in [
            "jira:",
            "jira:example",
            "jira:example:",
            "jira::ACME-1",
            "jira:example:ACME-",
            "jira:example:ACME-x",
            "jira:example:noproject",
            "not-jira-prefixed",
        ] {
            let kind = InputKind::from_parts("jira", bad_key);
            assert_eq!(
                kind,
                InputKind::Unknown {
                    tag: "jira".to_string()
                },
                "{bad_key} must degrade to Unknown, never error"
            );
        }
    }

    #[test]
    fn input_kind_unparseable_github_key_falls_back_to_unknown_not_error() {
        for bad_key in ["no-hash", "owner/repo#not-a-number", "no-slash#1"] {
            let kind = InputKind::from_parts("github", bad_key);
            assert_eq!(
                kind,
                InputKind::Unknown {
                    tag: "github".to_string()
                }
            );
            assert!(
                !kind.has_upstream(),
                "an Unknown row must never claim upstream"
            );
        }
    }

    #[test]
    fn input_kind_unparseable_scenario_key_falls_back_to_unknown() {
        for bad_key in ["scenario:", "not-scenario-prefixed"] {
            let kind = InputKind::from_parts("scenario", bad_key);
            assert_eq!(
                kind,
                InputKind::Unknown {
                    tag: "scenario".to_string()
                }
            );
        }
    }
}
