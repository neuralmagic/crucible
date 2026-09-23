//! [`ControllerCfg`]: the daemon's configuration, clap-derived with env fallbacks. Two halves:
//! the runtime/environment locators (where the DB and state live, which repos to watch) parsed
//! from CLI/env, and the [`Profile`] — discovery cadence plus the four unattended-run caps —
//! that the deploy profile's `[controller]` TOML block supplies. The profile is
//! `deny_unknown_fields` so a typo'd cap in a profile is a load error, not a silently-ignored
//! key.

#![allow(clippy::disallowed_macros)]

use anyhow::{Result, bail};
use crucible_contract::Tier;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How grounded ranking turns run. Replaces the old silent `CONTROLLER_SANDBOX_IMAGE`-presence gate
/// (which failed closed in-cluster: env unset → `local` backend → no `claude` binary → every grounded
/// escalation died to the text verdict). Now an explicit choice whose prerequisites are checked
/// loudly at startup, never per-verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum GroundedExecutor {
    /// Dispatch a controller-owned WorkPod per turn (the in-cluster default; the pod carries the
    /// podman stack + sandbox the agent turn needs).
    Pod,
    /// Run the turn in-process by shelling the `claude` CLI on this machine — a dev-machine mode
    /// only (the loop/controller image has no `claude` binary).
    Local,
    /// Skip grounded ranking entirely; every verdict stays the cheap text-only tier.
    Disabled,
}

/// How scope-propose turns run. Same shape as [`GroundedExecutor`]: `pod` dispatches a WorkPod,
/// `local` shells `crucible scope` in-process, `disabled` skips the scope entirely (no autopilot
/// scoping). `local` is the compatible default so nothing breaks for existing deployments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum ScopeExecutor {
    Pod,
    Local,
    Disabled,
}

/// How a playbook launch's engine runs. `pod` dispatches a controller-owned work pod, the only
/// mode a cluster deployment has. `local` runs the engine as a supervised subprocess on the
/// controller's own machine: a dev-machine mode, where the run spends real money through whatever
/// harness that machine carries and has no pod isolation around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum PlaybookExecutor {
    Pod,
    Local,
}

/// Discovery cadence + the four unattended-run caps. The TOML shape of the deploy
/// profile's `[controller]` block (rendered there); also flattened into the CLI so a caps knob
/// can be overridden per invocation.
#[derive(Debug, Clone, PartialEq, clap::Args, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    /// Seconds between discovery sweeps (the timer source's interval).
    #[arg(long = "discovery-cadence-secs", env = "CONTROLLER_DISCOVERY_CADENCE_SECS", default_value_t = default_discovery_secs())]
    pub(crate) discovery_secs: u64,
    /// Per-reconcile cost ceiling in USD (scope turns + launched runs for one reconcile).
    #[arg(long, env = "CONTROLLER_PER_RECONCILE_COST_USD", default_value_t = default_per_reconcile_cost())]
    pub(crate) per_reconcile_cost: f64,
    /// Max concurrent loop pods admitted at once.
    #[arg(long, env = "CONTROLLER_MAX_CONCURRENT_PODS", default_value_t = default_max_concurrent_pods())]
    pub max_concurrent_pods: u32,
    /// Max new scopes started per day.
    #[arg(long, env = "CONTROLLER_MAX_SCOPES_PER_DAY", default_value_t = default_max_scopes_per_day())]
    pub max_scopes_per_day: u32,
    /// Global daily cost ceiling in USD; crossing it parks the autopilot entirely.
    #[arg(long, env = "CONTROLLER_DAILY_COST_CEILING_USD", default_value_t = default_daily_cost_ceiling())]
    pub daily_cost_ceiling: f64,
    /// Fallback cost (USD) ledgered for one tier-ranking call when neither
    /// the verdict JSON nor the ranking command's output self-reports one.
    #[arg(long, env = "CONTROLLER_RANK_COST_FALLBACK_USD", default_value_t = default_rank_cost_fallback())]
    pub(crate) rank_cost_fallback_usd: f64,
    /// Max concurrent grounded-rank WorkPods (the per-kind concurrency cap, layered under the global
    /// `daily_cost_ceiling`). Over it, a grounded turn queues in the ledger and drains as a slot frees.
    #[arg(long, env = "CONTROLLER_GROUNDED_RANK_POD_CAP", default_value_t = default_grounded_rank_pod_cap())]
    pub(crate) grounded_rank_pod_cap: u32,
    /// Max grounded-rank turns dispatched per UTC day (the per-kind daily turn budget). Over it, a
    /// grounded turn queues until the next day rolls the count over.
    #[arg(long, env = "CONTROLLER_GROUNDED_RANK_DAILY_TURNS", default_value_t = default_grounded_rank_daily_turns())]
    pub(crate) grounded_rank_daily_turns: u32,
    /// Max retained failed work pods kept for debugging, newest first. Anything past the newest N
    /// is swept immediately, even inside the 24h retention window. 0 keeps none.
    #[arg(long, env = "CONTROLLER_FAILED_POD_KEEP", default_value_t = default_failed_pod_keep())]
    pub(crate) failed_pod_keep: u32,
    /// Max concern→refine→re-review cycles a scope turn's gaming review may spend
    /// (`crucible scope --gaming-refine-rounds`). 1 is the historical one-cycle behavior.
    #[arg(long, env = "CONTROLLER_SCOPE_GAMING_ROUNDS", default_value_t = default_scope_gaming_rounds())]
    pub(crate) scope_gaming_rounds: u32,
    /// Skip the adversarial gaming review entirely on a scope turn (`crucible scope
    /// --skip-gaming-review`), overriding `scope_gaming_rounds` — an operator escape hatch for
    /// demo/bring-up postures where the review's fail-closed loop blocks the first e2e run through
    /// a new rig. Off by default: the review stays the standing policy.
    #[arg(long, env = "CONTROLLER_SCOPE_SKIP_GAMING_REVIEW", default_value_t = default_scope_skip_gaming_review())]
    pub(crate) scope_skip_gaming_review: bool,
    /// Max concurrent image builds in flight (the per-kind concurrency cap). Builds are compute,
    /// not LLM spend, so this is their only budget — no daily-turn cap and no `$` ledger row.
    /// Over it, a pack that needs a build waits at `building` and dispatches when a slot frees.
    /// Not yet in the Lane O2 runtime-override set (read straight off the profile); making it
    /// retunable-without-restart is a follow-up.
    #[arg(long, env = "CONTROLLER_BUILD_POD_CAP", default_value_t = default_build_pod_cap())]
    pub(crate) build_pod_cap: u32,
    /// Agent iterations a controller-dispatched loop run gets (`crucible deploy render --iterations`).
    /// The pre-controller default of 1 is a stale assumption — it cut the first dispatched run off
    /// after a single turn (`iters_total=1`, no code diff), so a real run needs a multi-turn budget.
    #[arg(long = "run-iterations", env = "CONTROLLER_RUN_ITERATIONS", default_value_t = default_run_iterations())]
    pub(crate) run_iterations: u32,
    /// Per-run cost ceiling in USD for a controller-dispatched loop run (`--max-cost`). The stale
    /// default of `0` (unlimited) rendered as `max_cost=0.0` and cut the agent's turn immediately; a
    /// real budget lets it actually attempt a solve. 0 = unlimited.
    #[arg(long = "run-max-cost", env = "CONTROLLER_RUN_MAX_COST_USD", default_value_t = default_run_max_cost())]
    pub(crate) run_max_cost: f64,
}

fn default_discovery_secs() -> u64 {
    300
}
fn default_per_reconcile_cost() -> f64 {
    10.0
}
fn default_max_concurrent_pods() -> u32 {
    2
}
fn default_max_scopes_per_day() -> u32 {
    20
}
fn default_daily_cost_ceiling() -> f64 {
    50.0
}
fn default_rank_cost_fallback() -> f64 {
    0.05
}
fn default_grounded_rank_pod_cap() -> u32 {
    4
}
fn default_grounded_rank_daily_turns() -> u32 {
    50
}
fn default_failed_pod_keep() -> u32 {
    20
}
fn default_scope_gaming_rounds() -> u32 {
    1
}
fn default_scope_skip_gaming_review() -> bool {
    false
}
fn default_build_pod_cap() -> u32 {
    2
}
fn default_run_iterations() -> u32 {
    6
}
fn default_run_max_cost() -> f64 {
    25.0
}
fn default_playbook_max_cost_cap() -> f64 {
    default_run_max_cost()
}

/// Consecutive failed firings after which a playbook schedule disables itself.
const DEFAULT_SCHEDULE_AUTO_DISABLE_FAILURES: i64 = 5;

