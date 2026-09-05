//! The command line: the default (no subcommand) runs the loop.

pub(crate) mod build;
pub(crate) mod check;
pub(crate) mod init;
pub(crate) mod ps;
pub(crate) mod run;
pub(crate) mod setup;
pub(crate) mod workspace;

use crate::args::Args;
use crate::control::pr_watch;
use crate::openshell;
use crate::scope;
use crate::scope::rank_grounded;
use clap::Parser;
use std::path::PathBuf;

/// Top-level CLI: the default (no subcommand) runs the loop.
#[derive(Parser)]
#[command(
    about = "Agentic autoresearch loop: an LLM proposes, a frozen judge decides. Domain = crucible.toml"
)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct Cli {
    /// Print the controller/engine contract version and exit.
    #[arg(long, exclusive = true)]
    pub(crate) contract_version: bool,
    #[command(subcommand)]
    pub(crate) command: Option<Cmd>,
    #[command(flatten)]
    pub(crate) run: Args,
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
    /// Post-hoc run explainability: fold a run's `session.jsonl` (plus an optional Datadog span
    /// export) into a small flow-model IR and emit it as `.json` (the IR itself), `.dot`
    /// (Graphviz run overview), `.mmd` (mermaid flowchart), or `.html` (self-contained
    /// explainer page) — picked by the `--out` extension.
    Flow(FlowArgs),
}

/// `crucible flow`: post-hoc run explainability, a file-to-file fold over the session log (see
/// [`flow::render`]).
#[derive(clap::Args)]
pub(crate) struct FlowArgs {
    /// The run's session log (`state/session.jsonl`).
    #[arg(long)]
    pub session: PathBuf,
    /// Datadog span export for the same run (spans API v2 objects, a JSON array or
    /// `{"data": [...]}`). Optional: adds real timings and the per-tool-call timeline.
    #[arg(long, conflicts_with = "dd_trace")]
    pub spans: Option<PathBuf>,
    /// Fetch the span export straight from the Datadog API for this trace id instead
    /// of a `--spans` file. Needs `DD_API_KEY` + `DD_APP_KEY` in the environment
    /// (`DD_SITE` for a non-US1 org).
    #[arg(long)]
    pub dd_trace: Option<String>,
    /// How far back the `--dd-trace` search looks (a Datadog duration: `48h`, `7d`).
    #[arg(long, default_value = "48h")]
    pub dd_window: String,
    /// Output path; the extension picks the format: `.json` (the IR), `.dot`, `.mmd`,
    /// `.html` (self-contained explainer page).
    #[arg(long)]
    pub out: PathBuf,
}

