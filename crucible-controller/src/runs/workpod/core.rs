use crate::issues::engine::{self};
use crate::wire_enum::{ParseError, wire_enum};
use crucible::deploy::{DigestResolver, ProposeTier, TurnOpts};
use crucible_contract::Tier;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use std::sync::Arc;

/// What a dispatched work pod does. `AgentTurn` is one bounded turn (dispatched non-blocking,
/// collected out-of-band on its completion edge — see the module doc); `Run` is a full autoresearch
/// loop pod (hours-long, watched out-of-band by the shared pod watch, its result the published
/// `session.jsonl`). A buildah build kind slots in later without reshaping callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    /// One bounded, paid agent turn (a sandboxed `claude` invocation) — see [`TurnKind`].
    AgentTurn(TurnKind),
    /// A full autoresearch loop run for a scoped issue: rendered from the pack manifest, dispatched,
    /// tracked, and GC'd through this primitive, but collected out-of-band. The shared pod-completion
    /// watch drives its `session.jsonl` ingest (where its cost books, once, never here).
    Run,
}

/// The kinds of one-shot agent turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    /// A code-grounded triage-ranking turn (`crucible rank-grounded`).
    GroundedRank,
    /// A scope-propose turn (`crucible scope --propose --json`).
    Scope,
}

impl WorkKind {
    /// The `crucible.io/work-kind` label value the pod carries and the `work_pods.kind` column stores
    /// — the string a sweep reconciles labels ↔ rows on.
    pub(crate) fn label_value(self) -> &'static str {
        match self {
            WorkKind::AgentTurn(t) => t.label_value(),
            WorkKind::Run => "run",
        }
    }

    /// The ledger `kind` this work's cost books under. For [`WorkKind::Run`] this is only the tag
    /// stamped on the `work_pods` row for the audit view — the run's actual `run` ledger row is
    /// booked exactly once by the session ingest, never by this primitive.
    pub(crate) fn cost_tag(self) -> &'static str {
        match self {
            WorkKind::AgentTurn(t) => t.cost_tag(),
            WorkKind::Run => "run",
        }
    }

    /// Parse a `work-kind` label value back to the strong kind; unknown is a corruption error.
    /// Not a [`wire_enum!`]: `AgentTurn` nests [`TurnKind`] and the two label vocabularies
    /// (`label_value` vs the differently-spelled `cost_tag`) aren't a single bijection.
    pub(crate) fn parse_label(s: &str) -> Result<Self, ParseError> {
        match s {
            "grounded-rank" => Ok(WorkKind::AgentTurn(TurnKind::GroundedRank)),
            "scope" => Ok(WorkKind::AgentTurn(TurnKind::Scope)),
            "run" => Ok(WorkKind::Run),
            other => Err(ParseError::Unknown {
                noun: "work-kind",
                value: other.to_string(),
            }),
        }
    }
}

impl TurnKind {
    fn label_value(self) -> &'static str {
        match self {
            TurnKind::GroundedRank => "grounded-rank",
            TurnKind::Scope => "scope",
        }
    }
    fn cost_tag(self) -> &'static str {
        match self {
            TurnKind::GroundedRank => "rank-grounded",
            TurnKind::Scope => "scope",
        }
    }
}

/// A work pod's lifecycle: `queued` (admitted to backpressure, no pod yet) → `running` (pod created,
/// watched) → `succeeded`/`failed` (terminal phase observed) → `collected` (result read, succeeded
/// pod deleted) / `swept` (failed pod's retention elapsed, deleted; or an orphan cleaned).
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum WorkPodState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Collected,
    Swept,
}

wire_enum!(WorkPodState, "work-pod state", both, {
    WorkPodState::Queued => "queued",
    WorkPodState::Running => "running",
    WorkPodState::Succeeded => "succeeded",
    WorkPodState::Failed => "failed",
    WorkPodState::Collected => "collected",
    WorkPodState::Swept => "swept",
});

impl WorkPodState {
    /// A terminal-phase state (`succeeded`/`failed`) — the point `terminal_at` is stamped.
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, WorkPodState::Succeeded | WorkPodState::Failed)
    }
}