/// How long a schedule's snapshot of its owner's groups stays usable. Past it a firing whose scope
/// binds secrets parks until an owner re-saves the schedule.
const DEFAULT_SCHEDULE_OWNER_TTL_SECS: u64 = 7 * 24 * 3600;

/// The default playbook wall-clock cap, in hours. Spelled once: clap renders it into
/// `--playbook-max-time-cap`'s default and [`PlaybookCaps::default`] builds the same value.
const DEFAULT_PLAYBOOK_MAX_TIME_HOURS: u64 = 4;

/// The admin bounds a playbook launcher's ceilings are checked against.
#[derive(Debug, Clone)]
pub struct PlaybookCaps {
    pub max_cost: f64,
    pub max_time: crate::model::MaxTime,
}

impl Default for PlaybookCaps {
    fn default() -> Self {
        PlaybookCaps {
            max_cost: default_playbook_max_cost_cap(),
            max_time: crate::model::MaxTime::hours(DEFAULT_PLAYBOOK_MAX_TIME_HOURS),
        }
    }
}
/// The loop-pod control-bridge port, matching `crucible::deploy::render`'s `CONTROL_PORT`.
fn default_control_port() -> u16 {
    7777
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            discovery_secs: default_discovery_secs(),
            per_reconcile_cost: default_per_reconcile_cost(),
            max_concurrent_pods: default_max_concurrent_pods(),
            max_scopes_per_day: default_max_scopes_per_day(),
            daily_cost_ceiling: default_daily_cost_ceiling(),
            rank_cost_fallback_usd: default_rank_cost_fallback(),
            grounded_rank_pod_cap: default_grounded_rank_pod_cap(),
            grounded_rank_daily_turns: default_grounded_rank_daily_turns(),
            failed_pod_keep: default_failed_pod_keep(),
            scope_gaming_rounds: default_scope_gaming_rounds(),
            scope_skip_gaming_review: default_scope_skip_gaming_review(),
            build_pod_cap: default_build_pod_cap(),
            run_iterations: default_run_iterations(),
            run_max_cost: default_run_max_cost(),
        }
    }
}

impl Profile {
    /// The discovery cadence as a [`Duration`].
    pub fn discovery_interval(&self) -> Duration {
        Duration::from_secs(self.discovery_secs)
    }
}

/// The `DATABASE_URL` value that asks the daemon to run its own Postgres.
pub const EMBEDDED_DATABASE_URL: &str = "embedded";

