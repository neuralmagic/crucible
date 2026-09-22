use crate::api::state::*;
use crate::client::Db;
use crate::dto::dto;
use crate::issues::model::{AwaitingApproval, InputKind, Issue, KeptPr, RepoHealth, Scope};
use crate::model::LedgerDay;
use crate::runs::model::{Candidate, Run, TaskName};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// --- wire DTOs ---------------------------------------------------------------

/// How many characters of a [`LongText`] a list response carries.
pub(crate) const LIST_TRUNCATE_CHARS: usize = 200;

/// A free-text field big enough that list responses truncate it. Park reasons, turn errors, and
/// event reasons interpolate pod log tails and agent output, so serving them whole on an
/// unpaginated list ships hundreds of KB no row ever renders.
///
/// Carrying the flag next to the text (rather than as a sibling `*_truncated` field) means a
/// consumer can't read one without the other, and the matching detail endpoint answers with the
/// same shape — `truncated: false` — so the render path never branches on which endpoint it came
/// from.
#[derive(Debug, Serialize, ToSchema)]
pub struct LongText {
    /// The text as served: the whole value, or its first [`LIST_TRUNCATE_CHARS`] characters with a
    /// trailing `…`.
    pub text: String,
    /// True when `text` is only a preview; the detail endpoint carries the rest.
    pub truncated: bool,
}

impl LongText {
    /// The whole value, as detail endpoints serve it.
    pub(crate) fn full(text: String) -> Self {
        LongText {
            text,
            truncated: false,
        }
    }

    /// Truncate to [`LIST_TRUNCATE_CHARS`] characters. Counts characters, not bytes, so a
    /// multi-byte grapheme is never split mid-sequence. Idempotent.
    pub(crate) fn truncate(self) -> Self {
        if self.text.chars().count() <= LIST_TRUNCATE_CHARS {
            return self;
        }
        let head: String = self.text.chars().take(LIST_TRUNCATE_CHARS).collect();
        LongText {
            text: format!("{}…", head.trim_end()),
            truncated: true,
        }
    }
}

/// The source an issue came from, tagged for the SPA to `switch` on. Mirrors [`crate::issues::model::InputKind`].
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputKindDto {
    #[serde(rename = "github")]
    GitHub {
        owner: String,
        repo: String,
        number: u64,
    },
    Scenario {
        id: String,
    },
    Jira {
        site: String,
        project: String,
        number: u64,
    },
    /// One launch of a registered playbook pack: `playbook` is the registry id, `launch` the
    /// per-launch uuid.
    Playbook {
        playbook: String,
        launch: String,
    },
    /// An unrecognized or unparseable kind: inert, rendered with no kind-specific affordances.
    Unknown {
        tag: String,
    },
}