/// `crucible plan <show|run>`: compile and inspect a plan, or execute one.
#[derive(clap::Subcommand)]
pub(crate) enum PlanAction {
    /// Compile `workflow.star`; optionally materialize it into a manifest.
    CompileWorkflow {
        /// Starlark workflow source (conventionally `<pack>/workflow.star`).
        #[arg(long)]
        file: PathBuf,
        /// Manifest to materialize in place for validation and freeze.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// A parameter value, `name=value`, repeatable. Values bind during compilation, so the
        /// compiled graph is what they produced rather than a template of them.
        #[arg(long = "param", value_name = "NAME=VALUE")]
        params: Vec<String>,
    },
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
    /// Print the workflow DSL's own surface: every constructor, its lane, and its keyword
    /// arguments. Generated from the compiler's tables, so it describes the binary in hand
    /// rather than a document someone remembered to update.
    DslReference {
        /// `markdown` (the published reference page) or `json` (for tooling).
        #[arg(long, default_value = "markdown")]
        format: DslFormat,
    },
    /// Print a workflow source's `params` block as a JSON Schema document, without evaluating
    /// the source. One declaration serves command-line validation, ask validation, and a
    /// generated launch form.
    Params {
        /// The workflow source to read.
        #[arg(long)]
        file: PathBuf,
    },
    /// Print a frozen pack's resolved output bounds and capability disclosure as JSON, computed
    /// without executing any pack content. The controller extracts this to show an approver what
    /// a pack may write and what it can reach.
    Exposure {
        /// The domain manifest to read.
        #[arg(long)]
        manifest: PathBuf,
        /// The publish target a launcher would pass, for the `draft-pr` engine default. The
        /// manifest's own `[publish].pr_repo` wins over it, exactly as at run time.
        #[arg(long)]
        pr_repo: Option<String>,
    },
    /// Execute a plan with the shell runner: `command` tasks run as real subprocesses,
    /// `agent` tasks run `--agent-cmd` (the command-backend stand-in). Exits nonzero when
    /// the plan does not reach a valid verdict.
    Run {
        /// The plan file to execute. Omit it to build the plan from `--manifest`'s own
        /// `[workflow]`, which is how a playbook runs: the pack names its graph and the engine
        /// compiles it per run.
        #[arg(long, required_unless_present = "manifest")]
        file: Option<PathBuf>,
        /// A parameter value, `name=value`, repeatable. What the pack's `params` block declares
        /// is what it accepts; `crucible plan params --file <source>` prints the schema. Only a
        /// `--manifest` run compiles a graph, so a precompiled `--file` plan takes none.
        #[arg(long = "param", value_name = "NAME=VALUE", conflicts_with = "file")]
        params: Vec<String>,
        /// Total cost ceiling for the run, in USD. A playbook must be given one: its source may
        /// not declare a limit its operator set.
        #[arg(long)]
        max_cost: Option<f64>,
        /// Total wall-clock ceiling for the run (`90s`, `30m`, `2h`). A playbook must be given
        /// one, for the same reason.
        #[arg(long)]
        max_time: Option<String>,
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
        /// OpenShell compute driver for an agent task's sandbox: `podman` (default, nests it
        /// beside the runner) or `kubernetes` (schedules it as a sibling pod in-cluster). Fixed
        /// per deployment, so a rendered wrapper passes it; the manifest cannot declare it.
        #[arg(long, value_enum, default_value_t = openshell::gateway::ComputeDriver::Podman)]
        compute_driver: openshell::gateway::ComputeDriver,
        /// The harness every agent task runs, replacing the manifest's `[agent].harness`. A task
        /// that pins its own harness keeps it. Only a `--manifest` run has an agent to replace.
        #[arg(long, value_enum, requires = "manifest")]
        harness: Option<crate::manifest::Harness>,
        /// The model every agent task asks for, replacing the manifest's `[agent].model`. A task
        /// that pins its own model keeps it.
        #[arg(long, requires = "manifest")]
        model: Option<String>,
    },
}