/// The controller's full config: where scratch lives + which repos to watch + the [`Profile`].
///
/// The deploy render injects everything as env vars — `CONTROLLER_SCRATCH_DIR` and friends.
/// Everything durable lives in Postgres (`DATABASE_URL`); the scratch dir carries only
/// regenerable caches (repo checkouts, the flow-render cache).
#[derive(Debug, Clone, clap::Args)]
pub struct ControllerCfg {
    /// Where a dispatch reads the values of the secrets a scope binds ([[ADR-0036]]). Not a flag:
    /// it is a live dependency the binary installs at startup, carried here for the same reason
    /// [`ControllerCfg::digest_resolver`] is — the reconcile path threads `cfg` and nothing else
    /// down to dispatch. `None` refuses a launch that binds anything, rather than starting it
    /// without what it declared.
    #[arg(skip)]
    pub secret_provider: Option<std::sync::Arc<dyn crate::secrets::provider::SecretProvider>>,
    /// The legacy state directory. Nothing durable lives here anymore; kept so
    /// `db migrate-state` can read an old volume. The daemon never requires it to exist.
    #[arg(long, env = "CONTROLLER_STATE_DIR", default_value = "state")]
    pub state_dir: PathBuf,
    /// Scratch root for regenerable caches: repo checkouts (`checkouts/`) and the flow-render
    /// cache (`flow-cache/`). Defaults to `state_dir` when unset.
    #[arg(long, env = "CONTROLLER_SCRATCH_DIR")]
    pub scratch_dir: Option<PathBuf>,
    /// The Postgres ledger URL. `DATABASE_URL` so sqlx tooling (`cargo sqlx prepare`, the test
    /// macro) and the daemon read the same variable; the default only fits a localhost dev server.
    #[arg(
        long = "db",
        env = "DATABASE_URL",
        default_value = "postgres://localhost:5432/crucible"
    )]
    pub db: String,
    /// A repo (`owner/repo`) to watch at boot; repeatable, or comma-separated via the env var.
    /// **Seed-only** (Lane O3): consulted exactly once, at startup, to idempotently seed a
    /// watched row in the `repos` table (`crate::issues::repo_watch::seed_watched_repos`) — never overwriting a row an
    /// admin already added or paused/unwatched. Discovery itself never reads this field again; it
    /// iterates the DB's live watch-set (`Db::watched_repos`), so a repo added later via
    /// `POST /api/repos`, or one removed from this list on a redeploy, is unaffected either way.
    #[arg(long = "repo", env = "CONTROLLER_WATCHED_REPOS", value_delimiter = ',')]
    pub repos: Vec<String>,
    /// The deploy-pinned org whitelist for `POST /api/repos` (Lane O3): repeatable, or
    /// comma-separated via the env var. Empty ⇒ locked closed — no new repo can be added via the
    /// API regardless of caller (matching `Roles`' empty-list convention) — except a repo that was
    /// itself env-seeded via `--repo`/`CONTROLLER_WATCHED_REPOS` above, which is exempt (it was
    /// already operator-provisioned at deploy time). **Not** part of the Lane O2 runtime-override
    /// set: it picks which orgs may ever have a repo admitted, the same restart-only-by-design
    /// class as the admin list or the watched-repo seed, not a retunable cap.
    #[arg(
        long = "allowed-org",
        env = "CONTROLLER_ALLOWED_ORGS",
        value_delimiter = ','
    )]
    pub allowed_orgs: Vec<String>,
    /// The namespace the daemon's pod watch lists/watches — the loop namespace the launch
    /// path renders into (`deploy render --controller`'s Role grants get/list/watch there).
    #[arg(
        long = "pod-namespace",
        env = "CONTROLLER_POD_NAMESPACE",
        default_value = "autoresearch"
    )]
    pub pod_namespace: String,
    /// The service account the turn pods run as (`[cluster].service_account` in the deploy
    /// profile). The Tier 2 ingest drop-box's TokenReview extractor requires an uploader's token
    /// to belong to exactly this SA in `pod_namespace`. Unset accepts any SA in the namespace,
    /// which is looser than ideal — the Helm chart sets it to pin the exact account.
    #[arg(long = "turn-service-account", env = "CONTROLLER_TURN_SERVICE_ACCOUNT")]
    pub turn_service_account: Option<String>,
    /// The cluster new turn/run pods dispatch to: `hub` (the controller's own cluster) or a
    /// spoke name resolvable under `clusters_dir`. Each row records its cluster, so changing
    /// this affects only new dispatches; in-flight pods are driven on their recorded cluster.
    #[arg(
        long = "dispatch-cluster",
        env = "CONTROLLER_DISPATCH_CLUSTER",
        default_value = crate::runs::clusters::HUB_CLUSTER
    )]
    pub dispatch_cluster: String,
    /// Directory of spoke credentials, one `<name>/kubeconfig` per cluster (the chart mounts a
    /// kubeconfig Secret plus a projected SA token per spoke). Unset = hub-only dispatch.
    #[arg(long = "clusters-dir", env = "CONTROLLER_CLUSTERS_DIR")]
    pub clusters_dir: Option<std::path::PathBuf>,
    /// Who may dispatch to which shared cluster, as `cluster=principal[;principal]` entries
    /// (`wharf=group:/groups/llm-d;user:alice`). A cluster with no entry is open to every
    /// authenticated caller, which is what a deployment that configures none keeps. Malformed
    /// entries fail the startup validation.
    #[arg(
        long = "cluster-policy",
        env = "CONTROLLER_CLUSTER_POLICY",
        value_delimiter = ','
    )]
    pub cluster_policy: Vec<String>,
    /// Which cluster a GPU-measured contract's work dispatches onto, as `contract=cluster` entries
    /// (`vllm=wharf`). Consulted only when the issue named no target of its own, and only for an
    /// issue adopted against a named `codegen_contract`; anything else takes `dispatch_cluster`.
    #[arg(
        long = "dispatch-cluster-by-contract",
        env = "CONTROLLER_DISPATCH_CLUSTER_BY_CONTRACT",
        value_delimiter = ','
    )]
    pub dispatch_cluster_by_contract: Vec<String>,
    /// The turn service account on each spoke, as `cluster=sa` entries. A spoke pod's ingest token
    /// is TokenReviewed against its own cluster, where the service account is the spoke's rather
    /// than the hub's; a spoke with no entry uses `turn_service_account`'s value (a name, or any
    /// service account in that spoke's namespace when it too is unset). The spoke's namespace is
    /// not configured here: it comes from the spoke's kubeconfig context. Malformed entries fail
    /// the startup validation.
    #[arg(
        long = "spoke-service-accounts",
        env = "CONTROLLER_SPOKE_SERVICE_ACCOUNTS",
        value_delimiter = ','
    )]
    pub spoke_service_accounts: Vec<String>,
    /// The TCP port a loop pod's in-process control bridge listens on, dialed by the read-only live
    /// relay (`GET /api/runs/:id/live`). Must match the `--control-port` the rendered loop-pod
    /// wrapper passes (`crucible/src/deploy/render.rs`, currently 7777).
    #[arg(
        long = "control-port",
        env = "CONTROLLER_CONTROL_PORT",
        default_value_t = default_control_port()
    )]
    pub control_port: u16,
    /// DEPRECATED alias for `allowed_tiers`: `true` behaves exactly like adding `t3` to
    /// `allowed_tiers` (a union, not a replacement) — kept working this release so an existing
    /// `CONTROLLER_ALLOW_T3=true` deploy doesn't silently regress, but new deploys should set
    /// `allowed_tiers`/`CONTROLLER_ALLOWED_TIERS` directly. Off by default: v1 has no composite
    /// GPU rig to scope a T3 issue's gate against.
    #[arg(
        long = "allow-t3",
        env = "CONTROLLER_ALLOW_T3",
        default_value_t = false
    )]
    pub allow_t3: bool,
    /// The tiers the autopilot's pick will scope: everything else is deferred (never spends a
    /// scope turn) regardless of what the ranker confirmed. Default `t0,t1`: T1 has a real
    /// propose+refine+adversary pipeline, so it's scopeable out of the box; T2/T3 have no working
    /// propose path yet (see `scope-propose.md`'s "v1 targets T0 and T1"), so they stay excluded
    /// until a real rig backend lands. `allow_t3 = true` unions T3 into this set (the deprecated
    /// alias above) rather than being consulted separately.
    #[arg(
        long = "allowed-tier",
        env = "CONTROLLER_ALLOWED_TIERS",
        value_delimiter = ',',
        default_value = "t0,t1"
    )]
    pub allowed_tiers: Vec<Tier>,
    /// Require a code-grounded confirmation before every scope-turn spend, on top of the cheap
    /// API tier. On by default: a 20-issue comparison found grounded ranking runs ~9x the API
    /// arm's cost but catches the failure mode that matters most — an already-implemented fix or
    /// an unattackable issue the text-only ranker mistiered T0/T1/T2 — before a scope turn (~20x
    /// the grounded turn's own cost) gets wasted proposing against it. Turn off for GPU-free /
    /// quick-loop scenarios where eating an occasional wasted scope turn is cheaper than the
    /// grounded checkout + turn on every issue.
    #[arg(
        long = "prescope-grounded",
        env = "CONTROLLER_PRESCOPE_GROUNDED",
        default_value_t = true
    )]
    pub prescope_grounded: bool,
    /// Rank horizon in days: only consider issues with upstream activity in the last N days.
    /// A NULL or older upstream_updated_at gets machine-parked with "stale: no upstream activity
    /// in {N} days". 0 disables the gate.
    #[arg(
        long = "rank-horizon-days",
        env = "CONTROLLER_RANK_HORIZON_DAYS",
        default_value_t = 0
    )]
    pub rank_horizon_days: u32,
    /// How grounded ranking turns run: `pod` (dispatch a WorkPod, the documented in-cluster
    /// opt-in), `local` (shell `claude` in-process, dev-machine only), or `disabled` (skip
    /// grounding — the default, so an env-less deploy boots instead of failing the startup
    /// validation for a profile it never configured). Prerequisites for the chosen mode are
    /// checked at startup ([`ControllerCfg::validate_grounded`]).
    #[arg(
        long = "grounded-executor",
        env = "CONTROLLER_GROUNDED_EXECUTOR",
        value_enum,
        default_value_t = GroundedExecutor::Disabled
    )]
    pub grounded_executor: GroundedExecutor,
    /// The deploy profile the WorkPod render reads (namespaces, secrets, images). Required when
    /// `grounded_executor = pod`. Mirrors the loop launch path's `CONTROLLER_DEPLOY_PROFILE`.
    #[arg(long = "deploy-profile", env = "CONTROLLER_DEPLOY_PROFILE")]
    pub deploy_profile: Option<PathBuf>,
    /// Render pod image tags verbatim instead of resolving them to `@sha256:…` digests through the
    /// registry (an air-gapped cluster, or a test with no registry to reach).
    #[arg(long = "render-no-pin", env = "CONTROLLER_RENDER_NO_PIN")]
    pub render_no_pin: bool,
    /// The agent sandbox image a grounded-rank WorkPod runs (carries the `claude` CLI). Required when
    /// `grounded_executor = pod`.
    #[arg(
        long = "grounded-sandbox-image",
        env = "CONTROLLER_GROUNDED_SANDBOX_IMAGE"
    )]
    pub grounded_sandbox_image: Option<String>,
    /// GitHub logins with admin privileges (repeatable, or comma-separated via the env var).
    /// Empty = no admins (admin endpoints 403 for everyone, locked-closed). Admins are implicitly
    /// operators too.
    #[arg(long = "admin", env = "CONTROLLER_ADMINS", value_delimiter = ',')]
    pub admins: Vec<String>,
    /// GitHub logins with operator privileges — curation actions (park/unpark/bump) but not money
    /// or config (repeatable, or comma-separated via the env var). Empty = no operators
    /// (operator endpoints 403 for everyone but admins, locked-closed).
    #[arg(long = "operator", env = "CONTROLLER_OPERATORS", value_delimiter = ',')]
    pub operators: Vec<String>,
    /// IdP groups whose members hold operator privileges (the proxy forwards the OIDC `groups`
    /// claim as `x-auth-request-groups`). Matched verbatim or against the asserted group's last
    /// `/`-segment. Groups never grant admin.
    #[arg(
        long = "operator-group",
        env = "CONTROLLER_OPERATOR_GROUPS",
        value_delimiter = ','
    )]
    pub operator_groups: Vec<String>,
    /// Mark the session cookie `Secure`. On by default — prod is https at the oauth2-proxy edge.
    /// Set `CONTROLLER_SESSION_SECURE=false` for plain-http local dev, or browsers drop the cookie.
    #[arg(
        long = "session-secure-cookies",
        env = "CONTROLLER_SESSION_SECURE",
        default_value_t = true
    )]
    pub session_secure_cookies: bool,
    /// The name of the ConfigMap the runtime override store watches (Lane O2). Its data lives under
    /// one key (`overrides.json`); an operator retunes the overridable caps/toggles by editing it.
    #[arg(
        long = "overrides-configmap",
        env = "CONTROLLER_OVERRIDES_CONFIGMAP",
        default_value = crate::daemon::overrides_store::DEFAULT_CONFIGMAP_NAME
    )]
    pub overrides_configmap: String,
    /// The namespace the overrides ConfigMap lives in — the controller's OWN namespace, not the loop
    /// `pod_namespace`. Defaults to `pod_namespace` when unset; the deploy render points it at the
    /// release namespace via the downward API.
    #[arg(long = "overrides-namespace", env = "CONTROLLER_OVERRIDES_NAMESPACE")]
    pub overrides_namespace: Option<String>,
    /// The runtime autopilot flag, loaded from its ledger row after parsing. `None` in tests
    /// (treated as enabled). Set by the daemon before handing the cfg to the reconcile wiring.
    #[cfg(feature = "autoresearch")]
    #[arg(skip)]
    pub autopilot: Option<crate::daemon::autopilot_flag::AutopilotFlag>,
    /// Run the autoresearch lane: GitHub discovery, ranking, scoping, the approval gate, builds and
    /// scored loop runs, with their routes. Off, the controller runs playbooks only. Needs a build
    /// with the `autoresearch` feature.
    #[arg(
        long = "autoresearch",
        env = "CONTROLLER_AUTORESEARCH",
        default_value_t = false
    )]
    pub autoresearch: bool,
    /// The runtime override store (Lane O2), threaded in after parsing — the same `#[arg(skip)]`
    /// handle shape as `autopilot`. `None` in tests + `--once` (the effective config is then exactly
    /// the parsed defaults/env). Cloneable (an `Arc` inside), so every `cfg.clone()` shares the one
    /// live override cache the watch swaps.
    #[arg(skip)]
    pub overrides: Option<crate::daemon::overrides_store::ConfigStore>,
    /// The GitHub App installation-token source for the pack-PR credential, threaded in after
    /// parsing from the `CONTROLLER_GITHUB_APP_*` env vars (the same `#[arg(skip)]` handle shape
    /// as `autopilot`). `None` ⇒ the PAT chain (`AUTORESEARCH_PR_TOKEN`/…) applies. Cloneable
    /// (an `Arc` inside), so every `cfg.clone()` shares the one token cache.
    #[arg(skip)]
    pub github_app: Option<crate::secrets::github_app::GithubAppTokenSource>,
    /// How scope-propose turns run: `pod` (dispatch a WorkPod), `local` (shell `crucible scope`
    /// in-process, the compatible default), or `disabled` (skip autopilot scoping). `local` is the
    /// default so existing deploys keep working without config changes.
    #[arg(
        long = "scope-executor",
        env = "CONTROLLER_SCOPE_EXECUTOR",
        value_enum,
        default_value_t = ScopeExecutor::Local
    )]
    pub scope_executor: ScopeExecutor,
    /// The agent sandbox image a scope WorkPod runs. Falls back to `grounded_sandbox_image` when
    /// unset (the same image typically carries both commands).
    #[arg(long = "scope-sandbox-image", env = "CONTROLLER_SCOPE_SANDBOX_IMAGE")]
    pub scope_sandbox_image: Option<String>,
    /// Publish-on-keep fork map: `owner/repo=fork_owner/fork_repo` entries, repeatable or
    /// comma-separated (e.g. `neuralmagic/relay-testbed=wren/relay-testbed`). When a dispatched
    /// loop run keeps a candidate, its kept commits open a DRAFT PR against the mapped FORK, never
    /// upstream. A watched repo with no mapping opens no PR (its S3 record still lands). A pack's own
    /// `[publish] pr_repo` overrides its entry here. The push PAT rides the loop profile's secret env
    /// (`AUTORESEARCH_PR_TOKEN`) — this map only picks the target repo.
    #[arg(
        long = "pr-repo-map",
        env = "CONTROLLER_PR_REPO_MAP",
        value_delimiter = ','
    )]
    pub pr_repo_map: Vec<String>,
    /// Install the real build backends (`forge::build::dispatch_cluster` /
    /// `forge::github::dispatch_github`) at daemon assembly. Off by default: an env-less deploy
    /// keeps the not-installed stub, so a pack that declares a `[build]` block parks with a clear
    /// reason
    /// rather than dispatching a build no one configured creds for. On requires the per-backend creds
    /// below for the backends a pack actually uses.
    #[arg(
        long = "build-backends",
        env = "CONTROLLER_BUILD_BACKENDS",
        default_value_t = false
    )]
    pub build_backends: bool,
    /// Path to the registry authfile (a Docker `config.json`) the contract check reads dispatch
    /// image labels with. `image.pullSecrets` is the kubelet's, not this process's, so a private
    /// loop image needs the same credential mounted here or the label read 401s. Unset falls back
    /// to the process's own `REGISTRY_AUTH_FILE`/docker config, else anonymous.
    #[arg(long = "registry-authfile", env = "CONTROLLER_REGISTRY_AUTHFILE")]
    pub registry_authfile: Option<PathBuf>,
    /// Repositories the image catalog watches (`GET /api/images`): repeatable, or comma-separated
    /// via the env var, each a registry/repository reference without tag. Read with
    /// `--registry-authfile`. Empty leaves the catalog off.
    #[arg(
        long = "image-catalog-repo",
        env = "CONTROLLER_IMAGE_CATALOG_REPOS",
        value_delimiter = ','
    )]
    pub image_catalog_repos: Vec<String>,
    /// Seconds between image catalog sweeps.
    #[arg(
        long = "image-catalog-interval-secs",
        env = "CONTROLLER_IMAGE_CATALOG_INTERVAL_SECS",
        default_value_t = 600
    )]
    pub image_catalog_interval_secs: u64,
    /// Path to the registry PUSH authfile (a Docker `config.json`) the cluster build backend seeds
    /// its in-cluster push secret from, and the github backend resolves a private destination digest
    /// with. Mounted from the deploy's `quay-authfile`-style secret. Required when a `cluster` build
    /// dispatches; a `github-actions` build passes it to `pin_digest` only for a private dest.
    #[arg(long = "build-push-authfile", env = "CONTROLLER_BUILD_PUSH_AUTHFILE")]
    pub build_push_authfile: Option<PathBuf>,
    /// The GitHub token the `github-actions` build backend dispatches + polls with
    /// (`workflow_dispatch` needs `actions:write`). From the deploy secret env; required when a
    /// `github-actions` build dispatches. Never read from the manifest — creds live controller-side.
    #[arg(long = "build-github-token", env = "CONTROLLER_BUILD_GITHUB_TOKEN")]
    pub build_github_token: Option<String>,
    /// The git repo a `cluster` build clones its context from: forge clones `git_url@ref` and builds
    /// the declared `containerfile` under `context`. The domain Containerfiles live in THIS repo (the
    /// crucible repo hosting `domains/`), NOT the issue's upstream repo. Required when a `cluster`
    /// build dispatches.
    #[arg(
        long = "build-context-git-url",
        env = "CONTROLLER_BUILD_CONTEXT_GIT_URL"
    )]
    pub build_context_git_url: Option<String>,
    /// The git ref a `cluster` build clones (default `main`).
    #[arg(
        long = "build-context-git-ref",
        env = "CONTROLLER_BUILD_CONTEXT_GIT_REF",
        default_value = "main"
    )]
    pub build_context_git_ref: String,
    /// Path to a file holding a git token the `cluster` build's CLONE init container authenticates
    /// with, so a PRIVATE context repo (e.g. `neuralmagic/crucible` for the self-host loop) clones.
    /// Mounted from the deploy's git-token secret (like the push authfile); the token is read
    /// controller-side and seeded into a per-Job secret — never the manifest, never argv. Absent ⇒
    /// anonymous clone (public repos unchanged).
    #[arg(
        long = "build-context-git-token-file",
        env = "CONTROLLER_BUILD_GIT_TOKEN_FILE"
    )]
    pub build_context_git_token_file: Option<PathBuf>,
    /// The Jira Cloud base URL the adopt-by-key path fetches issues from (e.g.
    /// `https://example.atlassian.net`). Controller-side only — the loop never touches Jira. All
    /// three `jira_*` fields must be set for `POST /api/jira` to work; any unset ⇒ adopt disabled.
    #[arg(long = "jira-base-url", env = "JIRA_BASE_URL")]
    pub jira_base_url: Option<String>,
    /// The account email the Jira API token belongs to (basic-auth username).
    #[arg(long = "jira-email", env = "JIRA_EMAIL")]
    pub jira_email: Option<String>,
    /// A Jira Cloud API token (basic-auth password). From the deploy secret env; confidential, never
    /// logged. Read controller-side and never handed to the sandbox.
    #[arg(long = "jira-api-token", env = "JIRA_API_TOKEN")]
    pub jira_api_token: Option<String>,
    /// The externally reachable base URL of this controller's SPA (e.g.
    /// `https://crucible.example.com`), used to build absolute deep-links in tracker write-back
    /// comments. Unset ⇒ comments omit the link.
    #[arg(long = "public-url", env = "CONTROLLER_PUBLIC_URL")]
    pub public_url: Option<String>,
    /// The Jira project emitted experiment epics/tasks land in (e.g. `ACME`). All three
    /// `jira_emission_*` fields plus the Jira creds must be set for emission to be on.
    #[arg(long = "jira-emission-project", env = "JIRA_EMISSION_PROJECT")]
    pub jira_emission_project: Option<String>,
    /// Jira issue type id for the experiment container (the epic type; instance-specific).
    #[arg(
        long = "jira-emission-epic-type-id",
        env = "JIRA_EMISSION_EPIC_TYPE_ID"
    )]
    pub jira_emission_epic_type_id: Option<String>,
    /// Jira issue type id for per-PR review tasks (instance-specific).
    #[arg(
        long = "jira-emission-task-type-id",
        env = "JIRA_EMISSION_TASK_TYPE_ID"
    )]
    pub jira_emission_task_type_id: Option<String>,
    /// Labels stamped on every emitted issue (comma-separated, e.g. `agentops`).
    #[arg(
        long = "emission-labels",
        env = "EMISSION_LABELS",
        value_delimiter = ','
    )]
    pub emission_labels: Vec<String>,
    /// The named broker codegen tool contracts a scenario may be adopted against. A scenario that
    /// names one is GPU-measured: its scope turn renders with `--broker-measure`, and its loop pod
    /// gets the contract projected as `BROKER_CODEGEN_TOOLS_OVERLAY`, which the broker merges over
    /// `BROKER_CODEGEM_TOOLS_DEFAULTS`. Empty (the default) ⇒ no scenario can ask for one and the
    /// New Scenario form's select is empty; every adoption stays local-measure.
    ///
    /// Parsed by [`parse_broker_contracts`] at clap time, so a malformed value fails the boot with a
    /// readable error instead of 422-ing the first adoption that names a contract.
    #[arg(
        long = "broker-contracts",
        env = "CONTROLLER_BROKER_CONTRACTS",
        value_parser = parse_broker_contracts,
        default_value = ""
    )]
    pub broker_contracts: BrokerContracts,
    /// Upper bound on the per-run cost ceiling a playbook launcher may ask for, in USD. Ceilings
    /// are the launcher's — a pack may not declare them — so this is the only thing standing
    /// between a form field and the daily ledger.
    #[arg(
        long = "playbook-max-cost-cap",
        env = "CONTROLLER_PLAYBOOK_MAX_COST_USD",
        default_value_t = default_playbook_max_cost_cap()
    )]
    pub playbook_max_cost_cap: f64,
    /// Upper bound on the wall-clock ceiling a playbook launcher may ask for, in the engine's
    /// duration grammar (`90s`, `30m`, `2h`).
    #[arg(
        long = "playbook-max-time-cap",
        env = "CONTROLLER_PLAYBOOK_MAX_TIME",
        value_parser = crate::model::MaxTime::parse,
        default_value_t = crate::model::MaxTime::hours(DEFAULT_PLAYBOOK_MAX_TIME_HOURS)
    )]
    pub playbook_max_time_cap: crate::model::MaxTime,
    /// How a playbook launch's engine runs: `pod` (a controller-owned work pod, the default) or
    /// `local` (a supervised subprocess on this machine). Production stays `pod` unless a
    /// deployment opts in; the laptop flow is the reason `local` exists.
    #[arg(
        long = "playbook-executor",
        env = "CONTROLLER_PLAYBOOK_EXECUTOR",
        value_enum,
        default_value_t = PlaybookExecutor::Pod
    )]
    pub playbook_executor: PlaybookExecutor,
    /// How many consecutive failed firings take a playbook schedule out of the rotation. A
    /// schedule whose pack was deregistered, or whose stored ceilings no longer render, fails
    /// every window; this is the deployment's patience for that before the sweep disables it.
    #[arg(
        long = "schedule-auto-disable-failures",
        env = "CONTROLLER_SCHEDULE_AUTO_DISABLE_FAILURES",
        default_value_t = DEFAULT_SCHEDULE_AUTO_DISABLE_FAILURES
    )]
    pub schedule_auto_disable_failures: i64,
    /// How long a schedule's snapshot of its owner's groups stays usable at fire time.
    #[arg(
        long = "schedule-owner-ttl-secs",
        env = "CONTROLLER_SCHEDULE_OWNER_TTL_SECS",
        default_value_t = DEFAULT_SCHEDULE_OWNER_TTL_SECS
    )]
    pub schedule_owner_ttl_secs: u64,
    /// The environment variables a local-mode run may inherit from the controller's own
    /// environment, beyond `PATH` and the run's `CRUCIBLE_*` set. Local mode has no registry, so
    /// this is the operator's whole say over what a subprocess can read.
    #[arg(
        long = "local-secret-allowlist",
        env = "CONTROLLER_LOCAL_SECRET_ALLOWLIST",
        value_delimiter = ',',
        default_value = ""
    )]
    pub local_secret_allowlist: Vec<String>,
    #[command(flatten)]
    pub profile: Profile,
}

