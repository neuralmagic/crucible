//! The shared vocabulary of a run: its CLI options ([`Args`]), the paths everything anchors
//! off ([`Paths`]), and the inputs resolved once before the loop starts ([`Prepared`]).

use crate::duration::parse_duration;
use crate::identity;
use crate::manifest;
use crate::openshell;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Which front-end to drive the loop with.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Ui {
    /// Console output on a terminal, headless when piped (default).
    Auto,
    /// Force plain line output.
    Headless,
    /// Machine-readable NDJSON of the loop's own events on stdout (for CI / dashboards).
    Jsonl,
    /// Headless run that writes the session log to `state/session.jsonl` for
    /// external tailers. No terminal output.
    Stream,
}

#[derive(clap::Args, Clone)]
pub(crate) struct Args {
    /// The domain manifest: the engine reads it, builds a World + Judge, and works in its
    /// workspace. Required (every run is a manifest run).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Runtime state dir (session log + control file). Default: `<manifest-dir>/state` for a
    /// manifest run, else `state/`. Override for an in-pod writable mount.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    /// The `command` backend's proposal command (set from `[agent].agent_cmd`; no CLI flag).
    #[arg(skip)]
    pub agent_cmd: Option<String>,
    /// Max agent iterations.
    #[arg(long, default_value_t = 3)]
    pub iterations: u32,
    /// Wide-round breadth: fan out N independent candidates in parallel before the deep loop.
    /// Each candidate gets one PROPOSE turn biased to a distinct `[search].approaches` entry,
    /// measured serially, ranked by the gate. The winner seeds the deep loop. 0 = no wide round
    /// (pure deep, the default). Overrides `[search].wide`.
    #[arg(long, default_value_t = 0)]
    pub wide: u32,
    /// How many wide-round winners seed a deep loop (top-K by score). Default 1. Only
    /// meaningful when `--wide > 0`. Overrides `[search].policy_k`.
    #[arg(long, default_value_t = 1)]
    pub wide_keep: u32,
    /// Run each iteration as a canonical work-graph plan (propose → apply → measure → decide)
    /// through the shared plan executor instead of the hand-sequenced stages. Same events,
    /// same decisions (parity-gated), plus additive plan lines on the session log.
    /// Default off while the rollout soaks.
    #[arg(long)]
    pub graph_loop: bool,
    /// Don't stop early when an iteration solves the gate, run the full `--iterations` budget.
    /// For ablations: observe what each effort tier does with extra shots *after* solving
    /// (does it keep gold-plating, find more, or regress?). Default: stop on the first solve.
    #[arg(long)]
    pub no_early_stop: bool,
    /// Front-end: auto (default), headless, jsonl (machine NDJSON), or stream
    /// (headless + session log for external tailers).
    #[arg(long, value_enum, default_value_t = Ui::Auto)]
    pub ui: Ui,
    /// Per-run goal text, overriding the manifest's `[agent].goal`/`goal_file` (e.g. an issue
    /// body piped in by the forge trigger).
    #[arg(long)]
    pub goal: Option<String>,
    /// File holding the per-run goal, overriding the manifest's goal.
    #[arg(long)]
    pub goal_file: Option<PathBuf>,
    /// Agent process env (from the manifest's `[agent].env`: creds, Vertex, ...). No CLI flag.
    #[arg(skip)]
    pub env: Vec<(String, String)>,
    /// Credential files relayed into the sandbox before each turn (from `[[agent.relay]]`).
    #[arg(skip)]
    pub relay: Vec<manifest::RelayFile>,
    /// What the frozen pack's capability disclosure covers. The provisioning path refuses any
    /// grant outside it (RFC-0001:C-CAPABILITY-DISCLOSURE). `None` on a path with no frozen
    /// manifest to disclose from (a scope or rank turn), which is not the same as disclosing
    /// nothing. No CLI flag.
    #[arg(skip)]
    pub disclosure: Option<crate::exposure::Covered>,
    /// The frozen pack's resolved output bounds, for the two kinds the engine writes itself (the
    /// draft PRs publish-on-keep opens, the `workflow_dispatch` a github-actions build fires).
    /// Same value the broker is projected (RFC-0001:C-OUTPUTS). `None` on a path with no frozen
    /// manifest to bound from. No CLI flag.
    #[arg(skip)]
    pub output_bounds: Option<crate::outputs::RunBounds>,
    /// OpenShell egress policy (endpoints/binaries) for the `openshell` backend (from
    /// `[agent.openshell]`). No CLI flag.
    #[arg(skip)]
    pub openshell: manifest::OpenshellCfg,
    /// The loop-pod provisioning broker for the `openshell` backend (from `[agent.broker]`). The
    /// agent asks, the loop pod holds the keys. No CLI flag.
    #[arg(skip)]
    pub broker: manifest::BrokerCfg,
    /// Bearer token guarding the broker endpoint, set when the broker is spawned and seeded into
    /// the sandbox's `.mcp.json` headers. Runtime state rather than config, so there is no CLI flag.
    #[arg(skip)]
    pub broker_token: Option<String>,
    /// The model the agent runs. Overrides the manifest's `[agent].model`; when neither is set the
    /// resolved harness's own default applies (see `Args::model`).
    #[arg(long)]
    pub model: Option<String>,
    /// The agent harness that runs each turn: `claude` (default), `hermes`, or `codex`. Overrides
    /// the manifest's `[agent].harness`; when neither is set the engine defaults to claude (see
    /// `apply_agent_cfg`).
    #[arg(long, value_enum)]
    pub harness: Option<crate::manifest::Harness>,
    /// Hermes-harness tuning (from `[agent.hermes]`). No CLI flag.
    #[arg(skip)]
    pub hermes: manifest::HermesCfg,
    /// Codex-harness tuning (from `[agent.codex]`). No CLI flag.
    #[arg(skip)]
    pub codex: manifest::CodexCfg,
    /// Tools the agent must not call (from `[agent].disallowed_tools`). No CLI flag.
    #[arg(skip)]
    pub disallowed_tools: Vec<String>,
    /// Reasoning-effort tier for the agent, passed to Claude Code as `--effort <level>`. Overrides
    /// `[agent].reasoning_effort`; when neither is set the engine defaults to `medium` (see
    /// `apply_agent_cfg`).
    #[arg(long = "effort", value_enum)]
    pub reasoning_effort: Option<crate::manifest::ReasoningEffort>,
    /// Backend for the agent turn: `local` (default) runs it here; `openshell` runs
    /// it in an OpenShell sandbox (what an in-pod loop uses). Needs `--sandbox-image`.
    #[arg(long, value_enum, default_value_t = manifest::AgentBackend::Local)]
    pub agent_backend: manifest::AgentBackend,
    /// Sandbox image for `--agent-backend openshell` (the domain's agent toolbox baked in).
    #[arg(long)]
    pub sandbox_image: Option<String>,
    /// OpenShell compute driver: `podman` (default, nests the sandbox inside the loop pod) or
    /// `kubernetes` (schedules it as a sibling pod in-cluster). Fixed per deployment, so the
    /// rendered wrapper script passes it; `podman` is the right default for a laptop or EC2.
    #[arg(long, value_enum, default_value_t = openshell::gateway::ComputeDriver::Podman)]
    pub compute_driver: openshell::gateway::ComputeDriver,
    /// Kubernetes namespace recorded in the session log (the engine itself does no
    /// kubectl; domain deployment access lives in the manifest's commands). Empty = the kube
    /// context's current namespace.
    #[arg(long, default_value = "")]
    pub namespace: String,
    /// Stop after cumulative agent cost reaches this many USD (0 = unlimited).
    #[arg(long, default_value_t = 0.0)]
    pub max_cost: f64,
    /// Stop after this much wall-clock (e.g. `30m`, `1h`, `90s`; empty = unlimited).
    #[arg(long, default_value = "")]
    pub max_time: String,
    /// Max time to park waiting on a pending approval before giving up (e.g. `30m`, `2h`; empty =
    /// wait indefinitely). On timeout a blocked run escalate-halts.
    #[arg(long, default_value = "")]
    pub max_park: String,
    /// Start the in-process TCP control bridge on this port (requires `--ui stream`;
    /// use `kubectl port-forward` to reach it in a pod).
    #[arg(long)]
    pub control_port: Option<u16>,
    /// Resume a parked run: replay state/session.jsonl to restore progress and
    /// continue from the next iteration (appends to the same log, headless stream mode).
    #[arg(long)]
    pub resume: bool,
    /// Publish-on-keep: S3 destination for the durable run record, e.g.
    /// `s3://my-artifacts-bucket/autoresearch` (empty = don't publish to S3).
    /// Creds are IRSA web-identity (AWS_ROLE_ARN + the projected token).
    #[arg(long, default_value = "")]
    pub results_bucket: String,
    /// Publish-on-keep: `owner/repo` to push the kept-commits branch to when a run
    /// keeps at least one iteration (empty = don't push). PAT from AUTORESEARCH_PR_TOKEN
    /// or GITHUB_TOKEN. Opening the PR is v2.
    #[arg(long, default_value = "")]
    pub pr_repo: String,
    /// Publish-on-keep (composite only): per-component `(name, owner/repo)` fork map, populated from the
    /// composite manifest's `[[component]].pr_repo`, not a CLI flag. Each touched component opens one
    /// cross-linked draft PR against its fork.
    #[arg(skip)]
    pub component_pr_repos: Vec<(String, String)>,
    /// Declared pipeline artifacts (from `[[workspace.artifact]]`), for the publish layer:
    /// each `embed` match lands in the PR body and the S3 run record. No CLI flag.
    #[arg(skip)]
    pub artifacts: Vec<manifest::Artifact>,
    /// Wide-round search config (from `[search]`). No CLI flag, set by `run_from_manifest`.
    #[arg(skip)]
    pub search: Option<manifest::SearchCfg>,
    /// Manifest-only authored workflow.
    #[arg(skip)]
    pub workflow: Option<crate::plan::workflow::WorkflowCfg>,
    /// Manifest injects restored in each task workspace.
    #[arg(skip)]
    pub workflow_frozen_injects: Vec<(PathBuf, PathBuf)>,
    /// Toolbox exclusions for per-task harness overrides.
    #[arg(skip)]
    pub workflow_toolbox_exclude: Vec<String>,
    /// Opt-in: when publish-on-keep opens draft PR(s), spawn a detached `crucible watch-pr` pointed
    /// at them, reseeding this run's `STEER.md` from review comments so the NEXT run picks up feedback
    /// without a human running `watch-pr` by hand. Best-effort: spawn failure only logs (the PR still
    /// opened; a human can always run `watch-pr` themselves).
    #[arg(long)]
    pub watch_feedback: bool,
}