/// A `work_pods` row read back in full (the DB layer decodes `state` into the strong type).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkPodRow {
    pub(crate) pod_name: String,
    pub kind: String,
    pub issue_key: Option<String>,
    pub(crate) state: WorkPodState,
    pub(crate) cost_tag: String,
    pub(crate) result: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) terminal_at: Option<String>,
    /// The cluster this pod runs on (`hub` = the controller's own cluster).
    pub(crate) cluster: String,
    /// The UID the API server returned when it created the pod. `None` for a queued row, a pod
    /// that never reached create, and every row written before grants shipped.
    pub(crate) pod_uid: Option<String>,
}

/// A `work_pods` row to insert (a freshly dispatched or queued pod). The DB stamps the timestamps.
#[derive(Debug, Clone)]
pub struct NewWorkPod {
    pub(crate) pod_name: String,
    pub(crate) kind: String,
    pub(crate) issue_key: Option<String>,
    pub(crate) state: WorkPodState,
    pub(crate) cost_tag: String,
    pub(crate) cluster: String,
}

/// Whether a turn may spawn now or must queue: under BOTH the per-kind concurrency cap AND the
/// per-kind daily turn budget → spawn; over either → queue. Layered under the global daily cost
/// ceiling the reconcile already enforces (this never loosens that; it only adds a per-kind gate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Spawn,
    Queue,
}

/// The pure admission decision (see [`Admission`]).
pub(crate) fn admit(active: u32, cap: u32, turns_today: u32, daily_budget: u32) -> Admission {
    if active < cap && turns_today < daily_budget {
        Admission::Spawn
    } else {
        Admission::Queue
    }
}

/// The k8s object name for a grounded-rank turn pod: `crucible-turn-<sanitized-issue>-<suffix>`,
/// DNS-1123-label safe (lowercase alphanumerics + `-`, ≤63 chars) and unique per dispatch (the
/// suffix is the low bits of a monotonic-ish clock). The controller owns the name so its `work_pods`
/// PK + the pod's ownerRef both key on a value it chose, before the pod exists.
pub(crate) fn grounded_rank_pod_name(issue_key: &str) -> String {
    work_pod_name("crucible-turn-", issue_key)
}

fn work_pod_name(prefix: &str, key: &str) -> String {
    let sani = sanitize_dns_label(key);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // base36-ish short suffix from the nanos, plenty to disambiguate re-dispatch of one issue.
    let suffix = format!("{:x}", nanos & 0xffff_ffff);
    // Keep the whole name ≤63: trim the sanitized issue so prefix+issue+'-'+suffix fits.
    let budget = 63usize.saturating_sub(prefix.len() + 1 + suffix.len());
    let sani: String = sani.chars().take(budget).collect();
    let sani = sani.trim_matches('-');
    format!("{prefix}{sani}-{suffix}")
}

/// Lowercase, map every non-alphanumeric to `-`, and collapse the result to a DNS-1123-label body
/// (`owner/repo#42` → `owner-repo-42`). Leaves trimming/length to the caller.
pub(crate) fn sanitize_dns_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// The controller-owned k8s object name for a scope turn pod: `crucible-scope-<sanitized-issue>-<suffix>`,
/// DNS-1123-label safe, following the same pattern as [`grounded_rank_pod_name`].
pub(crate) fn scope_pod_name(issue_key: &str) -> String {
    work_pod_name("crucible-scope-", issue_key)
}