impl From<InputKind> for InputKindDto {
    fn from(k: InputKind) -> Self {
        match k {
            InputKind::GitHub {
                owner,
                repo,
                number,
            } => InputKindDto::GitHub {
                owner,
                repo,
                number,
            },
            InputKind::Scenario { id } => InputKindDto::Scenario { id },
            InputKind::Jira {
                site,
                project,
                number,
            } => InputKindDto::Jira {
                site,
                project,
                number,
            },
            InputKind::Playbook { playbook, launch } => InputKindDto::Playbook { playbook, launch },
            InputKind::Unknown { tag } => InputKindDto::Unknown { tag },
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IssueDto {
    pub key: String,
    pub repo: String,
    pub kind: InputKindDto,
    pub tier: Option<String>,
    /// The ranker's affinity verdict (`perf` | `perf-adjacent` | `unrelated`); null when ranked
    /// before affinity existed (or never ranked).
    pub affinity: Option<String>,
    pub status: String,
    pub priority: i64,
    pub evidence_url: Option<String>,
    /// Truncated by `GET /api/issues`, whole on `GET /api/issues/{key}`.
    pub parked_reason: Option<LongText>,
    pub parked_by: Option<String>,
    pub updated_at: String,
    /// GitHub's last-activity stamp for the upstream issue (RFC 3339); null for pre-backfill relics.
    pub upstream_updated_at: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub labels: Vec<String>,
    /// The latest kept-candidate draft PR this issue's runs opened (via
    /// `candidates.pr_url` → runs → scopes), or null when no PR has been kept. Distinct from a
    /// scope's `approval_pr`, which is the human-approval PR.
    pub pr_url: Option<String>,
    /// Whether `parked_reason` is [`ParkReason::StaleAlreadyImplemented`] — the grounded ranker
    /// found the ask already implemented, so a human should go close it upstream. Computed
    /// server-side so the SPA never needs to know the reason's wording.
    pub stale_closable: bool,
    /// The branch/tag a turn clones for this issue, or null for the repo's default. Accepted at
    /// adopt time and echoed on that ack, but the ack is the only place it was ever readable —
    /// which left no way to check what an adopted issue is actually pointed at.
    pub git_ref: Option<String>,
    /// The broker contract codegen runs under, or null for local measure. Same story as `git_ref`.
    pub codegen_contract: Option<String>,
    /// The inference provider this issue's launch pinned, or null to resolve through the configured
    /// defaults at each dispatch.
    pub agent_provider: Option<String>,
    /// The model pinned alongside it; null takes the resolved provider's default.
    pub agent_model: Option<String>,
}

impl IssueDto {
    /// Build the DTO from an issue row plus its kept PR resolved through
    /// [`crate::issues::store::latest_kept_pr_urls`]'s issue → pr_url map.
    pub(crate) fn from_parts(i: Issue, pr_url: Option<String>) -> Self {
        let stale_closable = i.park_reason().is_some_and(|r| r.is_stale_closable());
        IssueDto {
            git_ref: i.git_ref,
            codegen_contract: i.codegen_contract,
            agent_provider: i.agent_provider,
            agent_model: i.agent_model,
            key: i.key,
            repo: i.repo,
            kind: i.kind.into(),
            tier: i.tier,
            affinity: i.affinity,
            status: i.status.as_str().to_string(),
            priority: i.priority,
            evidence_url: i.evidence_url,
            parked_reason: i.parked_reason.map(LongText::full),
            parked_by: i.parked_by.map(|p| p.as_str().to_string()),
            updated_at: i.updated_at,
            upstream_updated_at: i.upstream_updated_at,
            title: i.title,
            author: i.author,
            labels: i.labels,
            pr_url,
            stale_closable,
        }
    }

    /// Truncate `parked_reason` for the list wire. A `ScopeFailed`/`NoSessionEmpty` reason embeds
    /// the engine's stage detail or ten raw pod log lines ([`crate::model::ParkReason`]'s
    /// `Display`), and `GET /api/issues` is unpaginated, so the full text is multi-KB per row
    /// across the whole backlog for a tooltip nobody opens.
    pub(crate) fn truncated_for_list(mut self) -> Self {
        self.parked_reason = self.parked_reason.map(LongText::truncate);
        self
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ScopeDto {
    pub id: i64,
    pub pack_digest: Option<String>,
    pub check_outcome: Option<String>,
    pub stale: bool,
    pub approval_pr: Option<String>,
    pub approved_by: Option<String>,
    pub approved_at: Option<String>,
    pub exposure: ExposureDto,
}

/// What a run of one pack revision may write and reach, plus the presentation block every surface
/// renders from it. `document` and `digest` are null for a row that stored no document, for which
/// `lines` carries the undeclared marker.
#[derive(Debug, Serialize, ToSchema)]
pub struct ExposureDto {
    pub document: Option<crate::playbooks::exposure::Exposure>,
    pub digest: Option<String>,
    /// The digest an approval bound to, once one was recorded. Only an approval surface sets it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_digest: Option<String>,
    pub lines: Vec<String>,
}

impl ExposureDto {
    /// The exposure a stored row carries, with its presentation block rendered.
    pub fn new(
        document: Option<crate::playbooks::exposure::Exposure>,
        digest: Option<String>,
    ) -> Self {
        ExposureDto {
            lines: crate::playbooks::exposure::present(document.as_ref()),
            document,
            digest,
            approved_digest: None,
        }
    }
}

/// The exposure a freshly recomputed extraction discloses (the draft one-shot path). Fails only
/// when the document cannot be serialized to take its digest.
impl TryFrom<&crate::playbooks::exposure::Extraction> for ExposureDto {
    type Error = anyhow::Error;

    fn try_from(extraction: &crate::playbooks::exposure::Extraction) -> Result<Self, Self::Error> {
        let document = extraction.declared().cloned();
        let digest = document.as_ref().map(|e| e.digest()).transpose()?;
        Ok(ExposureDto::new(document, digest))
    }
}

impl From<Scope> for ScopeDto {
    fn from(s: Scope) -> Self {
        let mut exposure = ExposureDto::new(s.exposure, s.exposure_digest);
        exposure.approved_digest = s.approved_exposure_digest;
        ScopeDto {
            id: s.id,
            pack_digest: s.pack_digest,
            check_outcome: s.check_outcome,
            stale: s.stale,
            approval_pr: s.approval_pr,
            approved_by: s.approved_by,
            approved_at: s.approved_at,
            exposure,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RunDto {
    pub run_id: String,
    pub scope: Option<i64>,
    /// The issue key + repo this run belongs to (via `runs.scope → scopes.issue → issues.repo`,
    /// the same join the leaderboard uses). Both `None` when the run has no scope; carried on the
    /// DTO so the run-detail header renders its issue backlink + repo without a leaderboard lookup.
    pub issue_key: Option<String>,
    pub repo: Option<String>,
    pub identity_digest: Option<String>,
    pub status: String,
    pub pod: Option<String>,
    pub session_uri: Option<String>,
    pub best_score: Option<f64>,
    pub cost_usd: Option<f64>,
    /// Where the engine ran: `pod` (a controller-owned work pod) or `local` (a supervised
    /// subprocess on the controller's own machine).
    pub dispatch: String,
    /// The cluster the run was dispatched onto: `hub` or a spoke name.
    pub cluster: String,
    /// The namespace that cluster resolved the pod into; null for a local run or one whose
    /// location predates the columns and could not be recovered.
    pub namespace: Option<String>,
    /// The inference provider the owning issue pinned, or null when the dispatch resolved through
    /// the configured defaults.
    pub agent_provider: Option<String>,
    /// The model pinned alongside it; null takes the resolved provider's default.
    pub agent_model: Option<String>,
    /// Task attempts that ended `transport`: lost to infrastructure, not to a verdict. A run can
    /// finish with these when the lost tasks were advisory.
    pub transport_losses: i64,
    /// The sandbox image reference the pack named; null before the preflight recorded it.
    pub image_ref: Option<String>,
    /// The digest the catalog resolved that reference to at dispatch.
    pub image_digest: Option<String>,
    /// The digest of the capability document the preflight matched.
    pub capability_digest: Option<String>,
    /// True when the run launched on the unverified-image override rather than a match.
    pub image_override: bool,
}

impl RunDto {
    /// Build the DTO from a run row plus what its owning issue carries: the key, the repo, and the
    /// provider/model pair the launch pinned.
    pub(crate) fn from_parts(
        r: Run,
        issue_key: Option<String>,
        repo: Option<String>,
        agent: AgentPinDto,
        transport_losses: i64,
    ) -> Self {
        RunDto {
            agent_provider: agent.provider,
            agent_model: agent.model,
            transport_losses,
            image_ref: r.image.reference,
            image_digest: r.image.digest,
            capability_digest: r.image.capability_digest,
            image_override: r.image.overridden,
            run_id: r.run_id,
            scope: r.scope,
            issue_key,
            repo,
            identity_digest: r.identity_digest,
            status: r.status,
            pod: r.pod,
            session_uri: r.session_uri,
            best_score: r.best_score,
            cost_usd: r.cost_usd,
            dispatch: r.dispatch.as_str().to_string(),
            cluster: r.location.cluster,
            namespace: r.location.namespace,
        }
    }
}

/// The provider/model pair an issue pinned, carried onto the runs it owns. Both `None` is a
/// dispatch that resolved through the configured defaults.
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentPinDto {
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
}

impl AgentPinDto {
    pub(crate) fn of(issue: &Issue) -> Self {
        AgentPinDto {
            provider: issue.agent_provider.clone(),
            model: issue.agent_model.clone(),
        }
    }
}

dto! {
    pub struct CandidateDto: From<c: Candidate> {
        pub run_id: String,
        pub kind: Option<String>,
        pub lane: Option<i64>,
        pub iter: Option<i64>,
        pub score: Option<f64>,
        pub decision: Option<String>,
        pub worktree: Option<String>,
        pub sandbox: Option<String>,
        pub pr_url: Option<String>,
        pub branch: Option<String>,
    }
}

/// A run plus its per-candidate rows — the shape `GET /api/runs/:run_id` returns and the
/// issue-detail provenance nests.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunDetail {
    pub run: RunDto,
    pub candidates: Vec<CandidateDto>,
}

dto! {
    /// One node of a run's admitted work graph, as the engine declared it.
    pub struct PlanTaskDto: From<t: crucible_contract::session::PlanTaskWire> {
        pub name: String,
        /// `agent`, `command`, or a reducer name like `top_k`.
        pub kind: String,
        /// The task names this one waits on — the DAG's edges.
        pub depends_on: Vec<String>,
        /// The durable agent session the task runs in; empty means a fresh turn.
        pub session: String,
        /// The dependency-satisfaction rule (`all`/`any`/…).
        pub needs: String,
        /// Whether a failure of this task fails the plan.
        pub required: bool,
    }
}

dto! {
    /// One task attempt's terminal status.
    pub struct TaskResultDto: From<r: crate::runs::model::TaskResult> {
        pub iter: i64,
        pub task: String,
        /// `pass`/`fail`/`transport`/`skipped`/`blocked`/`truncated`.
        pub status: String,
        pub note: String,
        pub cost_usd: Option<f64>,
        pub secs: Option<f64>,
        /// Why the executor never dispatched the task; present exactly when `status` is `blocked`.
        pub blocked: Option<TaskBlockedDto> = r.blocked.map(TaskBlockedDto::from),
    }
}

dto! {
    /// The contract's `TaskBlocked`, with the reason as its wire token.
    pub struct TaskBlockedDto: From<b: crucible_contract::TaskBlocked> {
        /// `required_task_failed`/`budget_ceiling`/`wall_clock_ceiling`/`dependency_did_not_pass`/
        /// `staging_refused`.
        #[schema(value_type = String)]
        pub reason: crucible_contract::BlockedReasonKind,
        /// The required task whose failure short-circuited the plan; only for `required_task_failed`.
        pub task: Option<String>,
    }
}

/// A run's newest admitted work graph plus every task attempt recorded against the run — the shape
/// `GET /api/runs/{run_id}/graph` returns.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunGraphDto {
    pub plan_version: i64,
    pub tasks: Vec<PlanTaskDto>,
    /// Every iteration's results, oldest first; a task may appear once per iteration.
    pub results: Vec<TaskResultDto>,
    /// The resolved output bounds of the pack this run executes, one per kind: what the pack
    /// declared plus the engine default for every kind it did not name, told apart by `source`.
    /// Absent — not empty — when the revision stored no exposure: a renderer must not pass an
    /// unextracted pack off as one that writes nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<GraphOutputDto>>,
}

/// Where an output bound came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OutputSourceDto {
    /// The pack's own `[outputs]` declaration.
    Manifest,
    /// The engine's default table for a kind the pack did not declare.
    EngineDefault,
}

impl OutputSourceDto {
    fn parse(raw: Option<&str>) -> Option<Self> {
        match raw {
            Some("manifest") => Some(OutputSourceDto::Manifest),
            Some("engine-default") => Some(OutputSourceDto::EngineDefault),
            _ => None,
        }
    }
}

/// One resolved output bound as a graph node: what it is, how many times a run may spend it, where
/// it lands, which task it hangs off, and whether the pack declared it.
#[derive(Debug, Serialize, ToSchema)]
pub struct GraphOutputDto {
    pub kind: String,
    pub count: u32,
    /// Null for a kind that addresses nothing.
    pub target: Option<OutputTargetDto>,
    /// The task that spends this bound. Null attaches it to the graph's sink.
    #[schema(value_type = Option<String>)]
    pub attached_to: Option<TaskName>,
    /// Null when the stored exposure predates the source field; a renderer treats that as
    /// declared, since hiding a bound would show less than the pack may write.
    pub source: Option<OutputSourceDto>,
}

/// Where an output bound may land: one address, or a scope an address has to fall inside.
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputTargetDto {
    Address {
        address: String,
    },
    Scope {
        scope: String,
        /// The workflow param whose value binds the address, when the pack named one.
        param: Option<String>,
    },
}

impl From<&crate::playbooks::exposure::OutputTarget> for OutputTargetDto {
    fn from(target: &crate::playbooks::exposure::OutputTarget) -> Self {
        match target {
            crate::playbooks::exposure::OutputTarget::Fixed { fixed } => OutputTargetDto::Address {
                address: fixed.clone(),
            },
            crate::playbooks::exposure::OutputTarget::Open { open } => OutputTargetDto::Scope {
                scope: open.scope.clone(),
                param: open.param.clone(),
            },
        }
    }
}

/// One bound against the plan whose tasks could spend it.
impl From<(&crate::playbooks::exposure::ExposureOutput, &[TaskName])> for GraphOutputDto {
    fn from((output, tasks): (&crate::playbooks::exposure::ExposureOutput, &[TaskName])) -> Self {
        GraphOutputDto {
            kind: output.kind.clone(),
            count: output.count,
            target: output.target.as_ref().map(OutputTargetDto::from),
            attached_to: crate::runs::model::producing_task(&output.kind, tasks).cloned(),
            source: OutputSourceDto::parse(output.source.as_deref()),
        }
    }
}

/// A scope plus every run it launched, each with its candidates.
#[derive(Debug, Serialize, ToSchema)]
pub struct ScopeDetail {
    pub scope: ScopeDto,
    pub runs: Vec<RunDetail>,
}

dto! {
    pub struct EventDto: From<e: crate::event_log::EventRecord> {
        pub ts: String,
        pub key: String,
        pub from: String,
        pub to: String,
        /// A park writes the same rendered [`crate::model::ParkReason`] into the event log as onto the
        /// issue row, pod log tail and all. Truncated by the feed (`GET /api/events` and the SSE
        /// stream), whole in [`IssueDetail::events`].
        pub reason: Option<LongText> = e.reason.map(LongText::full),
        pub evidence: Option<String>,
        pub actor: Option<String>,
    }
}

impl EventDto {
    /// Truncate `reason` for the feed wire. The full text stays reachable on the issue's detail
    /// page, which carries the same events untruncated.
    pub(crate) fn truncated_for_list(mut self) -> Self {
        self.reason = self.reason.map(LongText::truncate);
        self
    }
}

/// One status bucket with its count.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StatusCount {
    pub status: String,
    pub count: i64,
}

/// One tier bucket with its count.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TierCount {
    pub tier: String,
    pub count: i64,
}

dto! {
    /// One awaiting-approval scope pack: the issue key, repo, scope id, approval PR, staleness flag,
    /// and the digest of the exposure approving it would bind to.
    #[derive(Deserialize)]
    pub struct AwaitingApprovalDto: From<a: AwaitingApproval> {
        pub key: String,
        pub repo: String,
        pub scope_id: i64,
        pub approval_pr: String,
        pub stale: bool,
        /// `null` when the revision stored no document, so nothing is enforced against it. The full
        /// block is on the issue's own page.
        pub exposure_digest: Option<String>,
    }
}

dto! {
    /// One kept-candidate PR: the issue key and the PR URL awaiting review.
    #[derive(Deserialize)]
    pub struct KeptPrDto: From<k: KeptPr> {
        pub issue: String,
        pub pr_url: String,
    }
}

dto! {
    /// One pending pack import, as the rail lists it: who proposed what, pinned where, and whether
    /// the pinned engine had anything to say about it.
    #[derive(Deserialize)]
    pub struct PendingImportDto: From<i: crate::playbooks::imports::PackImport> {
        pub id: String,
        pub repo: String,
        pub path: String,
        pub rev: String,
        pub proposed_by: Option<String>,
        pub created_at: String,
        /// True when the pinned engine extracted a form, so the import is registrable.
        pub compiles: bool = i.schema_digest.is_some(),
        pub diagnostics: i64 = i64::try_from(i.diagnostics.len()).unwrap_or(i64::MAX),
        /// The draft seeded from this import's frozen tarball, once one was.
        pub draft_id: Option<String>,
    }
}

/// The approvals queue: scope packs awaiting approval, pack imports awaiting a registration, and
/// kept-candidate PRs awaiting review.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ApprovalsDto {
    pub awaiting_approval: Vec<AwaitingApprovalDto>,
    pub pending_imports: Vec<PendingImportDto>,
    pub kept_prs: Vec<KeptPrDto>,
}

dto! {
    /// Per-repository health: issue counts by status, the upstream poll watermark, and the runtime
    /// watch state (Lane O3).
    #[derive(Deserialize)]
    pub struct RepoHealthDto: From<r: RepoHealth> {
        pub repo: String,
        pub new: i64,
        pub scoped: i64,
        pub awaiting_approval: i64,
        pub running: i64,
        pub pr_open: i64,
        pub parked: i64,
        pub done: i64,
        pub total: i64,
        pub watermark: Option<String>,
        pub watched: bool,
        pub paused: bool,
        pub added_by: Option<String>,
        pub added_at: Option<String>,
    }
}

/// One cap metric: current value and optional cap/ceiling.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CapMetric {
    pub current: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap: Option<u32>,
}

/// One cost metric: current value (f64) and optional ceiling.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CostMetric {
    pub current: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<f64>,
}