/// The configured `name -> contract JSON` set, keyed by the name an adoption cites. The values are
/// normalized (re-serialized, compact) JSON objects: whatever shape the operator wrote, what leaves
/// here is what lands in the loop pod's `BROKER_CODEGEN_TOOLS_OVERLAY` and in the scope agent's goal
/// framing — byte for byte the same string in both places, so the pack the agent writes and the
/// overlay the broker reads can never describe different measurements.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrokerContracts(BTreeMap<String, String>);

impl BrokerContracts {
    /// The configured names, sorted — what `GET /api/config/broker-contracts` serves and the New
    /// Scenario form's select is built from.
    pub fn names(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }

    /// The contract JSON for `name`, or `None` when nothing is configured under it (a 422 at
    /// adoption; a failed run dispatch afterwards, if a redeploy dropped the entry).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Build a set directly, for tests and callers that already hold parsed contracts.
    pub fn from_map(map: BTreeMap<String, String>) -> Self {
        BrokerContracts(map)
    }
}

/// Parse `CONTROLLER_BROKER_CONTRACTS`: a JSON object mapping a contract name to its contract, in
/// either of two shapes per entry —
///   * a JSON **string** holding the contract JSON (what a Helm values map yields, since a chart
///     carries each contract as one quoted scalar), or
///   * the contract **object** written inline (easier to read in a hand-edited env var).
///
/// Both normalize to the same compact JSON string. Blank ⇒ no contracts, which is the default deploy.
///
/// The contract body is checked only for being a JSON object. The schema that actually constrains it
/// (`crucible_broker::codegen::config::ToolsOverlay`) lives in the broker, which the controller does
/// not link; duplicating it here would give two definitions to drift apart. A contract that parses
/// but does not match the overlay schema fails in the broker, with the broker's own error.
fn parse_broker_contracts(raw: &str) -> Result<BrokerContracts, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(BrokerContracts::default());
    }
    let root: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| format!("CONTROLLER_BROKER_CONTRACTS is not valid JSON: {e}"))?;
    let serde_json::Value::Object(entries) = root else {
        return Err(
            "CONTROLLER_BROKER_CONTRACTS must be a JSON object of contract name -> contract"
                .to_string(),
        );
    };
    let mut out = BTreeMap::new();
    for (name, value) in entries {
        if name.trim().is_empty() {
            return Err("CONTROLLER_BROKER_CONTRACTS has a blank contract name".to_string());
        }
        let contract = match value {
            serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(&s)
                .map_err(|e| format!("contract {name:?} is not valid JSON: {e}"))?,
            v => v,
        };
        if !contract.is_object() {
            return Err(format!(
                "contract {name:?} must be a JSON object (or a JSON string holding one)"
            ));
        }
        let normalized = serde_json::to_string(&crate::model::sorted_json(contract))
            .map_err(|e| format!("contract {name:?} could not be re-serialized: {e}"))?;
        out.insert(name, normalized);
    }
    Ok(BrokerContracts(out))
}