impl Args {
    /// `Args` as a flagless `crucible` invocation parses them: every default, nothing set.
    pub(crate) fn defaults() -> Result<Self, clap::Error> {
        #[derive(clap::Parser)]
        struct Flagless {
            #[command(flatten)]
            run: Args,
        }
        <Flagless as clap::Parser>::try_parse_from(["crucible"]).map(|f| f.run)
    }

    /// Parse `--max-time` (e.g. `30m`) into a duration; None when unset/invalid.
    pub(crate) fn max_time(&self) -> Option<Duration> {
        parse_duration(&self.max_time)
    }

    /// Parse `--max-park` into a duration; None = wait on an approval indefinitely.
    pub(crate) fn max_park(&self) -> Option<Duration> {
        parse_duration(&self.max_park)
    }

    /// The resolved agent harness: CLI `--harness` > manifest `[agent].harness` (folded on by
    /// `apply_agent_cfg`) > claude. Paths that never see a manifest (rank-grounded, scope) get
    /// the claude default.
    pub(crate) fn harness(&self) -> crate::manifest::Harness {
        self.harness.unwrap_or_default()
    }

    /// The resolved model: CLI `--model` > manifest `[agent].model` (folded on by
    /// `apply_agent_cfg`) > the resolved harness's default.
    pub(crate) fn model(&self) -> &str {
        self.model
            .as_deref()
            .unwrap_or_else(|| self.harness().default_model())
    }
}