/// Dashboard overview: status/tier counts plus running/scopes/cost vs caps.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Overview {
    pub statuses: Vec<StatusCount>,
    pub tiers: Vec<TierCount>,
    pub running: CapMetric,
    pub scopes_today: CapMetric,
    pub cost_today: CostMetric,
}

/// One box in the fleet-wide pipeline funnel strip: a stage's wire key, display label, current
/// count, and a hint for where the dashboard should link a click (an `/issues?...` or
/// `/runs?...` query the UI's existing filters already understand).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FunnelStage {
    pub key: String,
    pub label: String,
    pub count: i64,
    pub href_hint: String,
}

/// The fleet-wide pipeline funnel (`GET /api/funnel`): `stages` in pipeline order —
/// `discovered`, `ranked`, `awaiting_approval`, `running`, `pr_open`, `done` — followed by the two
/// out-of-band call-outs `parked` and `stale` (a `parked` sub-population, not additional volume).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FunnelDto {
    pub stages: Vec<FunnelStage>,
}

/// Assemble the funnel DTO. Shared by `GET /api/funnel` and (future) dashboard-side rendering so
/// stage order/labels/links live in exactly one place.
pub async fn funnel_dto(db: &Db) -> anyhow::Result<FunnelDto> {
    let c = crate::issues::store::funnel_counts(db.pool()).await?;
    let stages = vec![
        FunnelStage {
            key: "discovered".to_string(),
            // The bucket is the not-yet-ranked slice of `new` (the funnel is a strict partition),
            // NOT cumulative discovery volume — labeled accordingly so a healthy pipeline's small
            // number here doesn't read as a broken discovery counter.
            label: "Awaiting rank".to_string(),
            count: c.discovered,
            href_hint: "/issues?status=new".to_string(),
        },
        FunnelStage {
            key: "ranked".to_string(),
            label: "Ranked".to_string(),
            count: c.ranked,
            href_hint: "/issues?status=scoped".to_string(),
        },
        FunnelStage {
            key: "awaiting_approval".to_string(),
            label: "Awaiting approval".to_string(),
            count: c.awaiting_approval,
            href_hint: "/issues?status=awaiting-approval".to_string(),
        },
        FunnelStage {
            key: "running".to_string(),
            label: "Running".to_string(),
            count: c.running,
            href_hint: "/runs?status=running".to_string(),
        },
        FunnelStage {
            key: "pr_open".to_string(),
            label: "PR open".to_string(),
            count: c.pr_open,
            href_hint: "/issues?status=pr-open".to_string(),
        },
        FunnelStage {
            key: "done".to_string(),
            label: "Done".to_string(),
            count: c.done,
            href_hint: "/issues?status=done".to_string(),
        },
        FunnelStage {
            key: "parked".to_string(),
            label: "Parked".to_string(),
            count: c.parked,
            href_hint: "/issues?status=parked".to_string(),
        },
        FunnelStage {
            key: "stale".to_string(),
            label: "Stale".to_string(),
            count: c.stale,
            href_hint: "/issues?status=parked".to_string(),
        },
    ];
    Ok(FunnelDto { stages })
}

