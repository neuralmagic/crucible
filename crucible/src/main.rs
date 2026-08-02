//! crucible, a propose→apply→measure→accept/reject→remember loop. An LLM agent proposes a
//! change to a reversibly-mutable world; a frozen judge measures it; the engine keeps or
//! discards by a generic rule and remembers the winners. A domain is a `crucible.toml`
//! manifest (see `docs/crucible-contract.md`), not engine code; `examples/counter` is the
//! litmus manifest.
//!
//! Thin driver, fat agent: each iteration hands the agent (Claude) a goal +
//! history + a toolbox, then independently runs the manifest's `measure` command and gates
//! keep/discard. The engine names nothing domain-specific; everything plugs in behind the
//! [`crucible::World`] + [`crucible::Judge`] traits, satisfied by the built-in command
//! batteries the manifest configures.
//!
//! Layout: this file is the entrypoint + the shared vocabulary (CLI, [`Args`], [`Paths`],
//! [`Prepared`]); [`run`] holds setup + command dispatch; [`loop_driver`] holds the single
//! orchestration loop. That loop talks only to a [`reporter::Reporter`], so one loop drives
//! multiple front-ends: [`console::ConsoleReporter`] for headless runs and the NDJSON
//! [`stream::SessionReporter`] for stdout/session-log runs. The choice is just `--ui`
//! (default: auto by TTY).
//!
//! Operator ergonomics:
//!
//! - Ctrl+C never just dies: it stops cleanly after the current step and prints a summary. Headless offers a steer/quit prompt.
//! - Steering: drop guidance in STEER.md (or via the prompt) and it is injected into the next iteration's prompt, the lever for when the agent goes off the rails.

mod agent;
mod broker;
mod build;
mod check;
mod command_judge;
mod command_world;
mod console;
pub(crate) mod control;
mod crucible;
mod deploy;
mod engine;
mod escalation;
mod event;
mod harness;
mod hermes_trace;
mod identity;
mod ingest_client;
mod init;
mod loop_driver;
mod loop_graph;
mod manifest;
mod openshell;
mod plan;
mod pr_watch;
mod provisioning;
mod ps;
mod publish;
mod rank_grounded;
mod refine;
mod relay;
mod reporter;
mod result_mode;
mod run;
mod scope;
mod selftest;
mod session;
mod stream;
mod turn_trace;
pub(crate) use crucible_harness::stream_json;

use anyhow::Result;
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub(crate) static STOP: AtomicBool = AtomicBool::new(false);

/// Send SIGTERM to one PID (no-op if zero/negative), via libc rather than the `kill` binary.
pub(crate) fn kill_pid(pid: i32) {
    if pid > 0 {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::SIGTERM);
    }
}

/// Registry of live agent-child PIDs so Ctrl+C can kill ALL concurrent children (wide-round
/// parallel agents and the serial deep-loop agent alike).
pub(crate) mod pid_registry {
    use std::sync::Mutex;

    static PIDS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

    pub fn register(pid: i32) {
        if let Ok(mut v) = PIDS.lock() {
            v.push(pid);
        }
    }

    pub fn deregister(pid: i32) {
        if let Ok(mut v) = PIDS.lock() {
            v.retain(|&p| p != pid);
        }
    }

    pub fn kill_all() {
        if let Ok(v) = PIDS.lock() {
            for &pid in v.iter() {
                super::kill_pid(pid);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn register_and_deregister() {
            register(99999);
            register(99998);
            {
                let v = PIDS.lock().unwrap();
                assert!(v.contains(&99999));
                assert!(v.contains(&99998));
            }
            deregister(99999);
            {
                let v = PIDS.lock().unwrap();
                assert!(!v.contains(&99999));
                assert!(v.contains(&99998));
            }
            deregister(99998);
        }

        #[test]
        fn deregister_nonexistent_is_noop() {
            deregister(77777);
        }

        #[test]
        fn kill_all_does_not_panic_on_stale_pids() {
            register(1);
            kill_all();
            {
                let v = PIDS.lock().unwrap();
                assert!(v.contains(&1));
            }
            deregister(1);
        }
    }
}

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

/// Top-level CLI: the default (no subcommand) runs the loop.
#[derive(Parser)]
#[command(
    about = "Agentic autoresearch loop: an LLM proposes, a frozen judge decides. Domain = crucible.toml"
)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
    #[command(flatten)]
    run: Args,
}