/// The per-turn inputs a dispatch reads off the issue row (and, for a scenario, its sidecar) rather
/// than off config. Grouped into one struct because they used to be four positional parameters
/// threaded through five signatures — two of them `Option<String>`, so a transposed `goal_text` /
/// `git_ref` pair compiled fine and only showed up as a turn cloning the wrong branch.
#[derive(Debug, Clone, Default)]
pub struct TurnInputs {
    /// The issue's confirmed tier, forwarded as `--tier t0|t1`. Scope turns only.
    pub tier: Option<Tier>,
    /// A non-upstream item's ledgered free-text goal (there is no GitHub issue to fetch), written
    /// to a scratch file and forwarded as `--goal-file`. Scope turns only.
    pub goal_text: Option<String>,
    /// The goal is an authoritative brief: forwarded as `--authoritative` so the propose/refine
    /// prompts preserve its prescriptions instead of de-prescribing them. Scope turns only.
    pub authoritative: bool,
    /// The branch/tag to clone the repo at, forwarded as `--repo-ref`. `None` = default branch.
    pub git_ref: Option<String>,
    /// The name of the broker codegen contract the issue is measured under. A scope turn under a
    /// contract needs a broker-measure render option no engine yet has, so `Some` refuses the
    /// dispatch ([`UnsupportedTurnOption::BrokerMeasure`]). Scope turns only — a rank turn measures
    /// nothing.
    pub codegen_contract: Option<String>,
    /// A pack the repo already carries, relative to the checkout root. `Some` makes the scope turn
    /// validate that pack rather than draft one, which spends no agent. Scope turns only.
    pub pack_path: Option<String>,
    /// The harness + model this turn renders with, from the dispatch the issue resolved
    /// ([`crate::playbooks::providers::resolve_for_issue`]). Default (neither set) is the pre-registry render:
    /// the turn pod carries no flag and the pack manifest's `[agent]` table decides. Scope turns
    /// only — a rank turn runs the triage harness the profile configures, not the domain's.
    pub agent: crate::playbooks::providers::AgentSelection,
    /// The provider [`Self::agent`] came from, whose registered key is projected onto the turn pod
    /// as the harness's `*_API_KEY`. A turn rendered against a provider has to be able to pay for
    /// it; `None` (every rank turn, and every dispatch under an empty registry) delivers nothing.
    pub inference_provider: Option<crate::playbooks::providers::ModelProvider>,
}

/// A turn input the linked engine's [`TurnOpts`] has no field for. Refused at dispatch as a
/// contract rejection (ledgered, parked, never retried), so the contract is never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnsupportedTurnOption {
    #[error("the stored pack path is not one the engine will render: {detail}")]
    PackPath { detail: String },
    #[error(
        "scope turn under codegen contract {contract:?} needs an engine whose TurnOpts renders a \
         broker-measured scope (`--broker-measure`); the linked crucible has no such option"
    )]
    BrokerMeasure { contract: String },
}

impl UnsupportedTurnOption {
    /// The render option a `kind` turn carrying `codegen_contract` would need and the linked
    /// engine has no field for.
    pub(crate) fn for_turn(kind: WorkKind, codegen_contract: Option<&str>) -> Option<Self> {
        match (kind, codegen_contract) {
            (WorkKind::AgentTurn(TurnKind::Scope), Some(contract)) => Some(Self::BrokerMeasure {
                contract: contract.to_string(),
            }),
            _ => None,
        }
    }
}

/// Everything one turn dispatch needs to render + create its turn pod.
#[derive(Debug, Clone)]
pub struct WorkPodSpec {
    pub(crate) kind: WorkKind,
    pub(crate) pod_name: String,
    pub(crate) issue_key: String,
    pub(crate) repo_url: String,
    max_cost: f64,
    sandbox_image: String,
    /// The issue's confirmed tier, forwarded to the scope turn as `--tier t0|t1`. Only meaningful
    /// for a scope spec; `None` (and every other kind) emits no flag.
    tier: Option<Tier>,
    /// Max gaming-review concern→refine→re-review cycles, forwarded to the scope turn as
    /// `--gaming-refine-rounds`. Only a scope spec emits the flag; every other kind ignores it.
    gaming_refine_rounds: u32,
    /// Skip the adversarial gaming review entirely on a scope turn (an operator escape hatch for
    /// demo/bring-up postures — see `scope_skip_gaming_review`'s doc). When true, `turn_opts`
    /// sets `skip_gaming_review` instead of `gaming_refine_rounds`. Only a scope spec forwards
    /// either; every other kind ignores this field.
    skip_gaming_review: bool,
    /// A non-upstream scenario's ledgered free-text goal (no GitHub item to fetch): when set,
    /// `turn_opts` forwards it as [`TurnOpts::goal_text`] instead of the issue key — the
    /// Pod-executor counterpart of the local executor's `engine::scope_propose` goal-file arm. Only
    /// a scope spec ever sets this; every other kind leaves it `None`.
    pub(crate) goal_text: Option<String>,
    /// The goal is an authoritative brief, forwarded to the scope turn as `--authoritative` so
    /// the propose/refine prompts preserve its prescriptions. Only a scope spec emits the flag.
    authoritative: bool,
    /// The branch/tag the turn wrapper must clone `repo_url` at, forwarded as `--repo-ref`. Unlike
    /// the fields above this is kind-agnostic: any turn that clones honours it. `None` (every
    /// github/jira row) leaves the clone on the repo's default branch.
    git_ref: Option<String>,
    /// The broker codegen contract name the scope turn would render under. Carried by name so the
    /// refusal ([`UnsupportedTurnOption::BrokerMeasure`]) says WHICH contract asked for it. Only a
    /// scope spec acts on it.
    codegen_contract: Option<String>,
    /// The in-repo pack a scope turn validates instead of drafting one. Only a scope spec acts on it.
    pack_path: Option<String>,
    /// The harness + model the turn's `crucible` invocation carries. Kind-agnostic: any turn that
    /// runs an agent honours it.
    agent: crate::playbooks::providers::AgentSelection,
}