#[utoipa::path(
    get,
    path = "/api/funnel",
    responses((status = 200, description = "Fleet-wide pipeline funnel stage counts", body = FunnelDto))
)]
pub(crate) async fn funnel(State(state): State<ApiState>) -> Result<Json<FunnelDto>, AppError> {
    let dto = funnel_dto(&state.db).await?;
    Ok(Json(dto))
}

/// The full provenance for one issue: the row, every scope it went through (each with its runs
/// and their candidates), and the event-log history — `GET /api/issues/:key` renders from this
/// one assembly. `body` and `comments` live here rather than on [`IssueDto`] so the list endpoint
/// stays lean — full markdown bodies belong on the one detail fetch, not on every row of
/// `GET /api/issues`.
#[derive(Debug, Serialize, ToSchema)]
pub struct IssueDetail {
    pub issue: IssueDto,
    /// The upstream issue's body markdown, or null before discovery has (re)fetched it.
    pub body: Option<String>,
    /// The mirrored upstream comments, oldest first.
    pub comments: Vec<IssueCommentDto>,
    pub scopes: Vec<ScopeDetail>,
    pub events: Vec<EventDto>,
    /// The `scenarios` sidecar row, present only for `InputKind::Scenario` issues — the SPA's
    /// scenario detail panel renders from this instead of `body`/`title` (which stay null; a
    /// scenario never gets a GitHub title/body backfill).
    pub scenario: Option<ScenarioDetailDto>,
}

dto! {
    /// The free-text goal a human adopted, plus who adopted it and when. Distinct from
    /// [`InputKindDto::Scenario`]'s bare `id` — that's identity, this is content.
    pub struct ScenarioDetailDto: From<r: crate::issues::store::ScenarioRow> {
        pub title: String,
        pub body: String,
        pub affected_repos: Vec<String>,
        /// An authoritative brief is injected into the scope agent verbatim, prescriptions intact,
        /// rather than de-prescribed into a neutral problem framing.
        pub authoritative: bool,
        pub created_by: String,
        pub created_at: String,
    }
}