/// Runtime paths for a manifest run. Everything anchors off the manifest dir + an explicit
/// state dir, never the binary's install location (contract §2): a target repo is
/// self-describing, drop a `crucible.toml` at its root and run `crucible` inside it.
#[derive(Clone)]
pub(crate) struct Paths {
    pub workspace: PathBuf,
    /// Toolbox source dir (`[agent].toolbox_dir`, manifest-relative); its subdirs are copied
    /// into `<workspace>/.claude/skills` each run. `None` when the manifest sets no toolbox.
    pub skills: Option<PathBuf>,
    pub steer: PathBuf,
    /// Cross-process state dir (gitignored): the session log + control file live here.
    pub state: PathBuf,
    /// Append-only NDJSON event log the headless loop emits for external tailers.
    pub session_log: PathBuf,
    /// Cross-process stop signal written by the `stop` tool.
    pub control: PathBuf,
    /// Escalation marker the agent's `escalate` tool writes in its workspace; the loop detects it
    /// after a turn, restores the world, and halts for human review.
    pub escalation: PathBuf,
    /// Pending-provisioning marker the agent writes when it has an open approval to wait on; the loop
    /// detects it after a turn and parks or continues per its `mode`.
    pub provisioning: PathBuf,
    /// Append-only NDJSON record of every external input, authoritative over the session
    /// log for what an operator asked for; a resume replays it.
    pub admissions: PathBuf,
}