impl ControllerCfg {
    /// How a pod render pins its image tags: the registry, unless `render_no_pin` is set.
    pub fn digest_resolver(&self) -> Option<std::sync::Arc<dyn crucible::deploy::DigestResolver>> {
        match self.render_no_pin {
            true => None,
            false => Some(std::sync::Arc::new(crucible::deploy::RegistryDigests)),
        }
    }

    /// The resolved Jira creds for the adopt-by-key path, or `None` when any of the three
    /// `jira_*` fields is unset (adopt then answers a clear "not configured" error).
    pub fn jira_config(&self) -> Option<crate::launches::jira::JiraConfig> {
        crate::launches::jira::JiraConfig::from_parts(
            self.jira_base_url.clone(),
            self.jira_email.clone(),
            self.jira_api_token.clone(),
        )
    }

    /// Resolve the publish-on-keep fork for a watched repo (`owner/repo`) from [`Self::pr_repo_map`]:
    /// the first `from=to` entry whose left side equals `repo`, with a non-empty right side. `None` =
    /// no mapping, so a dispatched run of that repo opens no PR unless its pack names a `[publish]
    /// pr_repo`. A malformed entry (no `=`, blank side) is skipped rather than failing the lookup.
    pub(crate) fn pr_repo_for(&self, repo: &str) -> Option<String> {
        self.pr_repo_map.iter().find_map(|entry| {
            let (from, to) = entry.split_once('=')?;
            (from.trim() == repo && !to.trim().is_empty()).then(|| to.trim().to_string())
        })
    }