/// Subcommands that aren't the default loop run.
#[derive(clap::Subcommand)]
pub(crate) enum Cmd {
    /// Scaffold a minimal `crucible.toml` + measure stub in the current directory (or `--dir`):
    /// the bring-your-own-repo on-ramp, dropping crucible onto a repo like a justfile. Refuses
    /// to overwrite existing files.
    Init {
        /// Directory to scaffold into (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Validate a manifest without spending a loop iteration: parse it, resolve every file it
    /// references, run `measure_cmd` once to prove the measure contract, and warn if the gate is
    /// reachable by the agent's own edits. Exits nonzero with a findings list on failure.
    Check {
        /// The domain manifest to validate.
        #[arg(long)]
        manifest: PathBuf,
        /// Parse the manifest (deny_unknown_fields + validate) and stop: no referenced-file
        /// resolution, workspace setup, measure probe, or gate self-test. Runs anywhere (CI).
        #[arg(long)]
        parse_only: bool,
        /// Also validate a deploy profile's cluster wiring: the named [measure].cluster resolves
        /// against the fleet file, its secret name is non-empty, no bastion (not implemented yet),
        /// and, live, the sandbox SA cannot read the spoke kubeconfig Secret in the loop namespace.
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Explicit fleet-file path, overriding the `clusters.toml` sibling of `--profile`.
        #[arg(long)]
        clusters: Option<PathBuf>,
    },
    /// The scoping pipeline: ingest the goal, optionally `--propose` a fresh
    /// pack via one agent turn, validate the manifest (`crucible check`), and freeze a `SCOPE.md`
    /// recording the goal source, the check outcome, and the pack's `RunIdentity` digest. No
    /// isolation preflight (S3), no draft-PR approval (S4), the freeze report names those as
    /// pending.
    Scope(scope::ScopeArgs),
    /// List every crucible loop pod in the cluster (kube-native): NAME, NAMESPACE, PHASE, AGE,
    /// RESTARTS, and a best-effort ITER (ships as `-` for now, see `ps.rs`'s module doc). Selects
    /// on the `app.kubernetes.io/managed-by=crucible` label every rendered loop pod carries.
    Ps {
        /// Restrict to one namespace (default: every namespace the client can list).
        #[arg(long)]
        namespace: Option<String>,
        /// Emit the same rows as a JSON array instead of the aligned table.
        #[arg(long)]
        json: bool,
    },
    /// Render this domain's run deployment: the loop pod + RBAC, projected from the manifest (the
    /// run) + a per-cluster deploy profile (the environment), with image tags resolved to digests.
    /// Stops hand-writing the loop-pod YAML. Works for a composite manifest or a plain single-domain
    /// one (the latter needs its own `[deploy]` block naming its build/deploy target).
    Deploy {
        #[command(subcommand)]
        action: DeployAction,
    },
    /// Work-graph plans: compile and inspect a plan without executing it.
    Plan {
        #[command(subcommand)]
        action: PlanAction,
    },
    /// Watch one or more draft PRs' review comments and either steer a live run or reseed the next
    /// one: each NEW human comment is delivered either to a live run's control bridge as a `steer`,
    /// or appended to a reseed file that the next run's first turn reads, exactly one of
    /// `--control-addr`/`--reseed` is required. A kept composite candidate is a SET of linked PRs
    /// (one per component fork); pass `--pr` more than once to watch them all in one process.
    WatchPr {
        /// The PR to watch, e.g. `https://github.com/owner/repo/pull/42` (repeatable, a composite
        /// candidate opens one linked PR per component).
        #[arg(long = "pr", required = true)]
        pr: Vec<String>,
        /// The live run's control-bridge address (host:port, from its `--control-port`). Exactly one of
        /// this or `--reseed` is required.
        #[arg(long)]
        control_addr: Option<String>,
        /// A file (typically the NEXT run's `STEER.md`) to append fresh comments to instead of steering
        /// a live run, "start with reseed": no run needs to be up. Exactly one of this or
        /// `--control-addr` is required.
        #[arg(long)]
        reseed: Option<PathBuf>,
        /// Our own bot login to ignore, so the watcher never steers on the publisher's own comments.
        #[arg(long, default_value = "")]
        bot_user: String,
        /// Allowlist a specific commenter login (repeatable). When ANY are given, ONLY these logins may
        /// steer. Otherwise the default gate applies: only commenters GitHub reports with write access
        /// (author_association OWNER/MEMBER/COLLABORATOR), a steer drives an agent that edits + deploys.
        #[arg(long = "allow-user")]
        allow_user: Vec<String>,
        /// Seconds between polls.
        #[arg(long, default_value_t = pr_watch::DEFAULT_POLL_SECS)]
        poll_secs: u64,
        /// Fetch once and exit instead of polling forever, the scripting shape: collect whatever
        /// review a PR has accumulated (with no live run to baseline against) and reseed the next run.
        #[arg(long)]
        once: bool,
    },
    /// Download one published object at an exact `s3://bucket/key` URI to a local file, the general
    /// GetObject the controller's artifact proxy shells so no S3 client leaks into
    /// `crucible-controller`. Nothing is appended to the URI; the caller passes the exact key.
    Fetch {
        /// The exact `s3://bucket/key` object URI to download (no `session.jsonl` is appended).
        #[arg(long)]
        uri: String,
        /// Destination file path (parent dirs must exist).
        #[arg(long, short)]
        out: PathBuf,
    },
    /// Grounded triage ranking: run ONE code-grounded ranking turn over an existing checkout and
    /// print the verdict JSON `{tier,rationale,confidence,cost_usd,over_budget}`. The controller's
    /// cheap text-only ranker escalates to this when it is unsure; the turn is read-only (a
    /// throwaway worktree contains any write). The caller owns `--workspace`, this command never
    /// clones or mutates it.
    RankGrounded(rank_grounded::RankGroundedArgs),
    /// Dispatch a named `[build.<name>]` from the domain manifest, wait for it, and print the
    /// digest-pinned ref. The cluster backend renders a detached rootless-buildah Job; the
    /// `github-actions` backend dispatches a `workflow_dispatch`, correlates + polls the run, and pins
    /// the pushed tag. `--check` validates the github backend's declared input mapping against the
    /// workflow (introspection) and exits. This is the exact code path the controller dispatches later
    /// (one implementation, two callers).
    Build(build::BuildArgs),
}

/// `crucible plan <show|run>`: compile and inspect a plan, or execute one.
#[derive(clap::Subcommand)]
pub(crate) enum PlanAction {
    /// Print the compiled plan (tasks in dependency-first order) and the truncation verdict
    /// for the given substrate caps. TOML by `.toml` extension, JSON otherwise.
    Show {
        /// The plan file to compile.
        #[arg(long)]
        file: PathBuf,
        /// Substrate capabilities to preview against (repeatable). `any`-needs tasks always run.
        #[arg(long = "cap")]
        caps: Vec<String>,
        /// Emit mermaid flowchart source instead of the table (pipe to a mermaid renderer,
        /// or paste into any markdown surface that renders it).
        #[arg(long, conflicts_with = "render")]
        mermaid: bool,
        /// Render the graph to an image: inline in the terminal (iTerm2/WezTerm/kitty/ghostty
        /// image protocols) or, elsewhere, a PNG next to the plan file. Fully offline.
        #[arg(long)]
        render: bool,
    },
    /// Execute a plan with the shell runner: `command` tasks run as real subprocesses,
    /// `agent` tasks run `--agent-cmd` (the command-backend stand-in). Exits nonzero when
    /// the plan does not reach a valid verdict.
    Run {
        /// The plan file to execute.
        #[arg(long)]
        file: PathBuf,
        /// Substrate capabilities (repeatable). `any`-needs tasks always run.
        #[arg(long = "cap")]
        caps: Vec<String>,
        /// Stand-in command for agent tasks; receives CRUCIBLE_PROMPT / _HARNESS / _MODEL /
        /// _EFFORT in env. Without it, agent tasks are refused.
        #[arg(long, conflicts_with = "manifest")]
        agent_cmd: Option<String>,
        /// Run agent tasks through the real harness path using this manifest's `[agent]`
        /// config (workspace set up exactly as a loop run). Command tasks run in the
        /// workspace.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
}

/// `crucible deploy <render|apply>`: emit the deployment YAML, or render-and-`kubectl apply`.
#[derive(clap::Subcommand)]
pub(crate) enum DeployAction {
    /// Emit the rendered loop-pod + RBAC YAML to stdout (review / gitops / `kubectl apply -f -`).
    Render(DeployArgs),
    /// Render then `kubectl apply -f -` (the thin convenience over `render`).
    Apply(DeployArgs),
    /// Emit one grounded-rank turn pod (WorkPod primitive) to stdout: a single one-shot
    /// pod that clones a repo and runs `crucible rank-grounded` in the openshell sandbox, printing
    /// the verdict marker. The controller shells this, then stamps the work-pod labels + its
    /// ownerReference before creating the pod.
    RenderTurn(RenderTurnArgs),
}

#[derive(clap::Args)]
pub(crate) struct RenderTurnArgs {
    /// The per-cluster deploy profile (namespaces, secrets, resources, loop image, supervisor image).
    #[arg(long)]
    pub profile: PathBuf,
    /// The pod's k8s object name (the caller owns it, its work-pod row + ownerRef key on it).
    #[arg(long)]
    pub name: String,
    /// The issue to rank/scope, `owner/repo#N` (or a non-upstream scenario's synthetic key). Always
    /// required, it names the turn's `work_pods` row/pod even when `--goal-file` supplies the
    /// scope turn's actual goal content.
    #[arg(long)]
    pub issue: String,
    /// A non-upstream scenario's ledgered goal text, read from this local file and rendered into
    /// the in-pod `crucible scope --propose --goal-file …` (base64'd into the pod's wrapper script,
    /// since the file itself can't ride into a remote pod). `scope` turn kind only; when set, the
    /// in-pod invocation uses `--goal-file` instead of `--issue` (mirrors the non-pod executor's
    /// local-file `Ingest` arm in `engine::scope_propose`).
    #[arg(long)]
    pub goal_file: Option<PathBuf>,
    /// The clone URL of the repo under test (cloned fresh into the turn pod).
    #[arg(long)]
    pub repo_url: String,
    /// The agent sandbox image carrying the claude CLI (the openshell backend pulls it).
    #[arg(long)]
    pub sandbox_image: String,
    /// Cap on the turn's cost in USD.
    #[arg(long, default_value_t = 5.0)]
    pub max_cost: f64,
    /// Emit image tags verbatim instead of resolving to `@sha256:…` (air-gapped render).
    #[arg(long)]
    pub no_pin: bool,
    /// What the turn pod does: `rank` (grounded ranking, default) or `scope` (scope-propose).
    #[arg(long, default_value = "rank")]
    pub turn_kind: String,
    /// The issue's confirmed tier, forwarded to the in-pod `crucible scope --propose --tier …`.
    /// `scope` turn kind only; absent = the engine's t0 default.
    #[arg(long, value_enum)]
    pub tier: Option<crate::scope::ProposeTier>,
    /// Max gaming-review concern→refine→re-review cycles, forwarded to the in-pod
    /// `crucible scope --propose --gaming-refine-rounds …`. `scope` turn kind only.
    #[arg(long, default_value_t = 1)]
    pub gaming_refine_rounds: u32,
    /// Skip the adversarial gaming review entirely, forwarded to the in-pod `crucible scope
    /// --propose --skip-gaming-review`. `scope` turn kind only; overrides `gaming_refine_rounds`
    /// when set (an operator escape hatch for demo/bring-up postures).
    #[arg(long)]
    pub skip_gaming_review: bool,
    /// The goal is an authoritative brief, forwarded to the in-pod `crucible scope --propose
    /// --authoritative`. `scope` turn kind only.
    #[arg(long)]
    pub authoritative: bool,
}

#[derive(clap::Args)]
pub(crate) struct DeployArgs {
    /// The domain manifest (composite or single-domain), the run: components, broker, judge, world,
    /// `[deploy]` targets. Required unless `--controller` is set (the controller has no per-run
    /// manifest; it renders from the profile's `[controller]` table alone).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// The per-cluster deploy profile (the environment: namespaces, secrets, resources, loop image).
    #[arg(long)]
    pub profile: PathBuf,
    /// Agent iterations the rendered loop runs per launch. Ignored with `--controller`.
    #[arg(long, default_value_t = 1)]
    pub iterations: u32,
    /// Cumulative agent-cost ceiling in USD the rendered loop runs under (`--max-cost`, 0 =
    /// unlimited). Ignored with `--controller`. The controller passes its `run_max_cost` knob here;
    /// a manual render defaults to unlimited.
    #[arg(long, default_value_t = 0.0)]
    pub max_cost: f64,
    /// Emit image tags verbatim instead of resolving them to `@sha256:…` (for an air-gapped render
    /// where the registry isn't reachable). The pin is the footgun fix, so it's on by default.
    #[arg(long)]
    pub no_pin: bool,
    /// DEPRECATED (use the `crucible-controller` Helm chart).
    /// Render the outer-loop controller's Deployment/PVC/Service/RBAC instead of a domain's run,
    /// projected from the profile's `[controller]` table, no `--manifest` needed.
    #[arg(long)]
    pub controller: bool,
    /// Pack delivery: the manifest is a controller-drafted PACK on the state PVC, not a domain baked
    /// into the loop image. Emit a ConfigMap carrying the pack files and stage it (init-container →
    /// emptyDir) at the in-pod domain path, so `crucible run` finds the manifest. Set by the
    /// controller's run dispatch; a human render of a baked domain leaves it off.
    #[arg(long)]
    pub pack: bool,
    /// The pack ConfigMap's object name (with `--pack`): used for both the emitted CM's name and the
    /// pod volume that mounts it. Defaults to `<domain>-pack`; the controller passes a run-unique name.
    #[arg(long)]
    pub pack_configmap_name: Option<String>,
    /// Publish-on-keep: the `owner/repo` fork the rendered loop opens its kept-commits draft PR against
    /// (emitted as the loop's `--pr-repo`). The controller passes its per-repo default so a dispatched
    /// run publishes; omit for a manual render (the manifest's `[publish] pr_repo` still applies). The
    /// push PAT rides the profile's secret env (`AUTORESEARCH_PR_TOKEN`).
    #[arg(long)]
    pub pr_repo: Option<String>,
    /// Explicit fleet-file path (`[clusters.<name>]` tables), overriding the `clusters.toml`
    /// sibling of `--profile`.
    #[arg(long)]
    pub clusters: Option<PathBuf>,
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
    /// Vertex Claude model for the agent (set from `[agent].model`).
    #[arg(long, default_value = harness::claude::DEFAULT_MODEL)]
    pub model: String,
    /// The agent harness that runs each turn: `claude` (default) or `hermes`. Overrides the
    /// manifest's `[agent].harness`; when neither is set the engine defaults to claude (see
    /// `apply_agent_cfg`).
    #[arg(long, value_enum)]
    pub harness: Option<harness::Harness>,
    /// Hermes-harness tuning (from `[agent.hermes]`). No CLI flag.
    #[arg(skip)]
    pub hermes: manifest::HermesCfg,
    /// Reasoning-effort tier for the agent, passed to Claude Code as `--effort <level>`. Overrides
    /// `[agent].reasoning_effort`; when neither is set the engine defaults to `medium` (see
    /// `apply_agent_cfg`).
    #[arg(long = "effort", value_enum)]
    pub reasoning_effort: Option<agent::ReasoningEffort>,
    /// Backend for the agent turn: `local` (default) runs it here; `openshell` runs
    /// it in an OpenShell sandbox (what an in-pod loop uses). Needs `--sandbox-image`.
    #[arg(long, value_enum, default_value_t = agent::AgentBackend::Local)]
    pub agent_backend: agent::AgentBackend,
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
    /// Wide-round search config (from `[search]`). No CLI flag, set by `run_from_manifest`.
    #[arg(skip)]
    pub search: Option<manifest::SearchCfg>,
    /// Manifest-only authored workflow.
    #[arg(skip)]
    pub workflow: Option<manifest::WorkflowCfg>,
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
    /// Parse `--max-time` (e.g. `30m`) into a duration; None when unset/invalid.
    fn max_time(&self) -> Option<Duration> {
        parse_duration(&self.max_time)
    }

