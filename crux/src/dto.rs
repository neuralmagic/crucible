//! The controller's wire shapes, deserialize-only.
//!
//! Hand-written rather than generated from `ui/openapi.json` because this client reads a deliberate
//! subset: the fields the renderers print, and nothing else. Every field is optional-tolerant where
//! the controller might grow one, so a controller ahead of this binary degrades to a missing column
//! rather than a parse error mid-incident.

#![allow(clippy::disallowed_macros)]

use serde::Deserialize;

/// A string the list endpoints cut and the detail endpoints send whole.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct LongText {
    pub text: String,
    #[serde(default)]
    pub truncated: bool,
}

impl LongText {
    /// The text with the controller's own truncation marked, so a reader can tell a short reason
    /// from a clipped one and knows to open the detail view.
    pub fn render(&self) -> String {
        if self.truncated {
            format!("{}… [truncated; see `issue <key>`]", self.text)
        } else {
            self.text.clone()
        }
    }
}

/// `{"type": "github"|"scenario"|"jira"|"unknown", …}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputKind {
    Github {
        owner: String,
        repo: String,
        number: i64,
    },
    /// An adopted scenario: no upstream issue, so no owner/repo/number to print.
    Scenario {
        id: String,
    },
    Jira {
        site: String,
        project: String,
        number: i64,
    },
    Unknown {
        tag: String,
    },
    /// A kind this binary predates. Named, not fatal.
    #[serde(other)]
    Other,
}