    /// The overridable knobs as this configuration resolved them: the defaults layer the override
    /// store snapshots once when it is built.
    pub fn base_config(&self) -> crate::daemon::overrides_store::BaseConfig {
        crate::daemon::overrides_store::BaseConfig {
            discovery_secs: self.profile.discovery_secs,
            per_reconcile_cost: self.profile.per_reconcile_cost,
            max_concurrent_pods: self.profile.max_concurrent_pods,
            max_scopes_per_day: self.profile.max_scopes_per_day,
            daily_cost_ceiling: self.profile.daily_cost_ceiling,
            rank_cost_fallback_usd: self.profile.rank_cost_fallback_usd,
            grounded_rank_pod_cap: self.profile.grounded_rank_pod_cap,
            grounded_rank_daily_turns: self.profile.grounded_rank_daily_turns,
            failed_pod_keep: self.profile.failed_pod_keep,
            scope_gaming_rounds: self.profile.scope_gaming_rounds,
            scope_skip_gaming_review: self.profile.scope_skip_gaming_review,
            allow_t3: self.allow_t3,
            prescope_grounded: self.prescope_grounded,
            rank_horizon_days: self.rank_horizon_days,
            allowed_tiers: self.allowed_tiers.clone(),
            run_iterations: self.profile.run_iterations,
            run_max_cost: self.profile.run_max_cost,
            allow_draft_head_schedules: false,
        }
    }

    /// The effective config for exactly the overridable knobs, resolving `default < env < override`
    /// (Lane O2). The reconcile/dispatch paths read this per cycle instead of `self.profile.*` /
    /// `self.allow_t3` directly, so a runtime override in the watched ConfigMap takes effect without
    /// a restart. With no override store threaded (tests, `--once`), it's exactly the parsed values.
    pub fn effective(&self) -> crate::daemon::overrides_store::EffectiveConfig {
        match &self.overrides {
            Some(store) => store.effective(),
            None => crate::daemon::overrides_store::EffectiveConfig::resolve(
                &self.base_config(),
                &crate::daemon::overrides_store::OverrideSet::default(),
            ),
        }
    }

    /// The namespace the overrides ConfigMap lives in: the explicit `--overrides-namespace`, else
    /// the loop `pod_namespace` as a fallback.
    pub fn overrides_namespace(&self) -> String {
        self.overrides_namespace
            .clone()
            .unwrap_or_else(|| self.pod_namespace.clone())
    }

    /// The `POST /api/repos` org whitelist + env-seeded-repo exemption built off this parsed
    /// config — see [`crate::issues::repo_ref::RepoWhitelist`].
    #[cfg(feature = "autoresearch")]
    pub fn repo_whitelist(&self) -> crate::issues::repo_ref::RepoWhitelist {
        crate::issues::repo_ref::RepoWhitelist::new(self.allowed_orgs.clone(), self.repos.clone())
    }

    /// The Postgres ledger URL (`--db` / `DATABASE_URL`).
    pub fn db_url(&self) -> &str {
        &self.db
    }

    /// The scratch root: `--scratch-dir` when set, else `state_dir`.
    pub(crate) fn scratch_root(&self) -> &Path {
        self.scratch_dir.as_deref().unwrap_or(&self.state_dir)
    }

    /// Whether machine-initiated spend is allowed: `true` when the autopilot flag is absent
    /// (tests, no flag loaded) or explicitly enabled; `false` when an admin disabled it via
    /// `POST /api/autopilot`.
    #[cfg(feature = "autoresearch")]
    pub(crate) fn autopilot_enabled(&self) -> bool {
        self.autopilot.as_ref().is_none_or(|f| f.is_enabled())
    }

    /// Whether the autoresearch lane runs: built with the feature and switched on.
    pub fn autoresearch_enabled(&self) -> bool {
        cfg!(feature = "autoresearch") && self.autoresearch
    }

    /// Whether `DATABASE_URL` asks the daemon to run its own Postgres.
    pub fn wants_embedded_db(&self) -> bool {
        self.db == EMBEDDED_DATABASE_URL
    }

    /// Refuse `DATABASE_URL=embedded` on a build without the feature.
    pub fn validate_database(&self) -> Result<()> {
        if self.wants_embedded_db() && !cfg!(feature = "embedded-db") {
            bail!(
                "DATABASE_URL=embedded, but this crucible-controller was built without the embedded-db feature"
            );
        }
        Ok(())
    }

    /// Refuse `CONTROLLER_AUTORESEARCH=true` on a build without the feature.
    pub fn validate_autoresearch(&self) -> Result<()> {
        if self.autoresearch && !cfg!(feature = "autoresearch") {
            bail!(
                "CONTROLLER_AUTORESEARCH is on, but this crucible-controller was built without the autoresearch feature"
            );
        }
        Ok(())
    }

    /// Fail loudly at startup if the chosen [`GroundedExecutor`] is missing a prerequisite, so a
    /// misconfiguration surfaces once at boot rather than silently every verdict (the exact failure
    /// mode the old `CONTROLLER_SANDBOX_IMAGE` gate had). `pod` needs a deploy profile + a sandbox
    /// image to render the turn pod; `local`/`disabled` need nothing.
    pub fn validate_grounded(&self) -> Result<()> {
        if self.grounded_executor == GroundedExecutor::Pod {
            if self.deploy_profile.is_none() {
                bail!(
                    "grounded_executor = pod needs a deploy profile: set CONTROLLER_DEPLOY_PROFILE \
                     (or --deploy-profile) to the profile the WorkPod render reads"
                );
            }
            if self
                .grounded_sandbox_image
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            {
                bail!(
                    "grounded_executor = pod needs a sandbox image: set \
                     CONTROLLER_GROUNDED_SANDBOX_IMAGE (or --grounded-sandbox-image) to the agent \
                     sandbox carrying the claude CLI"
                );
            }
        }
        Ok(())
    }

    /// The per-cluster turn service accounts the ingest drop-box validates uploaders against.
    /// Parsed at startup so a `spoke_service_accounts` entry that is not `cluster=sa` fails the
    /// boot rather than being ignored.
    pub fn turn_accounts(&self) -> Result<TurnAccounts> {
        let spokes = parse_kv_list(
            &self.spoke_service_accounts,
            "CONTROLLER_SPOKE_SERVICE_ACCOUNTS",
            "cluster=serviceaccount",
        )?;
        // The hub's account has its own knob; accepting it here would give one cluster two
        // sources of truth.
        if let Some(sa) = spokes.get(crate::runs::clusters::HUB_CLUSTER) {
            let entry = format!("{}={sa}", crate::runs::clusters::HUB_CLUSTER);
            bail!(
                "CONTROLLER_SPOKE_SERVICE_ACCOUNTS entry `{entry}` names the hub: set the hub's \
                 account with CONTROLLER_TURN_SERVICE_ACCOUNT"
            );
        }
        Ok(TurnAccounts {
            hub: self.turn_service_account.clone(),
            spokes,
        })
    }

    /// The contract-to-cluster routing map. Parsed at startup so a malformed entry fails the boot
    /// rather than silently routing to the default, which is what the unread env did before.
    pub fn contract_clusters(&self) -> Result<BTreeMap<String, String>> {
        parse_kv_list(
            &self.dispatch_cluster_by_contract,
            "CONTROLLER_DISPATCH_CLUSTER_BY_CONTRACT",
            "contract=cluster",
        )
    }

    /// Validate the [`ScopeExecutor`] prerequisites at startup, mirroring [`validate_grounded`].
    /// `pod` needs a deploy profile and at least one sandbox image (its own or the grounded fallback).
    pub fn validate_scope(&self) -> Result<()> {
        if self.scope_executor == ScopeExecutor::Pod {
            if self.deploy_profile.is_none() {
                bail!(
                    "scope_executor = pod needs a deploy profile: set CONTROLLER_DEPLOY_PROFILE \
                     (or --deploy-profile)"
                );
            }
            let has_image = self
                .scope_sandbox_image
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some()
                || self
                    .grounded_sandbox_image
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .is_some();
            if !has_image {
                bail!(
                    "scope_executor = pod needs a sandbox image: set \
                     CONTROLLER_SCOPE_SANDBOX_IMAGE (or --scope-sandbox-image), or the grounded \
                     sandbox image as a fallback"
                );
            }
        }
        Ok(())
    }
}

/// Parse a repeatable `key=value` list into a map, trimming each side. A blank entry is skipped; an
/// entry missing either side fails so a typo in `env_name` stops the boot rather than being ignored.
fn parse_kv_list(
    entries: &[String],
    env_name: &str,
    shape: &str,
) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for raw in entries {
        let entry = raw.trim();
        if entry.is_empty() {
            continue;
        }
        let (key, value) = entry.split_once('=').unwrap_or(("", ""));
        if key.trim().is_empty() || value.trim().is_empty() {
            bail!("{env_name} entry `{entry}` is not `{shape}`");
        }
        map.insert(key.trim().to_string(), value.trim().to_string());
    }
    Ok(map)
}