/// How `crucible plan dsl-reference` renders the DSL surface.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum DslFormat {
    Markdown,
    Json,
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
    /// Branch or tag to clone `--repo-url` at. Omitted: the repo's default branch.
    #[arg(long)]
    pub repo_ref: Option<String>,
    /// A pack the checkout already carries, relative to its root. `scope` turn kind only; when set,
    /// the in-pod invocation validates and freezes that pack (`crucible scope --pack`) instead of
    /// drafting one, so the turn spends no agent and needs no sandbox.
    #[arg(long)]
    pub pack_path: Option<String>,
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
    pub tier: Option<crate::deploy::ProposeTier>,
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
    /// The agent harness the in-pod turn runs, forwarded as `--harness …`. Absent = the in-pod
    /// engine's own default.
    #[arg(long, value_enum)]
    pub harness: Option<crate::manifest::Harness>,
    /// The model the in-pod turn runs, forwarded as `--model …`. Absent = the model the in-pod
    /// engine derives from its harness.
    #[arg(long)]
    pub model: Option<String>,
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
    /// Playbook launch: the rendered pod runs `crucible plan run` over the manifest's `[workflow]`
    /// instead of the agent loop.
    #[arg(long, conflicts_with_all = ["iterations", "controller", "pr_repo", "harness", "model"], requires = "max_time")]
    pub playbook: bool,
    /// Wall-clock ceiling the launched playbook runs under (`90s`, `30m`, `2h`), emitted as the
    /// in-pod `plan run --max-time`. Required with `--playbook`.
    #[arg(long, requires = "playbook", value_parser = clap::value_parser!(crate::duration::MaxTime))]
    pub max_time: Option<crate::duration::MaxTime>,
    /// A playbook parameter value, `name=value`, repeatable. What the pack's `params` block declares
    /// is what it accepts.
    #[arg(long = "param", value_name = "NAME=VALUE", requires = "playbook")]
    pub params: Vec<String>,
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
    /// The agent harness the rendered loop runs, emitted as the wrapper's `--harness=…`. Absent =
    /// the manifest's `[agent].harness`.
    #[arg(long, value_enum)]
    pub harness: Option<crate::manifest::Harness>,
    /// The model the rendered loop runs, emitted as the wrapper's `--model=…`. Absent = the model
    /// the loop derives from its harness.
    #[arg(long)]
    pub model: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deploy_render(extra: &[&str]) -> Result<Cli, clap::Error> {
        let mut argv = vec![
            "crucible",
            "deploy",
            "render",
            "--manifest",
            "pack/crucible.toml",
            "--profile",
            "profile.toml",
        ];
        argv.extend_from_slice(extra);
        <Cli as clap::Parser>::try_parse_from(argv)
    }

    fn deploy_args(cli: Cli) -> DeployArgs {
        match cli.command {
            Some(Cmd::Deploy {
                action: DeployAction::Render(a),
            }) => a,
            _ => panic!("deploy render"),
        }
    }

    #[test]
    fn playbook_requires_a_max_time() {
        assert!(deploy_render(&["--playbook"]).is_err());
    }

    #[test]
    fn max_time_requires_playbook() {
        assert!(deploy_render(&["--max-time", "30m"]).is_err());
    }

    #[test]
    fn param_requires_playbook() {
        assert!(deploy_render(&["--param", "topic=sinks"]).is_err());
    }

    /// `iterations` carries a default, and clap fires a conflict only on a value the operator
    /// actually typed, so the default must not trip it while an explicit flag must.
    #[test]
    fn playbook_rejects_iterations() {
        assert!(deploy_render(&["--playbook", "--max-time", "30m"]).is_ok());
        assert!(deploy_render(&["--playbook", "--max-time", "30m", "--iterations", "3"]).is_err());
    }

    #[test]
    fn playbook_rejects_controller() {
        assert!(deploy_render(&["--playbook", "--max-time", "30m", "--controller"]).is_err());
    }

    /// `plan run` accepts none of these; silently dropping them is the class of bug this mode exists
    /// to remove.
    #[test]
    fn playbook_rejects_harness_model_and_pr_repo() {
        for flag in [
            ["--harness", "claude"],
            ["--model", "claude-opus-4-6"],
            ["--pr-repo", "example/fork"],
        ] {
            assert!(
                deploy_render(&["--playbook", "--max-time", "30m", flag[0], flag[1]]).is_err(),
                "{flag:?}"
            );
        }
    }

    /// The argv the controller's playbook dispatch actually shells, parsed by the real CLI.
    #[test]
    fn playbook_render_accepts_the_controllers_argv() {
        let cli = deploy_render(&[
            "--pack",
            "--pack-configmap-name",
            "crucible-run-42-pack",
            "--playbook",
            "--max-cost",
            "4.5",
            "--max-time",
            "30m",
            "--param",
            "topic=attention sinks",
            "--param",
            "depth=--deep",
        ])
        .expect("the controller's argv parses");
        let args = deploy_args(cli);
        assert!(args.playbook);
        assert!(args.pack);
        assert_eq!(
            args.pack_configmap_name.as_deref(),
            Some("crucible-run-42-pack")
        );
        assert_eq!(args.max_cost, 4.5);
        assert_eq!(
            args.max_time.map(|t| t.to_string()).as_deref(),
            Some("1800s")
        );
        assert_eq!(args.params, vec!["topic=attention sinks", "depth=--deep"]);
    }

    #[test]
    fn spans_and_dd_trace_are_mutually_exclusive() {
        let err = Cli::try_parse_from([
            "crucible",
            "flow",
            "--session",
            "s.jsonl",
            "--spans",
            "x.json",
            "--dd-trace",
            "abc123",
            "--out",
            "f.html",
        ])
        .err()
        .expect("--spans + --dd-trace must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let cli = Cli::try_parse_from([
            "crucible",
            "flow",
            "--session",
            "s.jsonl",
            "--dd-trace",
            "abc123",
            "--out",
            "f.html",
        ])
        .unwrap();
        let Some(Cmd::Flow(args)) = cli.command else {
            panic!("flow parses as its subcommand");
        };
        assert_eq!(args.dd_trace.as_deref(), Some("abc123"));
        assert_eq!(args.dd_window, "48h");
        assert!(
            Cli::try_parse_from([
                "crucible",
                "flow",
                "--session",
                "s.jsonl",
                "--spans",
                "x.json",
                "--out",
                "f.html",
            ])
            .is_ok()
        );
    }
}