impl InputKind {
    pub fn label(&self) -> &'static str {
        match self {
            InputKind::Github { .. } => "github",
            InputKind::Scenario { .. } => "scenario",
            InputKind::Jira { .. } => "jira",
            InputKind::Unknown { .. } => "unknown",
            InputKind::Other => "?",
        }
    }

    /// The upstream reference a human can go read, when there is one. A scenario has none — it was
    /// adopted straight into the controller — and printing an empty column for it is the point.
    pub fn upstream(&self) -> Option<String> {
        match self {
            InputKind::Github {
                owner,
                repo,
                number,
            } => Some(format!("{owner}/{repo}#{number}")),
            InputKind::Jira {
                project, number, ..
            } => Some(format!("{project}-{number}")),
            InputKind::Scenario { .. } | InputKind::Unknown { .. } | InputKind::Other => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub key: String,
    #[serde(default)]
    pub repo: String,
    pub kind: InputKind,
    #[serde(default)]
    pub tier: Option<String>,
    pub status: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub evidence_url: Option<String>,
    #[serde(default)]
    pub parked_reason: Option<LongText>,
    #[serde(default)]
    pub parked_by: Option<String>,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub upstream_updated_at: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub stale_closable: bool,
    /// The branch/tag the turn clones, or null for the repo's default.
    #[serde(default)]
    pub git_ref: Option<String>,
    /// The broker contract codegen runs under, or null for local measure.
    #[serde(default)]
    pub codegen_contract: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub affected_repos: Vec<String>,
    #[serde(default)]
    pub authoritative: bool,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scope {
    pub id: i64,
    #[serde(default)]
    pub pack_digest: Option<String>,
    #[serde(default)]
    pub check_outcome: Option<String>,
    #[serde(default)]
    pub stale: bool,
    /// The human-approval PR. Distinct from a candidate's `pr_url`.
    #[serde(default)]
    pub approval_pr: Option<String>,
    #[serde(default)]
    pub approved_by: Option<String>,
    #[serde(default)]
    pub approved_at: Option<String>,
    #[serde(default)]
    pub exposure: Exposure,
}

/// What a run of one pack revision may write and reach, as the controller renders it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Exposure {
    #[serde(default)]
    pub digest: Option<String>,
    /// The digest the recorded approval bound to.
    #[serde(default)]
    pub approved_digest: Option<String>,
    /// The controller's presentation block: the outputs a run may spend and the reach it holds, or
    /// the undeclared marker. Empty only against a controller that predates the exposure surface.
    #[serde(default)]
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Run {
    pub run_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub pod: Option<String>,
    #[serde(default)]
    pub best_score: Option<f64>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Candidate {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub lane: Option<i64>,
    #[serde(default)]
    pub iter: Option<i64>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunDetail {
    pub run: Run,
    #[serde(default)]
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScopeDetail {
    pub scope: Scope,
    #[serde(default)]
    pub runs: Vec<RunDetail>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub reason: Option<LongText>,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueDetail {
    pub issue: Issue,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub scopes: Vec<ScopeDetail>,
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default)]
    pub scenario: Option<Scenario>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Turn {
    pub pod_name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub issue_key: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub cost_tag: String,
    #[serde(default)]
    pub result: Option<LongText>,
    #[serde(default)]
    pub error: Option<LongText>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub terminal_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanTask {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// `all` (every dependency must pass) or `any`.
    #[serde(default)]
    pub needs: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskResult {
    #[serde(default)]
    pub iter: i64,
    pub task: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub secs: Option<f64>,
}

/// `GET /api/version`: which build is answering.
#[derive(Debug, Clone, Deserialize)]
pub struct Version {
    #[serde(default)]
    pub git_sha: String,
    #[serde(default)]
    pub version: String,
}

/// `GET /api/runs/{run_id}/log`: a run's engine output, or where it lives when the controller
/// holds none.
#[derive(Debug, Clone, Deserialize)]
pub struct RunLog {
    #[serde(default)]
    pub dispatch: String,
    #[serde(default)]
    pub text: Option<String>,
    /// True when the head was dropped to fit the controller's cap.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub location: Option<String>,
}

/// `GET /api/runs/{run_id}/files`: what the run's tasks captured.
#[derive(Debug, Clone, Deserialize)]
pub struct RunFiles {
    #[serde(default)]
    pub files: Vec<RunFile>,
}

/// One file a run's task captured, as the listing reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct RunFile {
    #[serde(default)]
    pub task: String,
    /// The fan-out instance key, for a file captured by an instance of a mapped task.
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size_bytes: u64,
    /// What `crux run-file` takes to fetch this file's content.
    #[serde(default)]
    pub key: String,
}

/// `GET /api/runs/{run_id}/graph`: the admitted plan plus every iteration's results.
#[derive(Debug, Clone, Deserialize)]
pub struct RunGraph {
    #[serde(default)]
    pub plan_version: i64,
    #[serde(default)]
    pub tasks: Vec<PlanTask>,
    /// Oldest first, every iteration. The renderer folds this to the latest per task.
    #[serde(default)]
    pub results: Vec<TaskResult>,
    /// The pack's resolved output bounds, one per kind: declared ones and the engine defaults for
    /// the rest, told apart by `source`. `None` is a revision that stored no exposure — which a
    /// controller predating the exposure surface also sends, and means the same thing.
    #[serde(default)]
    pub outputs: Option<Vec<GraphOutput>>,
}

/// One resolved output bound, drawn off the task that spends it.
#[derive(Debug, Clone, Deserialize)]
pub struct GraphOutput {
    pub kind: String,
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub target: Option<OutputTarget>,
    /// The task this bound hangs off; null attaches it to the graph's sink.
    #[serde(default)]
    pub attached_to: Option<String>,
    /// `manifest` or `engine-default`; absent from a controller predating the field, which
    /// reads as declared.
    #[serde(default)]
    pub source: Option<String>,
}

impl GraphOutput {
    /// Whether the pack itself declared this bound. An absent source is declared: hiding a
    /// bound would show less than the pack may write.
    pub fn is_declared(&self) -> bool {
        self.source.as_deref() != Some("engine-default")
    }
}

/// Where a bound may land: one address, or a scope an address has to fall inside.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputTarget {
    Address {
        address: String,
    },
    Scope {
        scope: String,
        #[serde(default)]
        param: Option<String>,
    },
}

impl OutputTarget {
    /// The target as one presentation token.
    pub fn render(&self) -> String {
        match self {
            OutputTarget::Address { address } => address.clone(),
            OutputTarget::Scope { scope, param } => match param {
                Some(param) => format!("{scope} (param {param})"),
                None => scope.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AwaitingApproval {
    pub key: String,
    pub scope_id: i64,
    #[serde(default)]
    pub approval_pr: String,
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeptPr {
    pub issue: String,
    pub pr_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Approvals {
    #[serde(default)]
    pub awaiting_approval: Vec<AwaitingApproval>,
    #[serde(default)]
    pub kept_prs: Vec<KeptPr>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrokerContracts {
    #[serde(default)]
    pub names: Vec<String>,
}

/// The ack every park/unpark/bump/redispatch returns. `actor` is the identity the controller
/// attributed the change to — the one fact worth echoing back at a human.
#[derive(Debug, Clone, Deserialize)]
pub struct OverrideAck {
    pub key: String,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReconcileAck {
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioAck {
    pub key: String,
    #[serde(default)]
    pub affected_repos: Vec<String>,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub codegen_contract: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApproveAck {
    pub key: String,
    pub scope_id: i64,
    #[serde(default)]
    pub approved_by: String,
    #[serde(default)]
    pub approved_at: String,
}

/// Which identity model the controller reports it is running.
///
/// `Other` rather than a hard failure: this rides in the answer to the one question an operator
/// asks when nothing else works, and a controller that grows a third mode must not take the whole
/// response down with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerAuthMode {
    Proxy,
    Native,
    Other(String),
}

impl<'de> Deserialize<'de> for ControllerAuthMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(match raw.as_str() {
            "proxy" => ControllerAuthMode::Proxy,
            "native" => ControllerAuthMode::Native,
            _ => ControllerAuthMode::Other(raw),
        })
    }
}

impl std::fmt::Display for ControllerAuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControllerAuthMode::Proxy => f.write_str("proxy"),
            ControllerAuthMode::Native => f.write_str("native"),
            ControllerAuthMode::Other(raw) => f.write_str(raw),
        }
    }
}

/// `GET /api/whoami`: what the controller thinks of the caller after all the headers landed.
///
/// `mode`, `groups` and `downgraded` are what the controller says about its own side of the
/// exchange, and are read beside the client's view of the credential so the two can be compared.
#[derive(Debug, Clone, Deserialize)]
pub struct Whoami {
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub mode: Option<ControllerAuthMode>,
    #[serde(default)]
    pub downgraded: bool,
}

/// `POST /api/playbooks/imports`: a proposal frozen at the commit the fetch resolved to.
#[derive(Debug, Clone, Deserialize)]
pub struct PackImport {
    pub id: String,
    pub repo: String,
    #[serde(default)]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub path: String,
    pub rev: String,
    #[serde(default)]
    pub schema_digest: Option<String>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub proposed_by: Option<String>,
}

/// Whether a diagnostic stopped the compile or only the dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DiagnosticKind {
    #[default]
    Compile,
    Dispatch,
    Other(String),
}

impl<'de> Deserialize<'de> for DiagnosticKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(match raw.as_str() {
            "compile" => DiagnosticKind::Compile,
            "dispatch" => DiagnosticKind::Dispatch,
            _ => DiagnosticKind::Other(raw),
        })
    }
}

/// One engine diagnostic with the anchor the editor pins it to.
#[derive(Debug, Clone, Deserialize)]
pub struct Diagnostic {
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub col: Option<u32>,
    pub message: String,
    #[serde(default)]
    pub kind: DiagnosticKind,
}

impl Diagnostic {
    /// `file:line:col: message`, dropping whatever part of the anchor the engine did not give.
    pub fn render(&self) -> String {
        let mut anchor = String::new();
        if let Some(file) = &self.file {
            anchor.push_str(file);
            if let Some(line) = self.line {
                anchor.push_str(&format!(":{line}"));
                if let Some(col) = self.col {
                    anchor.push_str(&format!(":{col}"));
                }
            }
            anchor.push_str(": ");
        }
        let tag = match &self.kind {
            DiagnosticKind::Compile => String::new(),
            DiagnosticKind::Dispatch => "[dispatch] ".to_string(),
            DiagnosticKind::Other(raw) => format!("[{raw}] "),
        };
        format!("{tag}{anchor}{}", self.message)
    }
}

/// `GET /api/playbook-drafts/{id}/files`: the whole pack at one save, and the base a writer edits
/// from.
#[derive(Debug, Clone, Deserialize)]
pub struct DraftFiles {
    pub version: i64,
    #[serde(default)]
    pub saved_by: Option<String>,
    #[serde(default)]
    pub saved_at: String,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, String>,
}

/// `POST /api/playbook-drafts/{id}/versions`: the version stored, and what the engine made of it.
#[derive(Debug, Clone, Deserialize)]
pub struct DraftCompile {
    pub version: i64,
    #[serde(default)]
    pub saved_by: Option<String>,
    #[serde(default)]
    pub saved_at: String,
    #[serde(default)]
    pub schema_digest: Option<String>,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
}

/// `POST /api/playbook-drafts/{id}/graduate`: the export PR the draft was pushed to.
#[derive(Debug, Clone, Deserialize)]
pub struct GraduateAck {
    pub pr_url: String,
}

/// The 409 a save gets when another editor landed first. `error` is the controller's own wording;
/// `current_version` is what the writer must re-read and merge onto.
#[derive(Debug, Clone, Deserialize)]
pub struct StaleBase {
    pub error: String,
    pub base_version: i64,
    pub current_version: i64,
    #[serde(default)]
    pub saved_by: Option<String>,
    #[serde(default)]
    pub saved_at: String,
}

/// The admin ceilings a launch's `--max-cost` and `--max-time` are bounded by.
#[derive(Debug, Clone, Deserialize)]
pub struct PlaybookCaps {
    pub max_cost: f64,
    pub max_time: String,
}

/// A registry secret's metadata. The value never leaves the controller.
#[derive(Debug, Clone, Deserialize)]
pub struct Secret {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub mode: String,
}

/// A secret attached to a scope: what a run at that scope gets, and under which name.
#[derive(Debug, Clone, Deserialize)]
pub struct SecretBinding {
    pub id: String,
    pub secret_id: String,
    pub scope_kind: String,
    pub scope_id: String,
    pub projection_kind: String,
    pub projection: String,
    #[serde(default)]
    pub declared_name: String,
    #[serde(default)]
    pub created_by: Option<String>,
}

/// A registered playbook, as the registry lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct Playbook {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub rev: String,
    #[serde(default)]
    pub created_by: Option<String>,
}

/// One launch of a playbook: the issue key it became, and what it is costing.
#[derive(Debug, Clone, Deserialize)]
pub struct PlaybookRun {
    pub key: String,
    #[serde(default)]
    pub playbook: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub runs: i64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub max_cost: f64,
    #[serde(default)]
    pub created_by: Option<String>,
    /// Why the launch is parked, when it is.
    #[serde(default)]
    pub parked_reason: Option<String>,
    /// A refusal to bind the secrets the pack declared; the launch does not run without them.
    #[serde(default)]
    pub secrets_refusal: Option<String>,
    /// Whether the pack's schema moved under a launch that was validated against the old one.
    #[serde(default)]
    pub schema_drifted: bool,
    /// The inference provider the launch pinned; absent resolves the controller's defaults.
    #[serde(default)]
    pub agent_provider: Option<String>,
    /// The model pinned alongside it; absent takes the provider's default.
    #[serde(default)]
    pub agent_model: Option<String>,
}

/// A row of the runs leaderboard.
#[derive(Debug, Clone, Deserialize)]
pub struct RunRow {
    pub run_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub issue_key: Option<String>,
    #[serde(default)]
    pub best_score: Option<f64>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub created: Option<String>,
    /// Where the run was dispatched. Absent on a controller that predates recorded location.
    #[serde(default)]
    pub cluster: Option<String>,
}

/// A recurrence: what it launches, when it next fires, and whether it can still fire at all.
#[derive(Debug, Clone, Deserialize)]
pub struct Schedule {
    pub id: String,
    #[serde(default)]
    pub playbook: String,
    #[serde(default)]
    pub cron_expr: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub next_due_at: Option<String>,
    #[serde(default)]
    pub last_fired_at: Option<String>,
    #[serde(default)]
    pub consecutive_failures: i64,
    #[serde(default)]
    pub owner_principal: Option<String>,
    /// The owner has to sign in again before this can fire.
    #[serde(default)]
    pub owner_signin_required: bool,
}

/// One tracker watch, as `GET /api/watches` lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct Watch {
    pub id: String,
    #[serde(default)]
    pub playbook: String,
    #[serde(default)]
    pub tracker: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub last_swept_at: Option<String>,
    #[serde(default)]
    pub last_launched_at: Option<String>,
    #[serde(default)]
    pub consecutive_failures: i64,
    #[serde(default)]
    pub owner_principal: Option<String>,
    /// The owner has to sign in again before this can launch.
    #[serde(default)]
    pub owner_signin_required: bool,
}

/// The launch acknowledgement: which issue the launch became.
#[derive(Debug, Clone, Deserialize)]
pub struct LaunchAck {
    pub key: String,
    #[serde(default)]
    pub playbook: String,
    #[serde(default)]
    pub max_cost: f64,
    #[serde(default)]
    pub max_time: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub dispatch_target: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// What this launch is allowed to write and reach, recorded before anything executed. Absent
    /// on a registered launch, whose disclosure is the registry revision's.
    #[serde(default)]
    pub exposure: Option<Exposure>,
}

/// The issue statuses the controller accepts, spelled as they go on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueStatus {
    New,
    Scoped,
    AwaitingApproval,
    Building,
    Running,
    PrOpen,
    Parked,
    Done,
}

impl IssueStatus {
    pub const ALL: [IssueStatus; 8] = [
        IssueStatus::New,
        IssueStatus::Scoped,
        IssueStatus::AwaitingApproval,
        IssueStatus::Building,
        IssueStatus::Running,
        IssueStatus::PrOpen,
        IssueStatus::Parked,
        IssueStatus::Done,
    ];

    pub fn wire(self) -> &'static str {
        match self {
            IssueStatus::New => "new",
            IssueStatus::Scoped => "scoped",
            IssueStatus::AwaitingApproval => "awaiting-approval",
            IssueStatus::Building => "building",
            IssueStatus::Running => "running",
            IssueStatus::PrOpen => "pr-open",
            IssueStatus::Parked => "parked",
            IssueStatus::Done => "done",
        }
    }

    /// The vocabulary, for an error message or a doc line.
    pub fn accepted() -> String {
        IssueStatus::ALL
            .iter()
            .map(|s| s.wire())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl std::fmt::Display for IssueStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire())
    }
}

impl std::str::FromStr for IssueStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        IssueStatus::ALL
            .into_iter()
            .find(|c| c.wire() == s)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown issue status `{s}`; accepted: {}",
                    IssueStatus::accepted()
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// A pack that compiles and still cannot be dispatched is the case the tag exists for: without
    /// it both kinds read as one undifferentiated list, and an agent cannot tell a pack it must fix
    /// to compile from one that compiled and will die at spawn.
    #[test]
    fn a_dispatch_diagnostic_is_tagged_and_a_compile_one_is_not() {
        let parse = |json: &str| {
            serde_json::from_str::<Diagnostic>(json)
                .expect("a diagnostic")
                .render()
        };
        assert_eq!(
            parse(r#"{"file":"workflow.star","line":4,"message":"unknown task"}"#),
            "workflow.star:4: unknown task",
            "an untagged diagnostic is a compile one and renders unchanged"
        );
        assert_eq!(
            parse(r#"{"file":"crucible.toml","message":"no sandbox_image","kind":"dispatch"}"#),
            "[dispatch] crucible.toml: no sandbox_image"
        );
        assert_eq!(
            parse(r#"{"message":"from a newer controller","kind":"whatever"}"#),
            "[whatever] from a newer controller",
            "an unrecognized kind is shown rather than failing the response"
        );
    }

    #[test]
    fn every_controller_status_parses_and_round_trips() {
        for s in IssueStatus::ALL {
            assert_eq!(IssueStatus::from_str(s.wire()).expect("parses"), s);
        }
        assert_eq!(
            IssueStatus::accepted(),
            "new, scoped, awaiting-approval, building, running, pr-open, parked, done"
        );
    }

    #[test]
    fn a_status_the_controller_does_not_have_is_refused_with_the_vocabulary() {
        for bogus in ["scoping", "awaiting_approval", "pr_open", "", "NEW"] {
            let err = IssueStatus::from_str(bogus).expect_err("not a status");
            let msg = format!("{err:#}");
            assert!(msg.contains("awaiting-approval"), "{msg}");
            assert!(msg.contains("pr-open"), "{msg}");
        }
    }
}