/// The service accounts an uploader's token may belong to, per cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnAccounts {
    /// The hub's turn service account (`CONTROLLER_TURN_SERVICE_ACCOUNT`). `None` accepts any
    /// service account in the hub's loop namespace.
    pub hub: Option<String>,
    /// Per-spoke turn service accounts (`CONTROLLER_SPOKE_SERVICE_ACCOUNTS`). A spoke with no
    /// entry uses [`TurnAccounts::hub`]'s value: its name, or any service account in that spoke's
    /// namespace when the hub's is unset.
    pub spokes: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {

    /// The routing knob the chart had been setting into a void: no code read
    /// `CONTROLLER_DISPATCH_CLUSTER_BY_CONTRACT`, so an operator's `vllm=hub` did nothing.
    #[test]
    fn contract_routing_parses_pairs_and_refuses_the_rest() {
        let cfg = |entries: &[&str]| {
            let mut cfg = crate::testing::cfg_from_args(["ctl"]);
            cfg.dispatch_cluster_by_contract = entries.iter().map(|e| e.to_string()).collect();
            cfg
        };
        let map = cfg(&["vllm=hub", " deepgemm = wharf "])
            .contract_clusters()
            .expect("parses");
        assert_eq!(map.get("vllm").map(String::as_str), Some("hub"));
        assert_eq!(
            map.get("deepgemm").map(String::as_str),
            Some("wharf"),
            "entries are trimmed on both sides"
        );
        assert!(cfg(&[""]).contract_clusters().expect("blank ok").is_empty());
        assert!(cfg(&["vllm"]).contract_clusters().is_err(), "no `=`");
        assert!(cfg(&["=hub"]).contract_clusters().is_err(), "no contract");
        assert!(cfg(&["vllm="]).contract_clusters().is_err(), "no cluster");
    }
    use super::*;

    #[test]
    fn profile_defaults_match_adr_caps() {
        let p = Profile::default();
        assert_eq!(p.discovery_interval(), Duration::from_secs(300));
        assert_eq!(p.max_concurrent_pods, 2);
        assert_eq!(p.max_scopes_per_day, 20);
    }

    #[test]
    fn profile_toml_fills_defaults_for_absent_keys() {
        // A partial `[controller]` block: the unset caps fall back to defaults.
        let p: Profile = toml::from_str("max_concurrent_pods = 4\n").expect("parse");
        assert_eq!(p.max_concurrent_pods, 4);
        assert_eq!(p.discovery_secs, default_discovery_secs());
    }

    #[test]
    fn profile_toml_rejects_unknown_keys() {
        let err = toml::from_str::<Profile>("max_pods = 4\n").unwrap_err();
        assert!(
            err.to_string().contains("max_pods") || err.to_string().contains("unknown"),
            "deny_unknown_fields should reject a typo'd cap: {err}"
        );
    }

    #[test]
    fn cli_parses_with_defaults_and_env_style_repos() {
        let h = crate::testing::cfg_from_args([
            "ctl",
            "--repo",
            "a/b,c/d",
            "--max-concurrent-pods",
            "7",
        ]);
        assert_eq!(h.repos, vec!["a/b", "c/d"]);
        assert_eq!(h.profile.max_concurrent_pods, 7);
        // An explicit --db flag wins over the DATABASE_URL env / localhost default.
        let h2 = crate::testing::cfg_from_args(["ctl", "--db", "postgres://db.example/ledger"]);
        assert_eq!(h2.db_url(), "postgres://db.example/ledger");
    }

    #[test]
    fn pr_repo_map_resolves_forks_and_ignores_unmapped() {
        let c = crate::testing::cfg_from_args([
            "ctl",
            "--pr-repo-map",
            "neuralmagic/relay-testbed=wren/relay-testbed,llm-d/llm-d-router=wren/llm-d-router",
        ]);
        assert_eq!(
            c.pr_repo_for("neuralmagic/relay-testbed").as_deref(),
            Some("wren/relay-testbed")
        );
        assert_eq!(
            c.pr_repo_for("llm-d/llm-d-router").as_deref(),
            Some("wren/llm-d-router")
        );
        // An unmapped repo has no fork (the loop opens no PR unless its pack names one).
        assert_eq!(c.pr_repo_for("neuralmagic/other").as_deref(), None);
        // No map at all is the default (env-less deploy): every lookup is None.
        let empty = crate::testing::cfg_from_args(["ctl"]);
        assert_eq!(empty.pr_repo_for("neuralmagic/relay-testbed"), None);
    }

    /// Both accepted entry shapes normalize to the same compact JSON string, because that exact
    /// string is what lands in the loop pod's `BROKER_CODEGEN_TOOLS_OVERLAY` AND in the scope
    /// agent's goal framing — a difference between the two would let the pack and the broker
    /// describe different measurements.
    #[test]
    fn broker_contracts_accept_a_string_or_an_inline_object_and_normalize_both() {
        let quoted = parse_broker_contracts(r#"{"deepgemm":"{\"gpus\":1,\"build\":{}}"}"#)
            .expect("string-shaped entry parses");
        let inline = parse_broker_contracts(r#"{"deepgemm":{"gpus":1,"build":{}}}"#)
            .expect("object-shaped entry parses");
        assert_eq!(quoted, inline, "both shapes normalize identically");
        assert_eq!(quoted.names(), vec!["deepgemm".to_string()]);
        assert_eq!(quoted.get("deepgemm"), Some(r#"{"build":{},"gpus":1}"#));
        assert_eq!(quoted.get("nope"), None);
    }

    /// Blank ⇒ no contracts, which is the default deploy and must not be an error.
    #[test]
    fn broker_contracts_default_to_empty() {
        for raw in ["", "   "] {
            let c = parse_broker_contracts(raw).expect("blank parses");
            assert!(c.is_empty(), "{raw:?}");
            assert!(c.names().is_empty());
        }
    }

    /// A contract that is not JSON — or not a JSON *object* — fails the parse, which is a clap
    /// value_parser, so it fails the boot. The alternative is a controller that starts fine and
    /// 422s or mis-measures the first scenario that names the contract.
    #[test]
    fn broker_contracts_reject_values_that_are_not_json_objects() {
        for (raw, want) in [
            ("not json at all", "not valid JSON"),
            (r#"["deepgemm"]"#, "must be a JSON object of contract name"),
            (r#"{"deepgemm":"{ oops"}"#, "is not valid JSON"),
            (r#"{"deepgemm":"[1,2]"}"#, "must be a JSON object"),
            (r#"{"deepgemm":42}"#, "must be a JSON object"),
            (r#"{"deepgemm":"\"a string\""}"#, "must be a JSON object"),
            (r#"{"  ":{"gpus":1}}"#, "blank contract name"),
        ] {
            let err = parse_broker_contracts(raw).expect_err(raw);
            assert!(err.contains(want), "{raw}: got {err}");
        }
    }

    /// The clap surface: the env-shaped value parses into the map, and a malformed one fails
    /// argument parsing rather than being silently dropped.
    #[test]
    fn broker_contracts_parse_through_clap() {
        let c = crate::testing::cfg_from_args([
            "ctl",
            "--broker-contracts",
            r#"{"deepgemm":"{\"gpus\":1}","vllm":{"gpus":2}}"#,
        ]);
        assert_eq!(
            c.broker_contracts.names(),
            vec!["deepgemm".to_string(), "vllm".to_string()],
            "names come back sorted"
        );
        assert_eq!(c.broker_contracts.get("vllm"), Some(r#"{"gpus":2}"#));

        assert!(
            crate::testing::try_cfg_from_args(["ctl", "--broker-contracts", "{"]).is_err(),
            "a malformed contracts map must fail the boot"
        );
        assert!(
            crate::testing::cfg_from_args(["ctl"])
                .broker_contracts
                .is_empty(),
            "no flag, no contracts"
        );
    }

    #[test]
    fn spoke_service_accounts_parse_into_a_map_and_malformed_entries_fail_at_startup() {
        let ok = crate::testing::cfg_from_args([
            "ctl",
            "--turn-service-account",
            "crucible-turn",
            "--spoke-service-accounts",
            "wharf=wharf-turn,slate=slate-turn",
        ])
        .turn_accounts()
        .expect("well-formed entries parse");
        assert_eq!(ok.hub.as_deref(), Some("crucible-turn"));
        assert_eq!(
            ok.spokes.get("wharf").map(String::as_str),
            Some("wharf-turn")
        );
        assert_eq!(
            ok.spokes.get("slate").map(String::as_str),
            Some("slate-turn")
        );

        // Unset is the hub-only default: no spokes, and no pinned hub service account.
        let bare = crate::testing::cfg_from_args(["ctl"])
            .turn_accounts()
            .expect("no entries parse");
        assert_eq!(bare, TurnAccounts::default());

        for bad in ["wharf", "wharf=", "=wharf-turn"] {
            let err = crate::testing::cfg_from_args(["ctl", "--spoke-service-accounts", bad])
                .turn_accounts()
                .unwrap_err()
                .to_string();
            assert!(err.contains(bad), "names the offending entry: {err}");
        }

        // The hub's account has its own knob; naming `hub` here is a config mistake, not a spoke.
        let err =
            crate::testing::cfg_from_args(["ctl", "--spoke-service-accounts", "hub=crucible-turn"])
                .turn_accounts()
                .unwrap_err()
                .to_string();
        assert!(err.contains("CONTROLLER_TURN_SERVICE_ACCOUNT"), "{err}");
    }

    #[test]
    fn an_embedded_database_is_asked_for_by_name_and_needs_the_feature() {
        let c = crate::testing::cfg_from_args(["ctl", "--db", "postgres://localhost/crucible"]);
        assert!(!c.wants_embedded_db());
        c.validate_database()
            .expect("a real URL validates on every build");
        let c = crate::testing::cfg_from_args(["ctl", "--db", "embedded"]);
        assert!(c.wants_embedded_db());
        assert_eq!(c.validate_database().is_ok(), cfg!(feature = "embedded-db"));
    }

    #[test]
    fn autoresearch_is_off_by_default_and_on_only_where_it_is_built() {
        let c = crate::testing::cfg_from_args(["ctl"]);
        assert!(!c.autoresearch_enabled());
        c.validate_autoresearch()
            .expect("off validates on every build");
        let c = crate::testing::cfg_from_args(["ctl", "--autoresearch"]);
        assert_eq!(c.autoresearch_enabled(), cfg!(feature = "autoresearch"));
        assert_eq!(
            c.validate_autoresearch().is_ok(),
            cfg!(feature = "autoresearch"),
            "switching the lane on needs a build that carries it"
        );
    }

    #[test]
    fn grounded_executor_defaults_to_disabled_and_pod_validates_prereqs() {
        // Default is `disabled` (an env-less deploy must boot); explicit `pod` without a
        // profile/sandbox image fails loudly.
        let c = crate::testing::cfg_from_args(["ctl"]);
        assert_eq!(c.grounded_executor, GroundedExecutor::Disabled);
        assert!(c.validate_grounded().is_ok(), "disabled needs no prereqs");
        let c = crate::testing::cfg_from_args(["ctl", "--grounded-executor", "pod"]);
        let err = c.validate_grounded().unwrap_err().to_string();
        assert!(
            err.contains("deploy profile"),
            "names the missing prereq: {err}"
        );

        // `pod` with both prereqs validates.
        let c = crate::testing::cfg_from_args([
            "ctl",
            "--deploy-profile",
            "/etc/profile.toml",
            "--grounded-sandbox-image",
            "ghcr.io/example/sandbox:latest",
        ]);
        c.validate_grounded().expect("pod with prereqs validates");
        assert_eq!(c.profile.grounded_rank_pod_cap, 4);
        assert_eq!(c.profile.grounded_rank_daily_turns, 50);

        // `local` and `disabled` need no prereqs.
        for mode in ["local", "disabled"] {
            let c = crate::testing::cfg_from_args(["ctl", "--grounded-executor", mode]);
            c.validate_grounded()
                .unwrap_or_else(|e| panic!("{mode} validates: {e}"));
        }
    }

    #[test]
    fn scope_executor_defaults_to_local_and_pod_validates_prereqs() {
        // Default is `local` (the compatible mode, no prereqs); explicit `pod` without a
        // profile/sandbox image fails loudly at startup, not at dispatch time.
        let c = crate::testing::cfg_from_args(["ctl"]);
        assert_eq!(c.scope_executor, ScopeExecutor::Local);
        assert!(c.validate_scope().is_ok(), "local needs no prereqs");

        let c = crate::testing::cfg_from_args(["ctl", "--scope-executor", "pod"]);
        let err = c.validate_scope().unwrap_err().to_string();
        assert!(
            err.contains("deploy profile"),
            "names the missing prereq: {err}"
        );

        // A profile but no image (own or grounded fallback) still fails.
        let c = crate::testing::cfg_from_args([
            "ctl",
            "--scope-executor",
            "pod",
            "--deploy-profile",
            "/etc/profile.toml",
        ]);
        let err = c.validate_scope().unwrap_err().to_string();
        assert!(
            err.contains("sandbox image"),
            "names the missing image: {err}"
        );

        // `pod` validates with its own image, or with the grounded image as fallback.
        for image_flag in ["--scope-sandbox-image", "--grounded-sandbox-image"] {
            let c = crate::testing::cfg_from_args([
                "ctl",
                "--scope-executor",
                "pod",
                "--deploy-profile",
                "/etc/profile.toml",
                image_flag,
                "ghcr.io/example/sandbox:latest",
            ]);
            c.validate_scope()
                .unwrap_or_else(|e| panic!("pod with {image_flag} validates: {e}"));
        }

        // `disabled` needs no prereqs either.
        let c = crate::testing::cfg_from_args(["ctl", "--scope-executor", "disabled"]);
        c.validate_scope().expect("disabled validates");
    }

    #[test]
    fn prescope_grounded_defaults_on_and_the_env_var_turns_it_off() {
        // Serialize on the crate-wide env lock: two tests setting the same env var would race.
        let _g = crate::ENV_LOCK.blocking_lock();

        assert!(
            crate::testing::cfg_from_args(["ctl"]).prescope_grounded,
            "on by default — grounded confirmation gates every scope spend unless opted out"
        );

        unsafe {
            std::env::set_var("CONTROLLER_PRESCOPE_GROUNDED", "false");
        }
        assert!(!crate::testing::cfg_from_args(["ctl"]).prescope_grounded);
        unsafe {
            std::env::set_var("CONTROLLER_PRESCOPE_GROUNDED", "true");
        }
        assert!(crate::testing::cfg_from_args(["ctl"]).prescope_grounded);
        unsafe {
            std::env::remove_var("CONTROLLER_PRESCOPE_GROUNDED");
        }
    }

    #[test]
    fn scope_gaming_rounds_defaults_to_one_and_reads_the_env_var() {
        let _g = crate::ENV_LOCK.blocking_lock();

        assert_eq!(
            crate::testing::cfg_from_args(["ctl"])
                .profile
                .scope_gaming_rounds,
            1,
            "one concern→refine→re-review cycle by default (the historical behavior)"
        );

        unsafe {
            std::env::set_var("CONTROLLER_SCOPE_GAMING_ROUNDS", "3");
        }
        assert_eq!(
            crate::testing::cfg_from_args(["ctl"])
                .profile
                .scope_gaming_rounds,
            3
        );
        unsafe {
            std::env::remove_var("CONTROLLER_SCOPE_GAMING_ROUNDS");
        }
    }

    #[test]
    fn run_iteration_and_budget_knobs_default_and_read_env_vars() {
        let _g = crate::ENV_LOCK.blocking_lock();

        let p = Profile::default();
        assert_eq!(
            p.run_iterations, 6,
            "a dispatched run gets a multi-turn budget"
        );
        assert_eq!(p.run_max_cost, 25.0);

        let c = crate::testing::cfg_from_args(["ctl"]).profile;
        assert_eq!(c.run_iterations, 6);
        assert_eq!(c.run_max_cost, 25.0);

        unsafe {
            std::env::set_var("CONTROLLER_RUN_ITERATIONS", "9");
        }
        unsafe {
            std::env::set_var("CONTROLLER_RUN_MAX_COST_USD", "40");
        }
        let c = crate::testing::cfg_from_args(["ctl"]).profile;
        assert_eq!(c.run_iterations, 9);
        assert_eq!(c.run_max_cost, 40.0);
        unsafe {
            std::env::remove_var("CONTROLLER_RUN_ITERATIONS");
        }
        unsafe {
            std::env::remove_var("CONTROLLER_RUN_MAX_COST_USD");
        }
    }

    #[test]
    fn scope_skip_gaming_review_defaults_off_and_reads_the_env_var() {
        let _g = crate::ENV_LOCK.blocking_lock();

        assert!(
            !crate::testing::cfg_from_args(["ctl"])
                .profile
                .scope_skip_gaming_review,
            "the gaming review stays the standing policy by default"
        );

        unsafe {
            std::env::set_var("CONTROLLER_SCOPE_SKIP_GAMING_REVIEW", "true");
        }
        assert!(
            crate::testing::cfg_from_args(["ctl"])
                .profile
                .scope_skip_gaming_review
        );
        unsafe {
            std::env::remove_var("CONTROLLER_SCOPE_SKIP_GAMING_REVIEW");
        }
    }
}