    /// Parse `--max-park` into a duration; None = wait on an approval indefinitely.
    pub(crate) fn max_park(&self) -> Option<Duration> {
        parse_duration(&self.max_park)
    }

    /// The resolved agent harness: CLI `--harness` > manifest `[agent].harness` (folded on by
    /// `apply_agent_cfg`) > claude. Paths that never see a manifest (rank-grounded, scope) get
    /// the claude default.
    pub(crate) fn harness(&self) -> harness::Harness {
        self.harness.unwrap_or_default()
    }

    /// Resolve where this run's agent events come from. `local` spawns `claude` directly
    /// (parsed as `stream-json`); `openshell` uses the in-Rust OpenShell driver.
    pub(crate) fn agent_source(&self) -> agent::AgentSource {
        if self.agent_backend == agent::AgentBackend::Command {
            return agent::AgentSource::Command(self.agent_cmd.clone().unwrap_or_default());
        }
        match self.agent_backend {
            agent::AgentBackend::Openshell => agent::AgentSource::OpenshellDriver,
            // Local (and the Command case handled above) spawn claude directly.
            _ => agent::AgentSource::LocalClaude,
        }
    }
}

/// Parse a short duration like `90s`, `30m`, `1h`. Empty/garbage → None.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.trim().parse().ok()?;
    let secs = match unit.trim() {
        "" | "s" | "sec" => n,
        "m" | "min" => n * 60.0,
        "h" | "hr" => n * 3600.0,
        _ => return None,
    };
    Some(Duration::from_secs_f64(secs))
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
}

impl Paths {
    fn for_manifest(
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
            escalation,
            provisioning,
            state,
        }
    }

    /// Paths for an isolated task worktree: everything (state, steer, markers) lives inside
    /// the clone so a task cannot touch the parent run's state.
    fn for_worktree(worktree: PathBuf, skills: Option<PathBuf>) -> Self {
        Self {
            skills,
            steer: worktree.join("STEER.md"),
            state: worktree.join("state"),
            session_log: worktree.join("state/session.jsonl"),
            control: worktree.join("state/control.json"),
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
}

fn main() -> Result<()> {
    run::dispatch(Cli::parse())
}

/// One crate-wide lock for tests that mutate a process-global env var (`GITHUB_API_URL`, …). The
/// environ is a single global, so per-module locks wouldn't serialize tests in different modules
/// racing through it (`scope`, `run`, `rank_grounded` all point `GITHUB_API_URL` at a local
/// listener); this is the one guard they share.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_handles_suffixes() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("garbage"), None);
        assert_eq!(parse_duration("10x"), None);
    }
}