impl Paths {
    pub(crate) fn for_manifest(
        workspace: PathBuf,
        state: PathBuf,
        manifest_dir: &Path,
        skills: Option<PathBuf>,
    ) -> Self {
        let escalation = workspace.join("ESCALATION.json");
        let provisioning = workspace.join("PROVISIONING_PENDING.json");
        Self {
            workspace,
            skills,
            steer: manifest_dir.join("STEER.md"),
            session_log: state.join("session.jsonl"),
            control: state.join("control.json"),
            admissions: state.join("admissions.jsonl"),
            escalation,
            provisioning,
            state,
        }
    }

    /// Paths for an isolated task worktree: everything (state, steer, markers) lives inside
    /// the clone so a task cannot touch the parent run's state.
    pub(crate) fn for_worktree(worktree: PathBuf, skills: Option<PathBuf>) -> Self {
        Self {
            skills,
            steer: worktree.join("STEER.md"),
            state: worktree.join("state"),
            session_log: worktree.join("state/session.jsonl"),
            control: worktree.join("state/control.json"),
            // An isolated task worktree takes no external input; the path exists only so
            // `Paths` stays one shape.
            admissions: worktree.join("state/admissions.jsonl"),
            escalation: worktree.join("ESCALATION.json"),
            provisioning: worktree.join("PROVISIONING_PENDING.json"),
            workspace: worktree,
        }
    }
}

/// Inputs resolved once before the loop (and before any UI takes the screen).
#[derive(Clone)]
pub(crate) struct Prepared {
    pub goal: String,
    pub template: String,
    /// `<YYYYMMDDTHHMMSSZ>-<goal-slug>`: the publish join key (S3 prefix, branch, PR↔S3).
    pub run_id: String,
    /// Cross-run memory: the prior run's tried-ideas rows for this goal, seeded from S3 at startup
    /// (empty when there's no prior run / no results bucket). Rendered into `RESULTS.md` so the
    /// agent's "read what's been tried" step inherits history across runs, not just iterations.
    pub prior: String,
    /// The world's comparability key, computed once at setup from the frozen manifest + workspace(s).
    /// Stamped into the session log and publish summary at run start.
    pub identity: identity::RunIdentity,
    /// `[judge].skip_baseline`: baseline (and re-scope re-baseline) snapshots only, no measure.
    pub skip_baseline: bool,
    /// `[preflight]`: the rung ladder run against the unmodified tree before iteration 1. `None`
    /// = no preflight.
    pub preflight: Option<manifest::PreflightCfg>,
    /// The build modes a `{mode}` preflight rung fans out over, from
    /// `[measure.build].mutable_kwargs.mode`. Empty when the domain declares none (validation
    /// already rejected a `{mode}` rung in that case).
    pub preflight_modes: Vec<String>,
    /// `[agent].seed_diff` content, handed to iteration 1's prompt as labeled seed material (its
    /// content hash rides `identity.seed_hash`). `None` = an unseeded run.
    pub seed_diff: Option<String>,
}

/// The agent a launch names in place of the manifest's `[agent]` defaults. A task that pins its
/// own harness or model keeps it; the override replaces only what the manifest would have
/// supplied.
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentOverride {
    pub harness: Option<manifest::Harness>,
    pub model: Option<String>,
}