dto! {
    /// One mirrored GitHub comment on the issue-detail surface.
    pub struct IssueCommentDto: From<c: crate::issues::model::IssueComment> {
        /// GitHub's comment id.
        pub id: i64,
        /// The comment author's login, or null for a deleted account.
        pub author: Option<String>,
        pub created_at: String,
        pub updated_at: String,
        /// The comment body markdown.
        pub body: String,
    }
}

/// Assemble the full provenance graph for one issue: issue row → scopes → runs → candidates,
/// plus the event-log history. `None` if the issue is untracked.
pub async fn issue_detail(db: &Db, key: &str) -> anyhow::Result<Option<IssueDetail>> {
    let Some(issue) = crate::issues::store::get_issue(db.pool(), key).await? else {
        return Ok(None);
    };
    let scopes = crate::issues::store::list_scopes_for_issue(db.pool(), key).await?;
    let mut scope_details = Vec::with_capacity(scopes.len());
    // The issue's kept PR for the header chip: the kept candidate from the newest run (run-id
    // lexical order is chronological), tracked while the provenance graph is assembled anyway.
    let mut kept_pr: Option<(String, String)> = None;
    for scope in scopes {
        let runs = crate::runs::store::list_runs_for_scope(db.pool(), scope.id).await?;
        let mut run_details = Vec::with_capacity(runs.len());
        for run in runs {
            let candidates =
                crate::runs::store::list_candidates_for_run(db.pool(), &run.run_id).await?;
            for c in &candidates {
                if c.decision.as_deref() == Some("keep")
                    && let Some(url) = &c.pr_url
                    && kept_pr.as_ref().is_none_or(|(rid, _)| *rid <= run.run_id)
                {
                    kept_pr = Some((run.run_id.clone(), url.clone()));
                }
            }
            let transport_losses =
                crate::runs::task_results::count_transport_losses(db.pool(), &run.run_id).await?;
            run_details.push(RunDetail {
                run: RunDto::from_parts(
                    run,
                    Some(key.to_string()),
                    Some(issue.repo.clone()),
                    AgentPinDto::of(&issue),
                    transport_losses,
                ),
                candidates: candidates.into_iter().map(CandidateDto::from).collect(),
            });
        }
        scope_details.push(ScopeDetail {
            scope: scope.into(),
            runs: run_details,
        });
    }
    let events = db
        .events()
        .read_for_key(key)
        .await?
        .into_iter()
        .map(EventDto::from)
        .collect();
    let comments = crate::issues::store::list_issue_comments(db.pool(), key)
        .await?
        .into_iter()
        .map(IssueCommentDto::from)
        .collect();
    let body = issue.body.clone();
    // Only a scenario has a sidecar row; skip the query for every GitHub issue.
    let scenario = if !issue.kind.has_upstream() {
        crate::issues::store::get_scenario(db.pool(), key)
            .await?
            .map(ScenarioDetailDto::from)
    } else {
        None
    };
    Ok(Some(IssueDetail {
        issue: IssueDto::from_parts(issue, kept_pr.map(|(_, url)| url)),
        body,
        comments,
        scenario,
        scopes: scope_details,
        events,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LedgerSummaryDto {
    pub days: Vec<LedgerDayDto>,
}

dto! {
    pub struct LedgerDayDto: From<d: LedgerDay> {
        pub day: String,
        pub total_usd: f64,
    }
}

/// How many days of history `GET /api/ledger/summary` returns.
const LEDGER_SUMMARY_DAYS: i64 = 30;

pub async fn ledger_summary_dto(db: &Db) -> anyhow::Result<LedgerSummaryDto> {
    let days = crate::ledger::ledger_summary(db.pool(), LEDGER_SUMMARY_DAYS).await?;
    Ok(LedgerSummaryDto {
        days: days.into_iter().map(LedgerDayDto::from).collect(),
    })
}

/// Assemble the overview dashboard DTO for `GET /api/overview`.
pub async fn overview_dto(db: &Db, caps: Option<&Caps>) -> anyhow::Result<Overview> {
    let today = crate::clock::today_utc();
    let statuses_raw = crate::issues::store::status_counts(db.pool()).await?;
    let tiers_raw = crate::issues::store::tier_counts(db.pool()).await?;
    let running_count = crate::runs::store::count_running(db.pool()).await?;
    let scopes_count = crate::issues::store::count_scopes_on_day(db.pool(), &today).await?;
    let cost = crate::ledger::ledger_day_total(db.pool(), &today).await?;

    let statuses = statuses_raw
        .into_iter()
        .map(|(status, count)| StatusCount { status, count })
        .collect();
    let tiers = tiers_raw
        .into_iter()
        .map(|(tier, count)| TierCount { tier, count })
        .collect();

    let running = CapMetric {
        current: running_count,
        cap: caps.map(|c| c.max_concurrent_pods),
    };
    let scopes_today = CapMetric {
        current: scopes_count,
        cap: caps.map(|c| c.max_scopes_per_day),
    };
    let cost_today = CostMetric {
        current: cost,
        ceiling: caps.map(|c| c.daily_cost_ceiling),
    };

    Ok(Overview {
        statuses,
        tiers,
        running,
        scopes_today,
        cost_today,
    })
}

pub(crate) fn not_found(msg: impl Into<String>) -> Response {
    (StatusCode::NOT_FOUND, Json(ErrorBody { error: msg.into() })).into_response()
}

pub(crate) fn forbidden(msg: impl Into<String>) -> Response {
    (StatusCode::FORBIDDEN, Json(ErrorBody::new(msg.into()))).into_response()
}

pub(crate) fn conflict(msg: impl Into<String>) -> Response {
    (StatusCode::CONFLICT, Json(ErrorBody::new(msg.into()))).into_response()
}

pub(crate) fn bad_gateway(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_GATEWAY, Json(ErrorBody::new(msg.into()))).into_response()
}

pub(crate) fn unavailable(msg: impl Into<String>) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorBody::new(msg.into())),
    )
        .into_response()
}

/// A refusal of what the caller sent, in the same shape as [`not_found`].
pub(crate) fn bad_request(msg: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody { error: msg.into() }),
    )
        .into_response()
}

dto! {
    /// The autopilot flag state (runtime-flippable kill switch for machine-initiated spend).
    pub struct AutopilotDto: From<s: crate::daemon::autopilot_flag::AutopilotState> {
        pub enabled: bool,
        pub changed_by: Option<String>,
        pub changed_at: Option<String>,
        pub reason: Option<String>,
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AutopilotSetBody {
    pub enabled: bool,
    pub reason: String,
}

pub(crate) fn unprocessable(msg: impl Into<String>) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(ErrorBody::new(msg.into())),
    )
        .into_response()
}