impl WorkPodSpec {
    /// A grounded-rank turn spec. `git_ref` pins the clone to a branch/tag (`None` = default
    /// branch).
    pub(crate) fn grounded_rank(
        pod_name: String,
        issue_key: String,
        repo_url: String,
        max_cost: f64,
        sandbox_image: String,
        git_ref: Option<String>,
    ) -> Self {
        WorkPodSpec {
            kind: WorkKind::AgentTurn(TurnKind::GroundedRank),
            pod_name,
            issue_key,
            // The ledger stores bare `owner/repo`; the turn wrapper git-clones, so it needs a URL.
            repo_url: crate::issues::engine::repo_clone_url(&repo_url),
            max_cost,
            sandbox_image,
            tier: None,
            gaming_refine_rounds: 1,
            skip_gaming_review: false,
            goal_text: None,
            authoritative: false,
            git_ref,
            codegen_contract: None,
            pack_path: None,
            agent: crate::playbooks::providers::AgentSelection::default(),
        }
    }

    /// A scope-propose turn spec. `gaming_refine_rounds` is the effective gaming-review refine
    /// bound and `skip_gaming_review` the effective skip-review override (both config-derived);
    /// `inputs` carries what the issue row itself dictates.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn scope(
        pod_name: String,
        issue_key: String,
        repo_url: String,
        max_cost: f64,
        sandbox_image: String,
        gaming_refine_rounds: u32,
        skip_gaming_review: bool,
        inputs: TurnInputs,
    ) -> Self {
        WorkPodSpec {
            kind: WorkKind::AgentTurn(TurnKind::Scope),
            pod_name,
            issue_key,
            // The ledger stores bare `owner/repo`; the turn wrapper git-clones, so it needs a URL.
            repo_url: crate::issues::engine::repo_clone_url(&repo_url),
            max_cost,
            sandbox_image,
            tier: inputs.tier,
            gaming_refine_rounds,
            skip_gaming_review,
            goal_text: inputs.goal_text,
            authoritative: inputs.authoritative,
            git_ref: inputs.git_ref,
            codegen_contract: inputs.codegen_contract,
            pack_path: inputs.pack_path,
            agent: inputs.agent,
        }
    }

    /// The engine's render options for this pod: what `crucible deploy render-turn` would parse
    /// off the flags, handed straight to [`crucible::deploy::render_turn`]. `digests` is the
    /// image-pinning resolver ([`crate::config::ControllerCfg::digest_resolver`]).
    pub(crate) fn turn_opts(
        &self,
        digests: Option<Arc<dyn DigestResolver>>,
    ) -> Result<TurnOpts, UnsupportedTurnOption> {
        let scope = self.kind == WorkKind::AgentTurn(TurnKind::Scope);
        if let Some(unsupported) =
            UnsupportedTurnOption::for_turn(self.kind, self.codegen_contract.as_deref())
        {
            return Err(unsupported);
        }
        let kind = match scope {
            true => crucible::deploy::TurnKind::Scope,
            false => crucible::deploy::TurnKind::Rank,
        };
        // Only the t0/t1 spellings exist on the engine's tier (mirrors `engine::scope_propose`'s
        // filter); anything else, and every non-scope turn, leaves it unset.
        let tier = match (scope, self.tier) {
            (true, Some(Tier::T0)) => Some(ProposeTier::T0),
            (true, Some(Tier::T1)) => Some(ProposeTier::T1),
            _ => None,
        };
        Ok(TurnOpts {
            kind,
            name: self.pod_name.clone(),
            issue: self.issue_key.clone(),
            goal_text: self.goal_text.clone().filter(|_| scope),
            repo_url: self.repo_url.clone(),
            repo_ref: self.git_ref.clone(),
            sandbox_image: self.sandbox_image.clone(),
            max_cost: self.max_cost,
            digests,
            tier,
            gaming_refine_rounds: self.gaming_refine_rounds,
            skip_gaming_review: scope && self.skip_gaming_review,
            authoritative: scope && self.authoritative,
            harness: self.agent.harness,
            model: self.agent.model.clone(),
            pack_path: match (scope, self.pack_path.as_deref()) {
                (true, Some(p)) => Some(crucible::deploy::PackPath::parse(p).map_err(|e| {
                    UnsupportedTurnOption::PackPath {
                        detail: e.to_string(),
                    }
                })?),
                _ => None,
            },
        })
    }

    /// The row to persist for this dispatch, in `state`, bound to the cluster it dispatches to.
    pub(crate) fn new_row(&self, state: WorkPodState, cluster: &str) -> NewWorkPod {
        NewWorkPod {
            pod_name: self.pod_name.clone(),
            kind: self.kind.label_value().to_string(),
            issue_key: Some(self.issue_key.clone()),
            state,
            cost_tag: self.kind.cost_tag().to_string(),
            cluster: cluster.to_string(),
        }
    }
}

/// Stamp the controller-owned metadata onto a rendered turn pod: the exact issue key as an annotation
/// (round-trips verbatim, unlike the lossy label), the managed-by pod-watch selector, and — when the
/// controller knows its own identity — an ownerReference so kind GC ties the pod's lifetime to the
/// controller. render-turn already set the name + the `work-kind` label, so this only adds ownership.
pub(crate) fn stamp_pod(pod: &mut Pod, spec: &WorkPodSpec, owner: Option<OwnerReference>) {
    apply_managed_meta(pod, &spec.issue_key, owner);
}

/// Stamp the metadata every controller-owned work pod shares: the exact issue key as an annotation
/// (round-trips verbatim, unlike the lossy label), the lossy issue-key label hint, the managed-by
/// pod-watch selector, and — when the controller knows its own identity — an ownerReference for kind
/// GC. Shared by [`stamp_pod`] (turns) and [`stamp_run_pod`] (loop runs).
pub(crate) fn apply_managed_meta(pod: &mut Pod, issue_key: &str, owner: Option<OwnerReference>) {
    stamp_managed_meta(&mut pod.metadata, issue_key, owner);
    pod.metadata
        .labels
        .get_or_insert_with(Default::default)
        .insert(
            crate::daemon::ISSUE_KEY_LABEL.to_string(),
            engine::issue_key_label_value(issue_key),
        );
}

/// Stamp the issue-key annotation, the managed-by pod-watch selector, and an optional ownerReference
/// onto any controller-owned object's metadata.
pub(crate) fn stamp_managed_meta(
    meta: &mut ObjectMeta,
    issue_key: &str,
    owner: Option<OwnerReference>,
) {
    meta.annotations
        .get_or_insert_with(Default::default)
        .insert(
            crate::daemon::ISSUE_KEY_ANNOTATION.to_string(),
            issue_key.to_string(),
        );
    if let Some((k, v)) = crate::daemon::MANAGED_BY_SELECTOR.split_once('=') {
        meta.labels
            .get_or_insert_with(Default::default)
            .insert(k.to_string(), v.to_string());
    }
    if let Some(owner) = owner {
        meta.owner_references
            .get_or_insert_with(Default::default)
            .push(owner);
    }
}

/// The controller's own object reference, for owning the turn pods it creates. Sourced from the
/// downward-API env the deployment injects (`CONTROLLER_OWNER_*`); absent → no ownerRef (labels still
/// drive the sweep, so GC still works, just not via k8s cascade).
pub(crate) fn owner_reference_from_env() -> Option<OwnerReference> {
    let name = std::env::var("CONTROLLER_OWNER_NAME")
        .ok()
        .filter(|s| !s.is_empty())?;
    let uid = std::env::var("CONTROLLER_OWNER_UID")
        .ok()
        .filter(|s| !s.is_empty())?;
    let kind = std::env::var("CONTROLLER_OWNER_KIND").unwrap_or_else(|_| "Deployment".to_string());
    let api_version =
        std::env::var("CONTROLLER_OWNER_API_VERSION").unwrap_or_else(|_| "apps/v1".to_string());
    Some(OwnerReference {
        api_version,
        kind,
        name,
        uid,
        controller: Some(true),
        block_owner_deletion: None,
    })
}