/// Reduce a pasted GitHub URL to the `owner/repo` slug the rest of the controller keys on
/// (`CONTROLLER_PR_REPO_MAP` lookups, per-repo checkouts, the issues table's repo grouping all
/// assume slugs). Anything that isn't a GitHub URL passes through untouched — non-GitHub remotes
/// stay full URLs on purpose ([`crate::runs::engine::repo_clone_url`] passes them through).
pub(crate) fn normalize_repo(repo: &str) -> &str {
    let Some(path) = repo
        .strip_prefix("https://github.com/")
        .or_else(|| repo.strip_prefix("http://github.com/"))
    else {
        return repo;
    };
    let path = path.trim_end_matches('/');
    path.strip_suffix(".git").unwrap_or(path)
}

/// Upper bound on a stored `git_ref`. Git itself has no such limit, but a branch name this long is
/// pathological and the value ends up in a turn pod's argv.
const GIT_REF_MAX_LEN: usize = 128;

/// Validate an optional branch/tag, returning the trimmed value (`None` = the repo's default
/// branch). Deliberately far narrower than git's own refname grammar, because this string is
/// concatenated into a `crucible deploy render-turn --repo-ref <ref>` argv inside the pod:
///   * a leading `-` would be parsed as another flag,
///   * `..` is a rev-range separator to git and a traversal step to anything treating it as a path,
///   * anything outside `[A-Za-z0-9._/-]` (spaces, `$`, `;`, `^`, `~`, `:`, glob metacharacters) has
///     no business in a branch name we generate a command line from.
///
/// Present-but-blank is a caller mistake, not a synonym for "default branch" — reject it rather
/// than silently accepting a field the caller clearly meant to fill in.
pub(crate) fn require_git_ref(git_ref: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = git_ref else {
        return Ok(None);
    };
    let r = raw.trim();
    if r.is_empty() {
        return Err(
            "git_ref must be non-empty when present (omit it for the default branch)".to_string(),
        );
    }
    if r.len() > GIT_REF_MAX_LEN {
        return Err(format!(
            "git_ref must be at most {GIT_REF_MAX_LEN} characters"
        ));
    }
    if r.starts_with('-') {
        return Err("git_ref must not start with '-'".to_string());
    }
    if r.contains("..") {
        return Err("git_ref must not contain '..'".to_string());
    }
    if !r
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        return Err(
            "git_ref may only contain ASCII letters, digits, '.', '_', '-', and '/'".to_string(),
        );
    }
    Ok(Some(r.to_string()))
}

/// `title`/`body`/`justification` must all be non-empty — same shape of check as `add_repo`'s
/// justification guard.
pub(crate) fn require_non_empty(fields: &[(&str, &str)]) -> Option<String> {
    let missing: Vec<&str> = fields
        .iter()
        .filter(|(_, v)| v.trim().is_empty())
        .map(|(name, _)| *name)
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(format!("{} must be non-empty", missing.join(", ")))
    }
}

/// The field-level rejection body: which form inputs the pack's schema refused, and why.
#[derive(Debug, Serialize, ToSchema)]
pub struct ValidationErrorBody {
    pub error: String,
    pub fields: Vec<crate::playbooks::registry::FieldError>,
}

pub(crate) fn invalid_fields(fields: Vec<crate::playbooks::registry::FieldError>) -> Response {
    refused(
        "the supplied parameters do not satisfy the playbook's schema",
        fields,
    )
}

/// A 422 with its own headline. The field list carries the detail either way; the headline names
/// which of the two refusals this is, since a deployment that cannot dispatch a backend has
/// nothing to do with the parameters the launcher supplied.
pub(crate) fn refused(
    error: &str,
    fields: Vec<crate::playbooks::registry::FieldError>,
) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(ValidationErrorBody {
            error: error.to_string(),
            fields,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry's wire field spellings, which the SPA's launch surface reads directly.
    #[test]
    fn playbook_dto_field_spellings_are_the_wire_contract() {
        let dto = crate::playbooks::api::registry::PlaybookDto {
            owner: "team:platform-administrators".to_string(),
            actions: vec![
                crate::authz::action::Verb::Read,
                crate::authz::action::Verb::ManageMembers,
            ],
            exposure_digest: None,
            id: "survey".to_string(),
            description: "reads a paper".to_string(),
            repo: "neuralmagic/crucible".to_string(),
            git_ref: None,
            rev: "7c2c1a563813ce952dd4039745730397cf2295c2".to_string(),
            path: "examples/paper".to_string(),
            tar_digest: "sha256:beef".to_string(),
            schema_digest: "sha256:cafe".to_string(),
            core_rev: "7c2c1a563813ce952dd4039745730397cf2295c2".to_string(),
            dispatch: crate::playbooks::api::registry::PackDispatchDto::new(
                Some(&crate::playbooks::dispatch::PackAgent::new(
                    "openshell".to_string(),
                    Some("quay.io/x/sandbox:dev".to_string()),
                )),
                &crate::playbooks::dispatch::DispatchCapability::new(
                    crate::config::PlaybookExecutor::Pod,
                    false,
                ),
                &[],
            ),
            created_by: Some("wren".to_string()),
            created_at: "2026-08-22T00:00:00Z".to_string(),
            updated_at: "2026-08-22T00:00:00Z".to_string(),
        };
        let v = serde_json::to_value(&dto).expect("serialize");
        assert_eq!(v["id"], "survey");
        assert_eq!(v["description"], "reads a paper");
        assert_eq!(v["repo"], "neuralmagic/crucible");
        assert_eq!(v["git_ref"], serde_json::Value::Null);
        assert_eq!(v["rev"], "7c2c1a563813ce952dd4039745730397cf2295c2");
        assert_eq!(v["path"], "examples/paper");
        assert_eq!(v["tar_digest"], "sha256:beef");
        assert_eq!(v["schema_digest"], "sha256:cafe");
        assert_eq!(v["core_rev"], "7c2c1a563813ce952dd4039745730397cf2295c2");
        assert_eq!(v["dispatch"]["backend"], "openshell");
        assert_eq!(v["dispatch"]["sandbox_image"], "quay.io/x/sandbox:dev");
        assert_eq!(v["dispatch"]["dispatchable"], false);
        assert_eq!(v["dispatch"]["local_mode"], false);
        assert!(
            v["dispatch"]["refusal"]
                .as_str()
                .is_some_and(|r| r.contains("CONTROLLER_DEPLOY_PROFILE")),
            "{v:#}"
        );
        assert_eq!(v["actions"], serde_json::json!(["read", "manage-members"]));
        assert_eq!(v["created_by"], "wren");
        assert_eq!(v["created_at"], "2026-08-22T00:00:00Z");
        assert_eq!(v["updated_at"], "2026-08-22T00:00:00Z");
    }

    /// The launch ack's field spellings: the launch form reads them back, and the schedule and
    /// one-shot surfaces list the same row.
    #[test]
    fn playbook_launch_ack_is_the_wire_contract() {
        let ack = crate::playbooks::api::registry::PlaybookLaunchAck {
            key: "playbook:survey:0199c0de-7c2c-71a5-8000-1".to_string(),
            playbook: "survey".to_string(),
            params: std::collections::BTreeMap::from([
                ("depth".to_string(), "deep".to_string()),
                ("topic".to_string(), "attention sinks".to_string()),
            ]),
            schema_digest: "sha256:form".to_string(),
            max_cost: 3.5,
            max_time: "30m".to_string(),
            advance_dedupe: false,
            dedupe_schedule: None,
            dispatch_target: "hub".to_string(),
            provider: Some("plat-openai".to_string()),
            model: Some("gpt-5.6-sol".to_string()),
            actor: Some("wren".to_string()),
            exposure: Some(ExposureDto {
                document: None,
                digest: Some("sha256:expo".to_string()),
                approved_digest: None,
                lines: vec!["outputs:".to_string(), "  draft-pr x1 -> o/r".to_string()],
            }),
            image: Default::default(),
        };
        let v = serde_json::to_value(&ack).expect("serialize");
        assert_eq!(v["provider"], "plat-openai");
        assert_eq!(v["model"], "gpt-5.6-sol");
        assert_eq!(v["key"], "playbook:survey:0199c0de-7c2c-71a5-8000-1");
        assert_eq!(v["playbook"], "survey");
        assert_eq!(v["params"]["topic"], "attention sinks");
        assert_eq!(v["params"]["depth"], "deep");
        assert_eq!(v["schema_digest"], "sha256:form");
        assert_eq!(v["max_cost"], 3.5);
        assert_eq!(v["max_time"], "30m");
        assert_eq!(v["advance_dedupe"], false);
        assert_eq!(v["dispatch_target"], "hub");
        assert_eq!(v["actor"], "wren");
        assert_eq!(v["exposure"]["digest"], "sha256:expo");
        assert_eq!(v["exposure"]["lines"][1], "  draft-pr x1 -> o/r");
        assert!(
            v["exposure"].get("approved_digest").is_none(),
            "only an approval surface carries the bound digest: {v}"
        );
    }

    /// The runs surface's wire contract: the SPA prefills a relaunch from `params` and the
    /// ceilings, so these spellings are the ones the form reads.
    #[test]
    fn playbook_run_dto_is_the_wire_contract() {
        let run = crate::launches::api::playbook_runs::PlaybookRunDto::from(
            crate::launches::model::PlaybookRun {
                key: "playbook:survey:0199c0de-7c2c-71a5-8000-1".to_string(),
                playbook: "survey".to_string(),
                description: Some("reads a paper".to_string()),
                params: serde_json::json!({"topic": "attention sinks"}),
                schema_digest: "sha256:form".to_string(),
                current_schema_digest: Some("sha256:moved".to_string()),
                max_cost: 3.5,
                max_time: "30m".to_string(),
                advance_dedupe: true,
                origin: crate::model::LaunchOrigin::Deferred,
                draft_version: None,
                schedule: Some("nightly".to_string()),
                status: crate::model::Status::Done,
                parked_reason: None,
                agent_provider: Some("plat-openai".to_string()),
                agent_model: Some("gpt-5.6-luna".to_string()),
                cost_usd: Some(1.25),
                runs: 2,
                transport_losses: 0,
                created_by: Some("wren".to_string()),
                created_at: "2026-08-22T00:00:00Z".to_string(),
            },
        );
        let v = serde_json::to_value(&run).expect("serialize");
        assert_eq!(v["key"], "playbook:survey:0199c0de-7c2c-71a5-8000-1");
        assert_eq!(v["playbook"], "survey");
        assert_eq!(v["description"], "reads a paper");
        assert_eq!(v["params"]["topic"], "attention sinks");
        assert_eq!(v["schema_digest"], "sha256:form");
        assert_eq!(v["current_schema_digest"], "sha256:moved");
        assert_eq!(v["schedule"], "nightly");
        assert_eq!(v["draft_version"], serde_json::Value::Null);
        assert_eq!(
            v["schema_drifted"], true,
            "a re-pin that moved the form is what the prefill has to warn about"
        );
        assert_eq!(v["max_cost"], 3.5);
        assert_eq!(v["max_time"], "30m");
        assert_eq!(v["advance_dedupe"], true);
        assert_eq!(v["origin"], "deferred");
        assert_eq!(v["status"], "done");
        assert_eq!(v["agent_provider"], "plat-openai");
        assert_eq!(v["agent_model"], "gpt-5.6-luna");
        assert_eq!(v["cost_usd"], 1.25);
        assert_eq!(v["runs"], 2);
        assert_eq!(v["created_by"], "wren");
        assert_eq!(v["created_at"], "2026-08-22T00:00:00Z");
    }

    /// A deferred one-shot's wire contract, including the status vocabulary the list page keys on.
    #[test]
    fn one_shot_dto_is_the_wire_contract() {
        let row = crate::launches::one_shots::OneShot {
            id: "0199c0de-7c2c-71a5-8000-2".to_string(),
            playbook: "survey".to_string(),
            params: serde_json::json!({"topic": "attention sinks"}),
            schema_digest: "sha256:form".to_string(),
            max_cost: 3.5,
            max_time: "30m".to_string(),
            advance_dedupe: false,
            dedupe_schedule: None,
            fire_at: "2026-08-23T14:00:00Z".to_string(),
            status: crate::launches::model::OneShotStatus::Fired,
            fired_key: Some("playbook:survey:0199c0de-7c2c-71a5-8000-1".to_string()),
            fired_at: Some("2026-08-23T14:00:03Z".to_string()),
            created_by: Some("wren".to_string()),
            owner_principal: Some("user:wren".to_string()),
            owner_signin_required: false,
            dispatch_target: None,
            agent_provider: None,
            agent_model: None,
            created_at: "2026-08-22T00:00:00Z".to_string(),
        };
        let v = serde_json::to_value(crate::launches::api::playbook_runs::OneShotDto::from(row))
            .expect("serialize");
        assert_eq!(v["id"], "0199c0de-7c2c-71a5-8000-2");
        assert_eq!(v["playbook"], "survey");
        assert_eq!(v["params"]["topic"], "attention sinks");
        assert_eq!(v["schema_digest"], "sha256:form");
        assert_eq!(v["max_cost"], 3.5);
        assert_eq!(v["max_time"], "30m");
        assert_eq!(v["advance_dedupe"], false);
        assert_eq!(v["fire_at"], "2026-08-23T14:00:00Z");
        assert_eq!(v["status"], "fired");
        assert_eq!(v["fired_key"], "playbook:survey:0199c0de-7c2c-71a5-8000-1");
        assert_eq!(v["fired_at"], "2026-08-23T14:00:03Z");
        assert_eq!(v["created_by"], "wren");
        assert_eq!(v["created_at"], "2026-08-22T00:00:00Z");
    }

    /// A refused launch answers with the offending inputs named, so the form can pin each message
    /// to the field that produced it.
    #[test]
    fn playbook_validation_errors_carry_a_field_and_a_message() {
        let body = crate::api::dto::ValidationErrorBody {
            error: "the supplied parameters do not satisfy the playbook's schema".to_string(),
            fields: vec![crate::playbooks::registry::FieldError {
                field: "topic".to_string(),
                message: "\"ATTENTION\" does not match \"^[a-z ]+$\"".to_string(),
            }],
        };
        let v = serde_json::to_value(&body).expect("serialize");
        assert_eq!(v["fields"][0]["field"], "topic");
        assert!(
            v["fields"][0]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("does not match")
        );
    }

    /// The wire discriminant must match `InputKind::tag()`, the DB/metrics label.
    #[test]
    fn input_kind_dto_discriminant_matches_tag() {
        let github = InputKindDto::from(InputKind::GitHub {
            owner: "neuralmagic".to_string(),
            repo: "crucible".to_string(),
            number: 1,
        });
        let value = serde_json::to_value(&github).expect("serialize");
        assert_eq!(value["type"], "github");
        assert_eq!(
            value["type"],
            InputKind::GitHub {
                owner: "neuralmagic".to_string(),
                repo: "crucible".to_string(),
                number: 1,
            }
            .tag()
        );

        let scenario = InputKindDto::from(InputKind::Scenario {
            id: "s1".to_string(),
        });
        let value = serde_json::to_value(&scenario).expect("serialize");
        assert_eq!(value["type"], "scenario");

        let jira = InputKindDto::from(InputKind::Jira {
            site: "example".to_string(),
            project: "ACME".to_string(),
            number: 1234,
        });
        let value = serde_json::to_value(&jira).expect("serialize");
        assert_eq!(value["type"], "jira");
        assert_eq!(value["site"], "example");
        assert_eq!(value["project"], "ACME");
        assert_eq!(value["number"], 1234);

        let launch = InputKind::Playbook {
            playbook: "survey".to_string(),
            launch: "0199c0de-7c2c-71a5-8000-000000000001".to_string(),
        };
        let value = serde_json::to_value(InputKindDto::from(launch.clone())).expect("serialize");
        assert_eq!(value["type"], "playbook");
        assert_eq!(value["type"], launch.tag());
        assert_eq!(value["playbook"], "survey");
        assert_eq!(value["launch"], "0199c0de-7c2c-71a5-8000-000000000001");
    }

    #[test]
    fn long_text_under_the_threshold_is_served_whole() {
        let short = LongText::full("parked: upstream closed".to_string()).truncate();
        assert_eq!(short.text, "parked: upstream closed");
        assert!(!short.truncated);

        // Exactly at the threshold is still whole — the cut is strictly "longer than".
        let exact = LongText::full("x".repeat(LIST_TRUNCATE_CHARS)).truncate();
        assert_eq!(exact.text.chars().count(), LIST_TRUNCATE_CHARS);
        assert!(!exact.truncated);
    }

    #[test]
    fn long_text_truncate_is_idempotent_and_counts_characters() {
        // Multi-byte throughout: a byte-wise cut would split a code point and panic.
        let cut = LongText::full("é".repeat(LIST_TRUNCATE_CHARS * 3)).truncate();
        assert!(cut.truncated);
        assert_eq!(
            cut.text.chars().count(),
            LIST_TRUNCATE_CHARS + 1,
            "the head plus the ellipsis"
        );
        assert!(cut.text.ends_with('…'));

        // Re-truncating the preview is a no-op on the text: the ellipsis costs one character, so
        // the second pass drops one and re-appends it.
        let again = LongText::full(cut.text.clone()).truncate();
        assert_eq!(again.text, cut.text);
    }

    /// A realistic parked backlog: `NoSessionEmpty` renders ten raw pod log lines into
    /// `parked_reason`, and `GET /api/issues` is unpaginated. Pin the list payload's per-row cost
    /// so nobody re-inlines the full text.
    #[test]
    fn list_dto_truncation_bounds_the_issues_payload() {
        let reason = crate::model::ParkReason::NoSessionEmpty {
            run_id: "20260115T101500Z-owner-repo-1".to_string(),
            rc: Some(1),
            tail: (0..10)
                .map(|i| {
                    format!(
                        "2026-01-15T10:15:0{i}Z crucible-wrapper: step {i} of the loop failed \
                         with a long explanatory message"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
        .to_string();

        let row = |n: usize| crate::issues::model::Issue {
            key: format!("owner/repo#{n}"),
            repo: "owner/repo".to_string(),
            kind: InputKind::from_parts("github", &format!("owner/repo#{n}")),
            tier: Some("T1".to_string()),
            status: crate::model::Status::Parked,
            priority: 0,
            evidence_url: None,
            parked_reason: Some(reason.clone()),
            parked_by: Some(crate::model::ParkedBy::Machine),
            updated_at: "2026-01-15T10:15:00Z".to_string(),
            ranked_content_hash: None,
            grounded_content_hash: None,
            title: Some("a representative issue title".to_string()),
            author: Some("octocat".to_string()),
            body: None,
            labels: vec!["bug".to_string()],
            upstream_updated_at: Some("2026-01-14T00:00:00Z".to_string()),
            scope_now_justification: None,
            scope_now_max_cost: None,
            redispatch_justification: None,
            git_ref: None,
            codegen_contract: None,
            affinity: Some("perf".into()),
            dispatch_target: None,
            agent_provider: None,
            agent_model: None,
        };

        let rows: Vec<crate::issues::model::Issue> = (1..=200).map(row).collect();
        let whole: Vec<IssueDto> = rows
            .iter()
            .cloned()
            .map(|i| IssueDto::from_parts(i, None))
            .collect();
        let listed: Vec<IssueDto> = rows
            .into_iter()
            .map(|i| IssueDto::from_parts(i, None).truncated_for_list())
            .collect();

        let whole_len = serde_json::to_string(&whole).expect("serialize").len();
        let listed_len = serde_json::to_string(&listed).expect("serialize").len();
        assert!(
            listed_len * 2 < whole_len,
            "truncation should more than halve the payload: {listed_len} vs {whole_len}"
        );
        // Every row's reason is bounded now, so the payload scales with row count alone. The
        // floor is the other fields plus the 200-char preview; the ceiling catches a re-inline.
        assert!(
            listed_len / listed.len() < 780,
            "per-row list cost crept up: {} bytes",
            listed_len / listed.len()
        );
        assert!(listed.iter().all(|d| {
            d.parked_reason
                .as_ref()
                .is_some_and(|r| r.truncated && r.text.chars().count() <= LIST_TRUNCATE_CHARS + 1)
        }));
    }
}
