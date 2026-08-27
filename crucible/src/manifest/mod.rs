//! `crucible.toml`: the portable domain manifest. Parses the contract's config (see
//! `docs/crucible-contract.md`) and builds the engine's World + Judge from it, so onboarding a
//! repo is config, not Rust. Unknown keys are rejected (typo protection).

mod backend;
mod broker;
mod deploy;
mod judge;
mod measure;
mod openshell;
mod preflight;
mod relay;
mod search;
mod selftest;
mod wiring;
mod workflow;
mod world;

pub use backend::{AgentBackend, UnknownBackend};
pub use broker::{BrokerCfg, broker_endpoint_from_url, broker_port, resolve_broker_url};
pub use deploy::DeployCfg;
pub use judge::JudgeCfg;
pub use measure::MeasureCfg;
pub use openshell::OpenshellCfg;
pub use preflight::{MODE_PLACEHOLDER, PreflightCfg};
pub use relay::RelayFile;
pub use search::SearchCfg;
pub use selftest::SelftestCfg;
pub use workflow::{KEPT_INPUT, WorkflowCaps, WorkflowCfg, WorkflowError, WorkflowType};
pub use world::WorldCfg;

use crate::command_judge::Direction;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Manifest-level rejections that belong to no single sub-table.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("inject destination {} is a directory", .path.display())]
    InjectDestIsDir { path: PathBuf },
    #[error("[[workspace.inject]] sources not found under {}:\n{}", .manifest_dir.display(), .missing.join("\n"))]
    InjectSrcMissing {
        manifest_dir: PathBuf,
        missing: Vec<String>,
    },
    #[error(
        "[agent.broker].bin is required when the broker is enabled (the domain's broker binary)"
    )]
    BrokerBinRequired,
    #[error("manifest has no [judge] (task mode)")]
    NoJudge,
    #[error("{table} requires a [judge]: a task manifest has no scores to rank, grade, or seed")]
    TaskLaneRejects { table: &'static str },
    #[error("[judge].objective = \"task\" is reserved as the task lane's wire discriminator")]
    TaskObjectiveReserved,
    #[error("[workflow] type = \"playbook\" runs once and scores nothing; remove [judge]")]
    PlaybookNeedsNoJudge,
    #[error(
        "[workflow].file compiled a {compiled:?} workflow but the manifest declares {declared:?}"
    )]
    WorkflowTypeMismatch {
        declared: WorkflowType,
        compiled: WorkflowType,
    },
    #[error("a composite needs at least two [[component]] entries")]
    CompositeTooSmall,
    #[error("duplicate component domain `{domain}`")]
    DuplicateComponent { domain: String },
    #[error(
        "[workspace].carry_forward entry `{entry}` must be a non-empty workspace-relative path with no `.` or `..` component"
    )]
    BadCarryForward { entry: String },
    #[error("[workspace].carry_forward is not supported in a composite manifest")]
    CompositeCarryForward,
    #[error(
        "[agent.codex].api_key_env `{name}` is not an environment variable name (letters, digits, and underscore, not starting with a digit)"
    )]
    BadCodexApiKeyEnv { name: String },
    #[error(
        "[[workspace.artifact]] embed/upload `{embed}` must be a bare file name (no path separators)"
    )]
    BadArtifactEmbed { embed: String },
    #[error("[[workspace.artifact]] is not supported in a composite manifest")]
    CompositeArtifact,
    #[error(
        "nothing can consume the supplied parameter(s) {}: this manifest carries its tasks \
         inline or declares no [workflow], so there is no source to bind them against",
        .names.join(", ")
    )]
    UnconsumableParams { names: Vec<String> },
}

/// Refuse values no compilation will see. Binding happens only while a named source is compiled,
/// so a run shape that never compiles has to say so rather than discard what a launcher supplied.
fn refuse_unconsumable(params: &std::collections::BTreeMap<String, String>) -> Result<()> {
    if params.is_empty() {
        return Ok(());
    }
    Err(ManifestError::UnconsumableParams {
        names: params.keys().cloned().collect(),
    }
    .into())
}

/// Shared tail of `Manifest`/`CompositeManifest`'s `load_frozen`: given the initial working-tree
/// parse (needed only to learn where the workspace is) and that workspace's path, decide whether
/// a frozen (base-commit) source should override it, and reparse if so.
///
/// - Manifest lives outside the workspace (the default `domains/` packs): `None` from
///   [`frozen_manifest_source`], working tree stands, no log noise (this is the unaffected case).
/// - Manifest lives inside the workspace and it's already a git repo with a commit: reparse from
///   that commit's blob, logging which SHA won.
/// - Manifest lives inside the workspace but there's no commit yet (the very first run, nothing
///   converged to pin to): hard-warn and fall back to the working tree for this run only.
fn load_frozen_generic<T>(
    manifest_path: &Path,
    working_tree: T,
    workspace: &Path,
    reparse: impl FnOnce(&str) -> Result<T>,
) -> Result<T> {
    match frozen_manifest_source(manifest_path, workspace) {
        None => Ok(working_tree),
        Some(Ok((sha, text))) => {
            eprintln!(
                "[crucible] manifest {} lives inside the workspace repo — loading its content \
                 from base commit {sha}, not the working tree",
                manifest_path.display()
            );
            reparse(&text)
        }
        Some(Err(_)) => {
            eprintln!(
                "[crucible] WARNING: manifest {} lives inside the workspace-to-be ({}), but no \
                 base commit exists yet to pin it to — this run trusts the working tree. Once \
                 the workspace has a first commit, later runs freeze to it.",
                manifest_path.display(),
                workspace.display()
            );
            Ok(working_tree)
        }
    }
}

/// The exact manifest text [`Manifest::load_frozen`]/[`CompositeManifest::load_frozen`] actually
/// parsed, the working tree, or the frozen base-commit blob when the manifest lives inside
/// `workspace` and the workspace already has a HEAD. The identity digest hashes this content, not
/// the reparsed struct, so it stays in step with whichever source `load_frozen` picked.
pub fn frozen_manifest_text(manifest_path: &Path, workspace: &Path) -> Result<String> {
    let working_tree = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
    match frozen_manifest_source(manifest_path, workspace) {
        None => Ok(working_tree),
        Some(Ok((_, text))) => Ok(text),
        Some(Err(_)) => Ok(working_tree),
    }
}

/// `None` when `manifest_path` doesn't resolve inside `workspace` (the normal, unaffected case).
/// `Some(Ok((sha, text)))` when it does and the workspace repo already has a HEAD to read the
/// manifest's blob from. `Some(Err(_))` when it does but there's no commit yet, the caller
/// treats that as "nothing to freeze against yet", not a hard failure.
fn frozen_manifest_source(
    manifest_path: &Path,
    workspace: &Path,
) -> Option<Result<(String, String)>> {
    let manifest_abs = absolute(manifest_path);
    let workspace_abs = absolute(workspace);
    let rel = match manifest_abs.strip_prefix(&workspace_abs) {
        Ok(rel) if manifest_abs != workspace_abs => rel.to_path_buf(),
        _ => return None,
    };
    Some(git_show_head(&workspace_abs, &rel))
}

/// Resolve `p` against the current directory (if relative) and collapse `.`/`..` components,
/// without requiring the path to exist (the workspace may not be cloned yet), so it's a plain
/// component-wise normalization rather than [`std::fs::canonicalize`].
fn absolute(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Read `rel`'s blob out of `repo_dir`'s HEAD commit via libgit2 (no `git show` subprocess).
fn git_show_head(repo_dir: &Path, rel: &Path) -> Result<(String, String)> {
    let repo = git2::Repository::open(repo_dir)
        .with_context(|| format!("opening workspace repo at {}", repo_dir.display()))?;
    let head = repo
        .head()
        .context("workspace repo has no HEAD yet")?
        .peel_to_commit()
        .context("peel HEAD to commit")?;
    let tree = head.tree().context("HEAD tree")?;
    let entry = tree
        .get_path(rel)
        .with_context(|| format!("{} not found at HEAD", rel.display()))?;
    let obj = entry.to_object(&repo).context("resolve tree entry")?;
    let blob = obj
        .as_blob()
        .context("manifest path in workspace repo is not a blob")?;
    let text = std::str::from_utf8(blob.content())
        .context("manifest blob is not utf8")?
        .to_string();
    Ok((head.id().to_string(), text))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub repo: Repo,
    #[serde(default)]
    pub workspace: Workspace,
    pub agent: AgentCfg,
    /// The frozen objective. Absent = the task lane: no baseline, no scoring, every completed
    /// turn is kept and snapshotted (see [`crate::task_judge::TaskJudge`]).
    #[serde(default)]
    pub judge: Option<JudgeCfg>,
    #[serde(default)]
    pub world: WorldCfg,
    /// Build/deploy targets for the deploy renderer. Optional, a config-tuning domain that never
    /// rebuilds an image leaves it unset.
    #[serde(default)]
    pub deploy: Option<DeployCfg>,
    /// Wide-round search config. Optional, most domains run pure-deep.
    #[serde(default)]
    pub search: Option<SearchCfg>,
    /// Authored iteration graph; absent uses the default workflow.
    #[serde(default)]
    pub workflow: Option<WorkflowCfg>,
    /// Publish-on-keep target for a single-repo run. Optional, a run with no `[publish]` (or no
    /// `pr_repo` in it) records to S3 but opens no draft PR. Composite runs carry their forks
    /// per-component in `[[component]].pr_repo` instead, so this is the single-repo analogue.
    #[serde(default)]
    pub publish: Option<PublishCfg>,
    /// Declarative image builds: named `[build.<name>]` targets. Absent (or empty) means today's
    /// behavior exactly, statically configured images, never rebuilt. The schema is purely
    /// additive; a config-tuning domain never touches it.
    #[serde(default)]
    pub build: BTreeMap<String, forge::spec::BuildSpec>,
    /// The codegen tool contract for a GPU-measured code domain, projected into the loop pod's
    /// broker-child env by the deploy renderer. Optional, only code domains that build + measure a
    /// candidate on a GPU declare it; a config-tuning or live-deployment domain leaves it unset.
    #[serde(default)]
    pub measure: Option<MeasureCfg>,
    /// The zero-agent-cost rung ladder run against the unmodified baseline tree before iteration 1.
    /// Absent = no preflight (the run starts straight at the baseline).
    #[serde(default)]
    pub preflight: Option<PreflightCfg>,
}

/// A single-repo run's publish-on-keep config: the fork the kept commits are pushed to as a draft PR.
/// The composite analogue is `[[component]].pr_repo`; this is the single-domain manifest's version, so
/// a scoped single-repo pack (or a hand-written domain) can name its own fork instead of relying on a
/// `--pr-repo` flag. When set it takes precedence over any `--pr-repo` the caller passes (see
/// `run`'s `run_from_manifest`).
#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct PublishCfg {
    /// `owner/repo` fork the kept-commits branch is pushed to for the draft PR. `None` = don't open a
    /// PR (the S3 record still lands). The push PAT comes from `AUTORESEARCH_PR_TOKEN`/`GITHUB_TOKEN`.
    #[serde(default)]
    pub pr_repo: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    #[serde(default = "default_workspace_dir")]
    pub dir: String,
    #[serde(default)]
    pub setup_cmd: Option<String>,
    /// Baked files copied into the workspace clone after setup, the generic frozen-judge primitive
    /// (a T1 scoring harness, a seeded fixture). See [`Inject`].
    #[serde(default)]
    pub inject: Vec<Inject>,
    /// Workspace-relative paths holding derived pipeline output (code traces, a codegen tree) that a
    /// discard must NOT delete, so iteration N+1 doesn't re-derive what iteration N already paid
    /// for. Each entry is added to `.git/info/exclude` and spared by the discard's clean, so carried
    /// content is invisible to candidate identity: a turn that only regenerates it still reads as no
    /// change. Absent (the default) = today's fresh-start behavior exactly; methodology-sensitive
    /// campaigns simply omit the key.
    ///
    /// Constraints and known gaps:
    /// - A path the repo TRACKS is not protected: git excludes only apply to untracked files, and
    ///   the discard's `reset --hard` reverts tracked content regardless. Carry pipeline output the
    ///   repo doesn't track.
    /// - A path that doesn't exist yet is fine; the exclude is prospective.
    /// - Wide-tournament worktrees are fresh checkouts with no untracked carried dirs, so the wide
    ///   round doesn't benefit.
    /// - Composite manifests reject the key: per-component carry-forward is a non-goal.
    #[serde(default)]
    pub carry_forward: Vec<String>,
    /// Carried like `carry_forward`, and also published. See [`Artifact`].
    #[serde(default)]
    pub artifact: Vec<Artifact>,
}

impl Workspace {
    /// Every carried path: `carry_forward` plus each declared artifact's path. This is the one
    /// list the git memory excludes from candidate identity and spares on a discard's clean.
    pub fn carried_paths(&self) -> Vec<String> {
        let mut all = self.carry_forward.clone();
        for a in &self.artifact {
            if !all.contains(&a.path) {
                all.push(a.path.clone());
            }
        }
        all
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            dir: default_workspace_dir(),
            setup_cmd: None,
            inject: Vec::new(),
            carry_forward: Vec::new(),
            artifact: Vec::new(),
        }
    }
}

/// Derived pipeline output worth publishing: the path is carried (never in a candidate commit
/// or diff), and publish surfaces the newest file matching `embed`/`upload` so a reviewer sees
/// the pipeline's report without it shipping in the PR diff.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Workspace-relative file or directory, same rules as `carry_forward` entries.
    pub path: String,
    /// Bare file name: newest match under `path` is inlined in the PR body + uploaded.
    #[serde(default)]
    pub embed: Option<String>,
    /// Bare file name: newest match is uploaded only (binaries a PR body can't inline).
    #[serde(default)]
    pub upload: Option<String>,
    /// PR-body section heading for the embed. Default: the `path`.
    #[serde(default)]
    pub title: Option<String>,
}

/// One file copied into the workspace clone. `src` is relative to the manifest dir (the baked
/// artifact lives with the domain), `dst` to the workspace dir (into the agent's clone). A `frozen`
/// inject is re-copied before EVERY scored measurement, so the agent can't edit the judge to game
/// the gate; a non-frozen inject is a one-time fixture the agent may then modify. This is the
/// generic alternative to hand-chaining an `install` into `setup_cmd`.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Inject {
    pub src: String,
    pub dst: String,
    #[serde(default = "default_true")]
    pub frozen: bool,
}

fn default_true() -> bool {
    true
}

fn default_workspace_dir() -> String {
    "workspace".to_string()
}

/// Copy one inject `src` -> `dst`, creating parent dirs. Used at setup (all injects) and before each
/// scored measure (frozen injects), so the frozen judge the gate runs is always the baked one.
pub fn apply_inject(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating inject dir {}", parent.display()))?;
    }
    // Replace the entry: copying through an agent-created symlink could escape `dst`.
    let dst_meta = std::fs::symlink_metadata(dst);
    let same_file = matches!(
        (std::fs::canonicalize(src), std::fs::canonicalize(dst)),
        (Ok(src), Ok(dst)) if src == dst
    );
    if dst_meta
        .as_ref()
        .is_ok_and(|meta| !meta.file_type().is_symlink())
        && same_file
    {
        return Ok(());
    }
    match dst_meta {
        Ok(meta) if meta.file_type().is_dir() && !meta.file_type().is_symlink() => {
            return Err(ManifestError::InjectDestIsDir {
                path: dst.to_path_buf(),
            }
            .into());
        }
        Ok(_) => std::fs::remove_file(dst)
            .with_context(|| format!("removing old inject destination {}", dst.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("inspecting inject destination {}", dst.display()));
        }
    }
    std::fs::copy(src, dst)
        .with_context(|| format!("inject {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

/// Which agent harness runs the turn. `claude` is the default everywhere; `hermes` (Nous
/// Research's hermes-agent) and `codex` (OpenAI's Codex CLI) are selected per-domain for harness
/// ablations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    #[default]
    Claude,
    Hermes,
    Codex,
}

impl Harness {
    /// The default model when neither the CLI nor the manifest names one.
    pub const fn default_model(self) -> &'static str {
        match self {
            Harness::Claude => "claude-opus-4-6",
            Harness::Hermes => "claude-opus-4-6",
            Harness::Codex => "gpt-5.6-sol",
        }
    }
}

/// The agent's reasoning-effort tier, passed to Claude Code as `--effort <level>`. A closed set
/// (Claude Code 2.1: low|medium|high|xhigh|max); unset means we don't pass the flag and Claude
/// Code picks its own default. Per-domain because the right tier is task-dependent (mechanical
/// remove-deprecated fixes need ~none; algorithmic issues want real reasoning), and it's a harness
/// hyperparameter worth ablating: does more thinking find better fixes, or just better reward hacks?
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    /// The exact token Claude Code's `--effort` flag expects.
    pub fn as_flag(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCfg {
    #[serde(default = "default_model")]
    pub model: String,
    /// The agent harness that runs each turn: `claude` (default) or `hermes`. Per-domain so a
    /// harness swap is a manifest edit (and an ablation axis), not engine surgery; the CLI
    /// `--harness` overrides it.
    #[serde(default)]
    pub harness: Harness,
    /// Hermes-harness tuning; ignored (and harmless) when `harness = "claude"`.
    #[serde(default)]
    pub hermes: HermesCfg,
    /// Codex-harness tuning; ignored (and harmless) when `harness = "claude"`.
    #[serde(default)]
    pub codex: CodexCfg,
    /// Reasoning-effort tier passed to Claude Code as `--effort <level>` (low|medium|high|xhigh|max).
    /// Unset = the engine default (`medium`); a `--effort` CLI flag overrides both. Set this to opt a
    /// known-hard domain up to `high`/`max`.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Tools the agent must not call, passed to Claude Code as `--disallowed-tools`. Names match
    /// Claude Code's own (`Bash`, `mcp__<server>__<tool>`).
    ///
    /// This exists because a prompt cannot enforce a prohibition. A domain whose measurement is
    /// owned by the engine's rungs can hand the agent a broker that also exposes those rungs as
    /// tools; telling it not to call them held for one iteration and not the next, and a turn that
    /// measures itself spends GPU the engine never sees and runs a second convergence loop under
    /// the one that counts iterations.
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default)]
    pub method_prompt: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub goal_file: Option<String>,
    /// Path (relative to the domain pack) of a diff handed to iteration 1 as declared seed
    /// material. The content is hashed into the run identity and disclosed on the published PR;
    /// the agent applies and validates it itself, it is never silently applied to the tree.
    /// Absent = an unseeded run.
    #[serde(default)]
    pub seed_diff: Option<String>,
    #[serde(default)]
    pub toolbox_dir: Option<String>,
    /// Skill directory names under `toolbox_dir` that must never reach the loop agent's
    /// workspace, setup-only tools that can move the evaluation surface itself (deployment config,
    /// workload/order capture), as opposed to tools that act on the candidate under test. This
    /// lives in the frozen manifest, not the workspace, so the in-loop agent has no path to edit
    /// it. `install_toolbox` refuses to copy any name listed here, and errors if a listed name
    /// doesn't exist under `toolbox_dir` (a stale exclusion is a config bug, not a no-op).
    #[serde(default)]
    pub toolbox_exclude: Vec<String>,
    #[serde(default)]
    pub backend: AgentBackend,
    #[serde(default)]
    pub sandbox_image: Option<String>,
    #[serde(default)]
    pub agent_cmd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Files to materialize into the agent's sandbox before each turn (e.g. a cluster
    /// kubeconfig), each rendered host-side and targeted-uploaded to its `dest`. See
    /// `relay`.
    #[serde(default)]
    pub relay: Vec<RelayFile>,
    /// OpenShell sandbox tuning (egress allowlist) for the `openshell` backend. Empty for
    /// the `local` / `command` backends.
    #[serde(default)]
    pub openshell: OpenshellCfg,
    /// The loop-pod provisioning broker. Off unless a domain opts in.
    #[serde(default)]
    pub broker: BrokerCfg,
}

fn default_model() -> String {
    Harness::default().default_model().to_string()
}

/// `[agent.hermes]`: hermes-harness tuning. Phase B grows this (auth, config.yaml knobs); for
/// now only the model override, mapped to hermes's `anthropic/<model>` syntax at invocation.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HermesCfg {
    /// Model override for hermes turns; unset = the resolved `[agent].model`.
    #[serde(default)]
    pub model: Option<String>,
}

/// `[agent.codex]`: codex-harness tuning. The shared `[agent].model` names a Claude model by
/// default, so a codex domain sets its OpenAI model here rather than retuning every other arm.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexCfg {
    /// Model override for codex turns; unset = the resolved `[agent].model`.
    #[serde(default)]
    pub model: Option<String>,
    /// Authentication selection. `auto` uses the named API-key env when non-empty and otherwise
    /// falls back to ChatGPT OAuth; the explicit modes never switch implicitly.
    #[serde(default)]
    pub auth: CodexAuthMode,
    /// Environment variable holding the selected API key; unset = `OPENAI_API_KEY`.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// Which of Codex's two OpenAI login methods a turn uses. `Auto` preserves the historical
/// environment-sensitive behavior for existing manifests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexAuthMode {
    #[default]
    Auto,
    Api,
    Chatgpt,
}

/// Cross-field checks shared by [`Manifest::validate`] and [`CompositeManifest::validate`]: the
/// broker-bin requirement plus the three sub-validators. Callers add their own shape-specific
/// checks (composite component count/uniqueness) around this.
struct CommonCfg<'a> {
    workspace: &'a Workspace,
    agent: &'a AgentCfg,
    judge: Option<&'a JudgeCfg>,
    search: &'a Option<SearchCfg>,
    workflow: &'a Option<WorkflowCfg>,
    build: &'a BTreeMap<String, forge::spec::BuildSpec>,
    preflight: &'a Option<PreflightCfg>,
    measure: &'a Option<MeasureCfg>,
}

fn validate_common(c: CommonCfg<'_>) -> Result<()> {
    if c.agent.broker.enabled && c.agent.broker.bin.is_empty() {
        return Err(ManifestError::BrokerBinRequired.into());
    }
    validate_carry_forward(&c.workspace.carry_forward)?;
    validate_artifacts(&c.workspace.artifact)?;
    validate_codex_api_key_env(c.agent.codex.api_key_env.as_deref())?;
    search::validate_search(c.search)?;
    if let Some(w) = c.workflow {
        w.validate()?;
    }
    match c.judge {
        Some(judge) => {
            selftest::validate_selftest(&judge.selftest)?;
            if judge.objective == "task" {
                return Err(ManifestError::TaskObjectiveReserved.into());
            }
            if c.workflow
                .as_ref()
                .is_some_and(|w| w.workflow_type == WorkflowType::Playbook)
            {
                return Err(ManifestError::PlaybookNeedsNoJudge.into());
            }
        }
        None => {
            let playbook = c
                .workflow
                .as_ref()
                .is_some_and(|w| w.workflow_type == WorkflowType::Playbook);
            for (present, table) in [
                (c.search.is_some(), "[search]"),
                (c.workflow.is_some() && !playbook, "[workflow]"),
                (c.preflight.is_some(), "[preflight]"),
            ] {
                if present {
                    return Err(ManifestError::TaskLaneRejects { table }.into());
                }
            }
        }
    }
    forge::spec::validate_builds(c.build)?;
    preflight::validate_preflight(c.preflight, c.measure)?;
    Ok(())
}

/// `[workspace].carry_forward` entries name paths inside the workspace, so an absolute path or one
/// climbing out via `..` would have the discard spare something outside the tree it owns. A `.`
/// component is rejected rather than normalized: entries are compared literally against git's
/// workspace-relative status paths and written verbatim as exclude patterns, and `./out` matches
/// neither, so it would silently carry nothing.
fn validate_carry_forward(entries: &[String]) -> Result<()> {
    for entry in entries {
        let trimmed = entry.trim_end_matches('/');
        let bad = trimmed.is_empty()
            || Path::new(entry).is_absolute()
            || Path::new(entry)
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir));
        if bad {
            return Err(ManifestError::BadCarryForward {
                entry: entry.clone(),
            }
            .into());
        }
    }
    Ok(())
}

/// `[agent.codex].api_key_env` names an environment variable of the loop process, and a name
/// outside the portable character set can never be read back, so the turn would fall through to
/// ChatGPT OAuth (or fail the explicit `api` mode) with nothing naming the typo.
fn validate_codex_api_key_env(name: Option<&str>) -> Result<()> {
    let Some(name) = name else {
        return Ok(());
    };
    let portable = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !portable {
        return Err(ManifestError::BadCodexApiKeyEnv {
            name: name.to_string(),
        }
        .into());
    }
    Ok(())
}

/// Paths obey the carry_forward rules; `embed`/`upload` are matched against file names, so a
/// separator in them could never match.
fn validate_artifacts(artifacts: &[Artifact]) -> Result<()> {
    validate_carry_forward(&artifacts.iter().map(|a| a.path.clone()).collect::<Vec<_>>())?;
    for a in artifacts {
        for name in [&a.embed, &a.upload].into_iter().flatten() {
            if name.is_empty() || name.contains('/') || name.contains('\\') {
                return Err(ManifestError::BadArtifactEmbed {
                    embed: name.clone(),
                }
                .into());
            }
        }
    }
    Ok(())
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let m: Self = toml::from_str(&text)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        m.validate()?;
        Ok(m)
    }

    /// Load a manifest for a run, freezing its content at the workspace repo's HEAD when the
    /// manifest lives inside the workspace (the BYO case: dropping `crucible.toml` into a repo
    /// the agent then edits directly, `[workspace].dir` unset or `"."`). Without this, an
    /// interrupted turn or a kept-but-uncommitted edit could leave the working-tree manifest
    /// silently different from the last converged (committed) one, and the next `crucible`
    /// invocation would trust it. See [`load_frozen_generic`] for the fallback rules.
    ///
    /// Structural note: `[workspace].dir` is itself a manifest field, so learning where the
    /// workspace is requires an initial working-tree parse. That parse only ever decides
    /// *whether* to prefer a frozen source; when one exists, its content, not the initial
    /// parse, is what's returned.
    pub fn load_frozen(manifest_path: &Path) -> Result<Self> {
        let working_tree = Self::load(manifest_path)?;
        let manifest_dir = manifest_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let workspace = manifest_dir.join(&working_tree.workspace.dir);
        load_frozen_generic(manifest_path, working_tree, &workspace, |text| {
            let m: Self = toml::from_str(text).with_context(|| {
                format!(
                    "parsing manifest {} at its workspace base commit",
                    manifest_path.display()
                )
            })?;
            m.validate()?;
            Ok(m)
        })
    }

    /// Cross-field checks the type system + `deny_unknown_fields` can't express. Run at load.
    fn validate(&self) -> Result<()> {
        validate_common(CommonCfg {
            workspace: &self.workspace,
            agent: &self.agent,
            judge: self.judge.as_ref(),
            search: &self.search,
            workflow: &self.workflow,
            build: &self.build,
            preflight: &self.preflight,
            measure: &self.measure,
        })
    }

    /// Compile `[workflow].file` into the graph it names. A manifest that carries its tasks
    /// inline is left alone, so this is safe to call on any manifest and callers do not branch.
    ///
    /// Compilation happens per run rather than once at authoring time because a parameterised
    /// graph is a function of its launch arguments: the frozen result is still the runtime
    /// authority, what changes is when it is produced.
    pub fn resolve_workflow(&mut self, manifest_dir: &Path) -> Result<()> {
        self.resolve_workflow_with(manifest_dir, &std::collections::BTreeMap::new())
    }

    /// Resolve with the values a launcher supplied. They are bound during compilation, so a
    /// missing required parameter refuses the run before any task is dispatched.
    pub fn resolve_workflow_with(
        &mut self,
        manifest_dir: &Path,
        params: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        let Some(workflow) = &self.workflow else {
            return refuse_unconsumable(params);
        };
        if !workflow.is_unresolved() {
            return refuse_unconsumable(params);
        }
        let declared = workflow.workflow_type;
        let file = workflow.file.clone().unwrap_or_default();
        let source = manifest_dir.join(&file);
        let compiled = crate::plan::starlark::compile_file_with(&source, manifest_dir, params)
            .with_context(|| format!("compiling [workflow].file = {file:?}"))?;
        if compiled.workflow.workflow_type != declared {
            return Err(ManifestError::WorkflowTypeMismatch {
                declared,
                compiled: compiled.workflow.workflow_type,
            }
            .into());
        }
        let mut resolved = compiled.workflow;
        resolved.resolved_from = Some(file);
        self.workflow = Some(resolved);
        self.validate()?;
        Ok(())
    }

    /// No `[judge]` = the task lane: keep every completed turn, no baseline, no scoring.
    pub fn is_task(&self) -> bool {
        self.judge.is_none()
    }

    /// The `[judge]` table, for callers that only make sense on a scored run. Task-lane
    /// callers must branch on [`Manifest::is_task`] first.
    pub fn require_judge(&self) -> Result<&JudgeCfg> {
        self.judge
            .as_ref()
            .ok_or_else(|| ManifestError::NoJudge.into())
    }

    pub fn direction(&self) -> Result<Direction> {
        judge::parse_direction(&self.require_judge()?.direction)
    }

    pub fn tiebreak_direction(&self) -> Result<Option<Direction>> {
        judge::parse_tiebreak_direction(self.require_judge()?)
    }

    /// Resolve `[workspace].inject` entries to absolute `(src, dst, frozen)`: `src` under
    /// `manifest_dir` (the baked artifact), `dst` under `workspace` (the clone).
    pub fn resolved_injects(
        &self,
        manifest_dir: &Path,
        workspace: &Path,
    ) -> Vec<(PathBuf, PathBuf, bool)> {
        self.workspace
            .inject
            .iter()
            .map(|i| (manifest_dir.join(&i.src), workspace.join(&i.dst), i.frozen))
            .collect()
    }

    /// Frozen injects as `(absolute src, workspace-relative dst)`. Unlike
    /// [`Manifest::resolved_injects`] the destination stays relative: a plan task may run in an
    /// isolated worktree, so the workspace root is only known at dispatch.
    pub fn frozen_inject_pairs(&self, manifest_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
        self.workspace
            .inject
            .iter()
            .filter(|i| i.frozen)
            .map(|i| (manifest_dir.join(&i.src), PathBuf::from(&i.dst)))
            .collect()
    }
}

/// A composite domain: N component domains assembled into one run with a multi-workspace world +
/// one combined gate. A manifest with a top-level `[composite]` table is loaded as this instead of
/// [`Manifest`]. Components are sibling domain dirs (`<domains>/<component.domain>/`), each reused
/// verbatim, the composite owns only the combined `[agent]`/`[judge]`/`[world]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositeManifest {
    pub composite: CompositeCfg,
    /// The composite's base workspace dir (default `workspace`). Each component is checked out into a
    /// subdir of this, so all components co-locate in one tree, one agent cwd, one sandbox upload.
    /// (`[workspace].inject`/`setup_cmd` are unused at the composite level; per-component setup is the
    /// component's own.)
    #[serde(default)]
    pub workspace: Workspace,
    /// `[[component]]` entries (TOML key `component`).
    #[serde(rename = "component")]
    pub components: Vec<ComponentRef>,
    /// The composite-level agent: model, the composite method prompt, backend, broker, egress. The
    /// components contribute their checkouts (and, in later slices, tools/skills); the standing
    /// method is the composite's.
    pub agent: AgentCfg,
    pub judge: JudgeCfg,
    #[serde(default)]
    pub world: WorldCfg,
    /// Per-component deploy targets, keyed by component name. A `[deploy.<component>]` here
    /// overrides that component's own `[deploy]`, the overlay owns issue-specific candidate repos
    /// without forking the base domain manifest.
    #[serde(default)]
    pub deploy: BTreeMap<String, DeployCfg>,
    /// Wide-round search config. Optional.
    #[serde(default)]
    pub search: Option<SearchCfg>,
    /// Authored iteration graph; absent uses the default workflow.
    #[serde(default)]
    pub workflow: Option<WorkflowCfg>,
    /// Declarative image builds: named `[build.<name>]` targets (e.g. a composite's assembled
    /// sandbox image, which `needs` its component images). Absent means no declared builds.
    #[serde(default)]
    pub build: BTreeMap<String, forge::spec::BuildSpec>,
    /// The codegen tool contract for a GPU-measured code domain, projected into the loop pod's
    /// broker-child env by the deploy renderer. Optional, only code domains that build + measure a
    /// candidate on a GPU declare it; a config-tuning or live-deployment domain leaves it unset.
    #[serde(default)]
    pub measure: Option<MeasureCfg>,
    /// The zero-agent-cost rung ladder run against the unmodified baseline tree before iteration 1.
    /// Absent = no preflight (the run starts straight at the baseline).
    #[serde(default)]
    pub preflight: Option<PreflightCfg>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositeCfg {
    pub name: String,
}

/// A reference to a component domain pulled into a composite.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentRef {
    /// The component domain directory name under `domains/`.
    pub domain: String,
    /// Override the checkout subdir name under the composite base workspace. Defaults to `domain`.
    #[serde(default)]
    pub workspace_dir: Option<String>,
    /// The `owner/repo` fork this component opens its draft PR against (e.g. `"wseaton/vllm"`). On a
    /// kept run, publish-on-keep pushes the component's kept commits here as a draft PR (one per
    /// component, cross-linked). `None` = don't publish this component (S3 record still lands).
    #[serde(default)]
    pub pr_repo: Option<String>,
}

/// A component resolved against a composite: its own domain manifest (for `[repo]`/setup) and tools,
/// plus the CHECKOUT location under the composite's base workspace.
pub struct ResolvedComponent {
    pub name: String,
    /// `domains/<name>`, where the component's baked artifacts/tools live.
    pub domain_dir: PathBuf,
    /// The component's own `crucible.toml` (its `[repo]`, setup, etc.).
    pub manifest: Manifest,
    /// `<composite base>/<subdir>`, where this component is checked out for the run.
    pub workspace: PathBuf,
}

/// True if `path` is a composite manifest (a top-level `[composite]` table). Lets the engine pick the
/// loader without fully parsing both shapes.
pub fn is_composite(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
        .is_some_and(|v| v.get("composite").is_some())
}

impl CompositeManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading composite manifest {}", path.display()))?;
        let m: Self = toml::from_str(&text)
            .with_context(|| format!("parsing composite manifest {}", path.display()))?;
        m.validate()?;
        Ok(m)
    }

    /// Composite counterpart to [`Manifest::load_frozen`]: freezes at the base workspace repo's
    /// HEAD when the composite manifest itself lives inside its own base workspace.
    pub fn load_frozen(manifest_path: &Path) -> Result<Self> {
        let working_tree = Self::load(manifest_path)?;
        let manifest_dir = manifest_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let workspace = working_tree.base_dir(manifest_dir);
        load_frozen_generic(manifest_path, working_tree, &workspace, |text| {
            let m: Self = toml::from_str(text).with_context(|| {
                format!(
                    "parsing composite manifest {} at its workspace base commit",
                    manifest_path.display()
                )
            })?;
            m.validate()?;
            Ok(m)
        })
    }

    fn validate(&self) -> Result<()> {
        if self.components.len() < 2 {
            return Err(ManifestError::CompositeTooSmall.into());
        }
        // Composite worlds build one git memory per component, none of which honors the top-level
        // key. Rejecting beats accepting it and silently carrying nothing.
        if !self.workspace.carry_forward.is_empty() {
            return Err(ManifestError::CompositeCarryForward.into());
        }
        if !self.workspace.artifact.is_empty() {
            return Err(ManifestError::CompositeArtifact.into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for c in &self.components {
            if !seen.insert(c.domain.as_str()) {
                return Err(ManifestError::DuplicateComponent {
                    domain: c.domain.clone(),
                }
                .into());
            }
        }
        validate_common(CommonCfg {
            workspace: &self.workspace,
            agent: &self.agent,
            judge: Some(&self.judge),
            search: &self.search,
            workflow: &self.workflow,
            build: &self.build,
            preflight: &self.preflight,
            measure: &self.measure,
        })
    }

    pub fn direction(&self) -> Result<Direction> {
        judge::parse_direction(&self.judge.direction)
    }

    pub fn tiebreak_direction(&self) -> Result<Option<Direction>> {
        judge::parse_tiebreak_direction(&self.judge)
    }

    /// The composite's base workspace dir, holds the per-component checkouts.
    pub fn base_dir(&self, manifest_dir: &Path) -> PathBuf {
        manifest_dir.join(&self.workspace.dir)
    }

    /// The per-component PR fork map `(component name, owner/repo)`, only components that declare a
    /// `pr_repo`. The publish layer joins this with the world's `publish_components` by name to open one
    /// draft PR per fork.
    pub fn component_pr_repos(&self) -> Vec<(String, String)> {
        self.components
            .iter()
            .filter_map(|c| c.pr_repo.clone().map(|r| (c.domain.clone(), r)))
            .collect()
    }

    /// Resolve each component: load its own domain manifest (sibling `<domains>/<domain>/crucible.toml`)
    /// for `[repo]`/setup, and place its checkout at `<base>/<subdir>` so all components co-locate.
    /// `manifest_dir` is the composite manifest's dir; its parent is the shared `domains/` root.
    pub fn resolve_components(&self, manifest_dir: &Path) -> Result<Vec<ResolvedComponent>> {
        let domains_dir = manifest_dir
            .parent()
            .context("composite manifest dir has a parent (the domains/ root)")?;
        let base = self.base_dir(manifest_dir);
        let mut out = Vec::with_capacity(self.components.len());
        for c in &self.components {
            let domain_dir = domains_dir.join(&c.domain);
            let manifest = Manifest::load(&domain_dir.join("crucible.toml"))
                .with_context(|| format!("loading component `{}` manifest", c.domain))?;
            let subdir = c.workspace_dir.clone().unwrap_or_else(|| c.domain.clone());
            out.push(ResolvedComponent {
                name: c.domain.clone(),
                domain_dir,
                manifest,
                workspace: base.join(subdir),
            });
        }
        Ok(out)
    }

    /// The effective deploy target for a component: the composite's `[deploy.<name>]` override if set,
    /// else the component's own `[deploy]`. `None` when neither names one.
    pub fn deploy_for(&self, c: &ResolvedComponent) -> Option<DeployCfg> {
        self.deploy
            .get(&c.name)
            .cloned()
            .or_else(|| c.manifest.deploy.clone())
    }
}

/// Every `[[workspace.inject]].src` must resolve under `manifest_dir`, as a hard error rather than a
/// `crucible check` finding. `deploy render` calls this: a dangling inject renders a perfectly valid-looking pack
/// whose missing file only surfaces inside the sandbox, mid-turn, as whatever the agent was supposed
/// to read not being there.
pub fn ensure_injects_resolve(m: &Manifest, manifest_dir: &Path) -> Result<(), ManifestError> {
    let missing: Vec<String> = m
        .workspace
        .inject
        .iter()
        .filter(|inject| !manifest_dir.join(&inject.src).exists())
        .map(|inject| format!("  {} -> {}", inject.src, inject.dst))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(ManifestError::InjectSrcMissing {
        manifest_dir: manifest_dir.to_path_buf(),
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[agent].model` defaults to the DEFAULT harness's model, not to whatever `[agent].harness`
    /// says: a codex domain names its model in `[agent.codex]` (or `[agent].model`), and the
    /// unset default stays claude's so every existing manifest keeps its resolved model.
    #[test]
    fn an_unset_model_derives_from_the_default_harness() {
        assert_eq!(default_model(), Harness::Claude.default_model());
        let m: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "./m.nu"
            direction = "higher"
            objective = "value"
            [agent]
            backend = "command"
            agent_cmd = "./bump.nu"
            goal = "g"
            harness = "codex"
        "#,
        )
        .unwrap();
        assert_eq!(m.agent.harness, Harness::Codex);
        assert_eq!(m.agent.model, Harness::Claude.default_model());
        assert!(m.agent.codex.model.is_none());
    }

    #[test]
    fn a_codex_api_key_env_that_is_not_an_env_name_is_rejected() {
        assert!(validate_codex_api_key_env(None).is_ok());
        assert!(validate_codex_api_key_env(Some("OPENAI_API_KEY_WORK")).is_ok());
        assert!(validate_codex_api_key_env(Some("_key0")).is_ok());
        for bad in [
            "",
            "0KEY",
            "OPENAI-API-KEY",
            "OPENAI_API_KEY=x",
            "OPENAI API KEY",
        ] {
            let err = validate_codex_api_key_env(Some(bad))
                .expect_err("an unreadable env name must be rejected");
            assert!(err.to_string().contains(bad), "{err}");
        }
    }

    #[test]
    fn parses_a_minimal_gitworld_manifest() {
        let m: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "./measure.nu"
            direction = "higher"
            objective = "value"
            [agent]
            backend = "command"
            agent_cmd = "./bump.nu"
            goal = "raise it"
        "#,
        )
        .unwrap();
        assert_eq!(m.judge.as_ref().unwrap().measure_cmd, "./measure.nu");
        assert_eq!(m.direction().unwrap(), Direction::Higher);
        assert_eq!(m.workspace.dir, "workspace"); // default
        assert!(m.world.snapshot_cmd.is_none(), "no [world] -> GitWorld");
        assert!(m.publish.is_none(), "no [publish] -> no PR target");
        assert!(!m.is_task(), "a [judge] manifest is not a task");
    }

    #[test]
    fn no_judge_is_the_task_lane() {
        let m: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "command"
            agent_cmd = "./chore.sh"
            goal = "do the chore"
        "#,
        )
        .unwrap();
        m.validate().expect("a judge-less manifest is valid");
        assert!(m.is_task());
        assert!(m.direction().is_err(), "no [judge] -> no direction");
        assert!(m.require_judge().is_err());
        let judge = m
            .build_judge(PathBuf::from("unused"), vec![])
            .expect("task judge needs no workspace");
        assert_eq!(judge.objective(), "task", "no [judge] -> TaskJudge");
    }

    #[test]
    fn task_lane_rejects_search_workflow_and_preflight() {
        for table in [
            "[search]\nwide = 2\napproaches = [\"a\", \"b\"]",
            "[workflow]\ntype = \"custom\"\nresult = \"t\"\n[[workflow.task]]\nname = \"t\"\nkind = \"evaluate\"\ncommand = \"true\"",
            "[preflight]\ncommands = [\"true\"]",
        ] {
            let toml = format!(
                r#"
                [repo]
                path = "."
                [agent]
                backend = "command"
                agent_cmd = "true"
                goal = "g"
                {table}
            "#
            );
            let m: Manifest = toml::from_str(&toml).expect("parses");
            let err = m.validate().expect_err(table);
            assert!(
                err.to_string().contains("requires a [judge]"),
                "{table}: {err:#}"
            );
        }
    }

    #[test]
    fn a_judgeless_manifest_admits_a_playbook_workflow_and_still_refuses_the_others() {
        let playbook = "[workflow]\ntype = \"playbook\"\n[[workflow.task]]\nname = \"t\"\nkind = \"evaluate\"\ncommand = \"true\"";
        let m: Manifest = toml::from_str(&task_manifest(playbook)).expect("parses");
        m.validate()
            .expect("a playbook is the one [workflow] the task lane takes");
        assert!(m.is_task(), "a playbook manifest is still judgeless");

        let custom = "[workflow]\ntype = \"custom\"\nresult = \"t\"\n[[workflow.task]]\nname = \"t\"\nkind = \"evaluate\"\ncommand = \"true\"";
        let err = toml::from_str::<Manifest>(&task_manifest(custom))
            .expect("parses")
            .validate()
            .expect_err("custom still needs a judge");
        assert!(err.to_string().contains("requires a [judge]"), "{err:#}");
    }

    /// A manifest may name its source instead of carrying its output. The graph is compiled
    /// on resolve and validated then, so an unresolved manifest is valid and an unreadable
    /// source fails where it is read rather than where it is used.
    #[test]
    fn a_manifest_may_name_its_workflow_source_instead_of_carrying_it() {
        let dir = std::env::temp_dir().join(format!("crucible-wf-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("graph.star"),
            "a = command(name = \"a\", run = \"true\")\nworkflow(type = \"playbook\", tasks = [a])\n",
        )
        .unwrap();

        let table = "[workflow]\ntype = \"playbook\"\nfile = \"graph.star\"";
        let mut m: Manifest = toml::from_str(&task_manifest(table)).expect("parses");
        m.validate().expect("an unresolved manifest is valid");
        assert!(
            m.workflow.as_ref().unwrap().is_unresolved(),
            "the graph is still a path"
        );
        m.resolve_workflow(&dir).expect("compiles");
        let workflow = m.workflow.as_ref().unwrap();
        assert!(!workflow.is_unresolved());
        assert_eq!(workflow.tasks.len(), 1);
        assert_eq!(workflow.tasks[0].name.0, "a");

        // Resolving again is a no-op, so a caller never has to ask whether it already ran.
        m.resolve_workflow(&dir).expect("idempotent");
        assert_eq!(m.workflow.as_ref().unwrap().tasks.len(), 1);

        // The lane the source compiles must be the lane the manifest admitted.
        let mismatched = "[workflow]\ntype = \"custom\"\nfile = \"graph.star\"";
        let err = toml::from_str::<Manifest>(&task_manifest(mismatched))
            .expect("parses")
            .resolve_workflow(&dir)
            .expect_err("custom manifest, playbook source");
        assert!(
            err.to_string().contains("but the manifest declares"),
            "{err:#}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Binding happens only while a named source is compiled. A run shape with nothing to
    /// compile therefore refuses the values a launcher supplied instead of dropping them, so
    /// the same `--param` behaves the same whichever way a pack spells its graph.
    #[test]
    fn supplied_parameters_no_compilation_can_consume_are_refused() {
        let supplied = std::collections::BTreeMap::from([
            ("depth".to_string(), "3".to_string()),
            ("mode".to_string(), "fast".to_string()),
        ]);
        let dir =
            std::env::temp_dir().join(format!("crucible-wf-unbindable-{}", std::process::id()));

        let inline = "[workflow]\ntype = \"playbook\"\n[[workflow.task]]\nname = \"a\"\nkind = \"command\"\ncommand = \"true\"";
        let err = toml::from_str::<Manifest>(&task_manifest(inline))
            .expect("parses")
            .resolve_workflow_with(&dir, &supplied)
            .expect_err("inline tasks bind nothing");
        assert!(err.to_string().contains("depth, mode"), "{err:#}");

        let err = toml::from_str::<Manifest>(&task_manifest(""))
            .expect("parses")
            .resolve_workflow_with(&dir, &supplied)
            .expect_err("no [workflow] binds nothing");
        assert!(err.to_string().contains("depth, mode"), "{err:#}");

        toml::from_str::<Manifest>(&task_manifest(inline))
            .expect("parses")
            .resolve_workflow_with(&dir, &std::collections::BTreeMap::new())
            .expect("nothing supplied, nothing to refuse");
    }

    #[test]
    fn a_workflow_declaring_both_a_file_and_tasks_is_refused() {
        let both = "[workflow]\ntype = \"playbook\"\nfile = \"graph.star\"\n[[workflow.task]]\nname = \"a\"\nkind = \"command\"\ncommand = \"true\"";
        let err = toml::from_str::<Manifest>(&task_manifest(both))
            .expect("parses")
            .validate()
            .expect_err("one or the other");
        assert!(
            err.to_string().contains("it is one or the other"),
            "{err:#}"
        );
    }

    #[test]
    fn a_playbook_is_refused_beside_a_judge() {
        let toml = r#"
            [repo]
            path = "."
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "g"
            [judge]
            objective = "latency"
            direction = "minimize"
            measure_cmd = "true"
            [workflow]
            type = "playbook"
            [[workflow.task]]
            name = "t"
            kind = "evaluate"
            command = "true"
        "#;
        let err = toml::from_str::<Manifest>(toml)
            .expect("parses")
            .validate()
            .expect_err("a scored playbook is a contradiction");
        assert!(err.to_string().contains("remove [judge]"), "{err:#}");
    }

    #[test]
    fn a_playbook_refuses_a_task_naming_an_engine_operation() {
        let playbook = "[workflow]\ntype = \"playbook\"\n[[workflow.task]]\nname = \"p\"\nkind = \"engine\"\nop = \"propose\"";
        let err = toml::from_str::<Manifest>(&task_manifest(playbook))
            .expect("parses")
            .validate()
            .expect_err("propose belongs to the scored loop");
        assert!(
            err.to_string().contains("names engine operation"),
            "{err:#}"
        );
    }

    fn task_manifest(table: &str) -> String {
        format!(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "g"
            {table}
        "#
        )
    }

    #[test]
    fn scored_judge_cannot_claim_the_task_objective() {
        let m: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            objective = "task"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
        "#,
        )
        .expect("parses");
        let err = m.validate().expect_err("objective task is reserved");
        assert!(err.to_string().contains("reserved"), "{err:#}");
    }

    #[test]
    fn composite_still_requires_a_judge() {
        let err = match toml::from_str::<CompositeManifest>(
            r#"
            [composite]
            name = "c"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            [[component]]
            domain = "x"
        "#,
        ) {
            Ok(_) => panic!("a composite without [judge] must not parse"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("judge"), "{err}");
    }

    /// `[preflight]` on a codegen manifest: the rung ladder, the `{mode}` fan-out list it needs,
    /// and the optional baseline rung.
    fn preflight_manifest(preflight: &str, modes: &str) -> String {
        format!(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            objective = "v"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            [measure]
            gpus = 2
            [measure.build]
            base_image = "reg/base:latest"
            mutable_kwargs = {{ mode = {modes} }}
            {preflight}
        "#
        )
    }

    #[test]
    fn preflight_parses_its_ladder_and_baseline() {
        let m: Manifest = toml::from_str(&preflight_manifest(
            r#"
            [preflight]
            commands = ["gate.py --rung 1 --mode {mode}", "gate.py --rung 2 --digest {digest}"]
            baseline = "gate.py --rung 3 --digest {digest}"
        "#,
            r#"["derive", "full"]"#,
        ))
        .expect("preflight parses");
        m.validate().expect("a declared mode list satisfies {mode}");
        let cfg = m.preflight.expect("[preflight] present");
        assert_eq!(cfg.commands.len(), 2);
        assert_eq!(
            cfg.baseline.as_deref(),
            Some("gate.py --rung 3 --digest {digest}")
        );
        assert_eq!(
            m.measure.and_then(|x| x.build_modes()),
            Some(vec!["derive".to_string(), "full".to_string()]),
            "the {{mode}} fan-out list comes from [measure.build].mutable_kwargs.mode"
        );
    }

    #[test]
    fn preflight_rejects_an_unknown_key() {
        let parsed = toml::from_str::<Manifest>(&preflight_manifest(
            r#"
            [preflight]
            commands = ["gate.py"]
            rungs = 3
        "#,
            r#"["derive"]"#,
        ));
        let err = match parsed {
            Err(e) => e,
            Ok(_) => panic!("unknown [preflight] key is a typo, not a feature"),
        };
        assert!(err.to_string().contains("rungs"), "{err}");
    }

    #[test]
    fn preflight_rejects_an_empty_ladder() {
        let m: Manifest = toml::from_str(&preflight_manifest(
            "[preflight]\ncommands = []\n",
            r#"["derive"]"#,
        ))
        .unwrap();
        let err = m.validate().expect_err("an empty ladder validates nothing");
        assert!(
            err.to_string().contains("[preflight].commands is empty"),
            "{err:#}"
        );
    }

    #[test]
    fn preflight_mode_placeholder_needs_a_declared_mode_list() {
        // No [measure] at all: nothing to fan {mode} out over.
        let bare = r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            objective = "v"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            [preflight]
            commands = ["gate.py --mode {mode}"]
        "#;
        let m: Manifest = toml::from_str(bare).unwrap();
        let err = m
            .validate()
            .expect_err("{mode} with no [measure] is unexpandable");
        assert!(
            err.to_string()
                .contains("[measure.build].mutable_kwargs.mode"),
            "the error names the missing key: {err:#}"
        );
        // Declared but empty is the same verdict: zero expansions means the rung never runs.
        let m: Manifest = toml::from_str(&preflight_manifest(
            "[preflight]\ncommands = [\"gate.py --mode {mode}\"]\n",
            "[]",
        ))
        .unwrap();
        let err = m
            .validate()
            .expect_err("an empty mode list expands to nothing");
        assert!(
            err.to_string()
                .contains("[measure.build].mutable_kwargs.mode"),
            "{err:#}"
        );
    }

    #[test]
    fn parses_and_validates_workspace_carry_forward() {
        let manifest = |carry: &str| {
            format!(
                r#"
            [repo]
            path = "."
            [workspace]
            carry_forward = {carry}
            [judge]
            measure_cmd = "m"
            direction = "higher"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
        "#
            )
        };
        let dir = tempdir("carry-forward");
        let path = dir.join("crucible.toml");

        std::fs::write(
            &path,
            manifest(r#"["codegen-out/", "traces", "target/codegen"]"#),
        )
        .unwrap();
        let m = Manifest::load(&path).expect("valid carry_forward loads");
        assert_eq!(
            m.workspace.carry_forward,
            ["codegen-out/", "traces", "target/codegen"]
        );

        // An absolute path or a `..` escape would have the discard spare files outside the
        // workspace; an empty entry would spare everything. `.` components are rejected too:
        // entries are matched literally against git's relative paths, so `./out` would carry
        // nothing at all.
        for bad in [
            r#"["/etc"]"#,
            r#"["../secrets"]"#,
            r#"["out/../.."]"#,
            r#"["/"]"#,
            r#"["./codegen-out"]"#,
            r#"["."]"#,
        ] {
            std::fs::write(&path, manifest(bad)).unwrap();
            let err = match Manifest::load(&path) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("{bad} must be rejected"),
            };
            assert!(err.contains("carry_forward"), "{bad}: {err}");
        }

        // Absent key = today's fresh-start behavior.
        std::fs::write(&path, manifest("[]").replace("carry_forward = []", "")).unwrap();
        let no_key = match Manifest::load(&path) {
            Ok(m) => m,
            Err(e) => panic!("absent key must load: {e}"),
        };
        assert!(
            no_key.workspace.carry_forward.is_empty(),
            "absent key = fresh start"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_validates_and_carries_workspace_artifacts() {
        let manifest = |artifact: &str| {
            format!(
                r#"
            [repo]
            path = "."
            [workspace]
            carry_forward = ["cross-trace/"]
            {artifact}
            [judge]
            measure_cmd = "m"
            direction = "higher"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
        "#
            )
        };
        let dir = tempdir("workspace-artifact");
        let path = dir.join("crucible.toml");

        std::fs::write(
            &path,
            manifest(
                r#"[[workspace.artifact]]
                path = "codegen-out/runtime"
                embed = "summary.txt"
                title = "Runtime investigation"
                [[workspace.artifact]]
                path = "cross-trace/"
        "#,
            ),
        )
        .unwrap();
        let m = Manifest::load(&path).expect("valid artifact loads");
        assert_eq!(m.workspace.artifact.len(), 2);
        assert_eq!(
            m.workspace.artifact[0].embed.as_deref(),
            Some("summary.txt")
        );
        assert!(m.workspace.artifact[1].embed.is_none());
        // Artifact paths join the carried set, deduped against carry_forward.
        assert_eq!(
            m.workspace.carried_paths(),
            ["cross-trace/", "codegen-out/runtime"]
        );

        // Artifact paths obey the carry_forward path rules; embed must be a bare file name
        // (it is matched against file names while walking, so a separator can never match).
        for (bad, needle) in [
            (
                "[[workspace.artifact]]\npath = \"../secrets\"\n",
                "carry_forward",
            ),
            (
                "[[workspace.artifact]]\npath = \"out\"\nembed = \"a/b.txt\"\n",
                "embed",
            ),
            (
                "[[workspace.artifact]]\npath = \"out\"\nembed = \"\"\n",
                "embed",
            ),
        ] {
            std::fs::write(&path, manifest(bad)).unwrap();
            let err = match Manifest::load(&path) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("{bad} must be rejected"),
            };
            assert!(err.contains(needle), "{bad}: {err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_publish_pr_repo() {
        let base = r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            objective = "v"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
        "#;
        // A `[publish] pr_repo` names the single-repo fork the kept-commits draft PR opens against.
        let m: Manifest = toml::from_str(&format!(
            "{base}\n[publish]\npr_repo = \"wseaton/relay-testbed\"\n"
        ))
        .unwrap();
        assert_eq!(
            m.publish.and_then(|p| p.pr_repo).as_deref(),
            Some("wseaton/relay-testbed")
        );
        // A bare `[publish]` table with no `pr_repo` parses to `None` (S3 record still lands, no PR).
        let m: Manifest = toml::from_str(&format!("{base}\n[publish]\n")).unwrap();
        assert!(m.publish.expect("table present").pr_repo.is_none());
    }

    #[test]
    fn reasoning_effort_parses_and_rejects_unknown() {
        let toml_with = |eff: &str| {
            format!(
                r#"
                [repo]
                path = "."
                [judge]
                measure_cmd = "m"
                direction = "higher"
                objective = "v"
                [agent]
                backend = "command"
                agent_cmd = "a"
                goal = "g"
                reasoning_effort = "{eff}"
            "#
            )
        };
        let m: Manifest = toml::from_str(&toml_with("xhigh")).unwrap();
        assert_eq!(m.agent.reasoning_effort, Some(ReasoningEffort::Xhigh));
        // Closed set: a bogus tier is a parse error, not a silent default.
        assert!(toml::from_str::<Manifest>(&toml_with("turbo")).is_err());
    }

    #[test]
    fn reasoning_effort_defaults_to_none() {
        let m: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            objective = "v"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
        "#,
        )
        .unwrap();
        assert!(m.agent.reasoning_effort.is_none());
    }

    #[test]
    fn parses_inject_and_resolves_paths() {
        let m: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            objective = "v"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            [workspace]
            dir = "ws"
            [[workspace.inject]]
            src = "judges/h_test.go"
            dst = "pkg/x/h_test.go"
            [[workspace.inject]]
            src = "fixtures/seed.json"
            dst = "testdata/seed.json"
            frozen = false
        "#,
        )
        .unwrap();
        assert_eq!(m.workspace.inject.len(), 2);
        assert!(m.workspace.inject[0].frozen, "frozen defaults to true");
        assert!(!m.workspace.inject[1].frozen);

        let md = Path::new("/repo");
        let ws = Path::new("/repo/ws");
        let resolved = m.resolved_injects(md, ws);
        assert_eq!(
            resolved[0],
            (
                PathBuf::from("/repo/judges/h_test.go"),
                PathBuf::from("/repo/ws/pkg/x/h_test.go"),
                true,
            )
        );
    }

    #[test]
    fn apply_inject_copies_and_creates_parents() {
        let tmp = std::env::temp_dir().join(format!("inject-test-{}", std::process::id()));
        let src = tmp.join("src.txt");
        let dst = tmp.join("nested/deeper/dst.txt");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&src, b"frozen-judge").unwrap();
        apply_inject(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"frozen-judge");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn apply_inject_replaces_a_destination_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir().join(format!("inject-symlink-test-{}", std::process::id()));
        let src = tmp.join("src.txt");
        let outside = tmp.join("outside.txt");
        let dst = tmp.join("dst.txt");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&src, b"frozen-judge").unwrap();
        std::fs::write(&outside, b"do-not-touch").unwrap();
        symlink(&outside, &dst).unwrap();

        apply_inject(&src, &dst).unwrap();

        assert!(
            !std::fs::symlink_metadata(&dst)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"frozen-judge");
        assert_eq!(std::fs::read(&outside).unwrap(), b"do-not-touch");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The synthetic fixture packs under `tests/fixtures/domains/`, the core suite's stand-ins
    /// for real domain packs, which live out of tree.
    fn fixture_domains() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/domains")
    }

    #[test]
    fn command_world_fixture_manifests_parse_and_build() {
        // A CommandWorld domain pack (the alpha fixture) must satisfy the contract: parse, name a
        // CommandWorld (deployment snapshot/restore), and a lower-is-better CommandJudge.
        let alpha_dir = fixture_domains().join("alpha");
        for name in ["crucible.toml", "crucible.test.toml"] {
            let m =
                Manifest::load(&alpha_dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(
                m.direction().unwrap(),
                Direction::Lower,
                "{name} lower wins"
            );
            assert!(
                m.world.snapshot_cmd.is_some() && m.world.restore_cmd.is_some(),
                "{name} drives the rig as a CommandWorld"
            );
            assert!(
                m.agent.env.contains_key("ANTHROPIC_VERTEX_PROJECT_ID"),
                "{name} carries Vertex creds in [agent].env"
            );
            // Goal must be resolvable (inline or a readable manifest-relative file).
            match (&m.agent.goal, &m.agent.goal_file) {
                (Some(_), _) => {}
                (None, Some(f)) => {
                    assert!(alpha_dir.join(f).exists(), "{name} goal_file {f} exists")
                }
                (None, None) => panic!("{name} has neither goal nor goal_file"),
            }
        }
    }

    #[test]
    fn composite_fixture_parses_and_resolves() {
        // A combined domain must parse, name its components, resolve each to a sibling component
        // domain dir, and build a multi-workspace world + the combined judge, all from config, no
        // engine special-casing. Both the base manifest and the issue overlays load.
        let dir = fixture_domains().join("gamma");
        let m = CompositeManifest::load(&dir.join("crucible.toml")).expect("base composite parses");
        assert_eq!(m.composite.name, "gamma");
        assert_eq!(m.direction().unwrap(), Direction::Higher);
        let comps: Vec<&str> = m.components.iter().map(|c| c.domain.as_str()).collect();
        assert_eq!(comps, ["alpha", "beta"], "components in order");

        // Goal + composite method prompt resolve on disk.
        let goal = m.agent.goal_file.as_ref().expect("goal_file");
        assert!(dir.join(goal).exists(), "goal_file {goal} exists");
        let prompt = m.agent.method_prompt.as_ref().expect("method_prompt");
        assert!(dir.join(prompt).exists(), "method_prompt {prompt} exists");

        // Components co-locate under the composite's base workspace (each a subdir); their own domain
        // manifests load (for [repo]/setup) and domain_dir points at the component pack.
        let cr = m.resolve_components(&dir).expect("resolve components");
        assert_eq!(cr.len(), 2);
        assert_eq!(cr[0].name, "alpha");
        assert!(
            cr[0].workspace.ends_with("domains/gamma/workspace/alpha"),
            "alpha checkout co-located under composite base: {}",
            cr[0].workspace.display()
        );
        assert!(
            cr[0].domain_dir.ends_with("domains/alpha"),
            "domain_dir points at the component pack: {}",
            cr[0].domain_dir.display()
        );
        assert!(
            cr[1].workspace.ends_with("domains/gamma/workspace/beta"),
            "beta checkout co-located: {}",
            cr[1].workspace.display()
        );

        // World + judge build (no fs touched; workspaces need not exist yet).
        m.build_world(&dir).expect("build composite world");
        m.build_judge(&dir).expect("build composite judge");

        // The issue overlays are the same shape with their own goal/prompt/gate, each must parse +
        // resolve + build.
        for (file, name) in [
            ("crucible.delta.toml", "delta"),
            ("crucible.omega.toml", "omega"),
        ] {
            let o = CompositeManifest::load(&dir.join(file))
                .unwrap_or_else(|e| panic!("{file}: {e:#}"));
            assert_eq!(o.composite.name, name);
            assert!(
                dir.join(o.agent.goal_file.as_ref().unwrap()).exists(),
                "{file} goal"
            );
            assert!(
                dir.join(o.agent.method_prompt.as_ref().unwrap()).exists(),
                "{file} prompt"
            );
            o.build_world(&dir)
                .unwrap_or_else(|e| panic!("{file} build_world: {e:#}"));
            o.build_judge(&dir)
                .unwrap_or_else(|e| panic!("{file} build_judge: {e:#}"));
        }
    }

    #[test]
    fn component_pr_repos_maps_only_declared_forks() {
        // The multi-fork publish map: only components that declare a `pr_repo` are returned, keyed by
        // component name. A component without one is silently absent (no PR, S3 record only).
        let m: CompositeManifest = toml::from_str(
            r#"
            [composite]
            name = "x"
            [[component]]
            domain  = "vllm"
            pr_repo = "wseaton/vllm"
            [[component]]
            domain  = "epp"
            pr_repo = "wseaton/llm-d-router"
            [[component]]
            domain = "coordinator"
            [agent]
            goal = "g"
            [judge]
            measure_cmd = "m"
            direction = "higher"
        "#,
        )
        .expect("parses with per-component pr_repo");
        assert_eq!(
            m.component_pr_repos(),
            vec![
                ("vllm".to_string(), "wseaton/vllm".to_string()),
                ("epp".to_string(), "wseaton/llm-d-router".to_string()),
            ],
            "only forked components map; coordinator (no pr_repo) is absent"
        );
    }

    #[test]
    fn overlay_fixture_declares_both_forks() {
        // An overlay must carry both component forks so the native publisher opens both PRs.
        let dir = fixture_domains().join("gamma");
        let m = CompositeManifest::load(&dir.join("crucible.delta.toml")).expect("overlay parses");
        assert_eq!(
            m.component_pr_repos(),
            vec![
                ("alpha".to_string(), "example/alpha-fork".to_string()),
                ("beta".to_string(), "example/beta-fork".to_string()),
            ]
        );
    }

    #[test]
    fn composite_rejects_single_component() {
        let one = r#"
            [composite]
            name = "x"
            [[component]]
            domain = "vllm"
            [agent]
            goal = "g"
            [judge]
            measure_cmd = "m"
            direction = "higher"
        "#;
        let m: CompositeManifest = toml::from_str(one).unwrap();
        assert!(m.validate().is_err(), "a composite needs >= 2 components");
    }

    #[test]
    fn composite_rejects_carry_forward() {
        let text = r#"
            [composite]
            name = "x"
            [[component]]
            domain = "vllm"
            [[component]]
            domain = "kernels"
            [workspace]
            carry_forward = ["codegen-out"]
            [agent]
            goal = "g"
            [judge]
            measure_cmd = "m"
            direction = "higher"
        "#;
        let m: CompositeManifest = toml::from_str(text).unwrap();
        let err = match m.validate() {
            Err(e) => e.to_string(),
            Ok(()) => panic!("composite carry_forward must be rejected, not silently ignored"),
        };
        assert!(err.contains("carry_forward"), "{err}");
    }

    #[test]
    fn broker_enabled_requires_bin() {
        // Each domain ships its own broker binary (it injects the domain resolver into the generic
        // crucible-broker engine), so there's no default bin: enabling the broker without naming one
        // is a config error caught at load, not a silent spawn of the empty string.
        let base = r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            [agent]
            backend = "openshell"
            goal = "g"
            [agent.broker]
            enabled = true
        "#;
        let m: Manifest = toml::from_str(base).unwrap();
        assert!(
            m.validate().is_err(),
            "enabled broker with no bin must fail validation"
        );
        let m2: Manifest =
            toml::from_str(&format!("{base}\n            bin = \"my-domain-broker\"")).unwrap();
        assert!(m2.validate().is_ok(), "naming the bin satisfies it");
    }

    #[test]
    fn config_only_fixture_parses() {
        // A config-only pack (the beta fixture) must satisfy the contract from config alone: parse,
        // a higher-is-better gate, a resolvable goal + method prompt, and no [world] (GitWorld). This
        // is the onboarding-interface canary, if a manifest field changes shape, this fails before a
        // new domain author hits it.
        let dir = fixture_domains().join("beta");
        let m = Manifest::load(&dir.join("crucible.toml")).expect("beta manifest parses");
        assert_eq!(
            m.direction().unwrap(),
            Direction::Higher,
            "throughput: higher wins"
        );
        let goal = m.agent.goal_file.as_ref().expect("goal_file");
        assert!(dir.join(goal).exists(), "goal_file {goal} exists");
        let prompt = m.agent.method_prompt.as_ref().expect("method_prompt");
        assert!(dir.join(prompt).exists(), "method_prompt {prompt} exists");
        assert!(
            m.world.snapshot_cmd.is_none() && m.world.restore_cmd.is_none(),
            "skeleton is GitWorld: no rig snapshot/restore yet"
        );
        assert!(!m.agent.broker.enabled, "broker off in the skeleton");
    }

    /// An issue-overlay pack with a frozen judge harness (the alpha issue fixture) must parse +
    /// build, its goal file must exist, and its frozen judge inject must resolve to a baked file
    /// that exists (else the gate has nothing to re-establish).
    #[test]
    fn issue_overlay_fixture_parses_and_inject_resolves() {
        let alpha_dir = fixture_domains().join("alpha");
        let m = Manifest::load(&alpha_dir.join("crucible.issue.toml")).expect("overlay parses");
        let f = m.agent.goal_file.as_ref().expect("goal_file");
        assert!(alpha_dir.join(f).exists(), "goal_file {f} exists");
        let ws = alpha_dir.join(&m.workspace.dir);
        m.build_judge(ws.clone(), vec![]).expect("build_judge");
        // The frozen harness must be baked + resolve under the manifest dir.
        let injects = m.resolved_injects(&alpha_dir, &ws);
        assert_eq!(injects.len(), 1, "the overlay injects exactly the harness");
        let (src, _dst, frozen) = &injects[0];
        assert!(frozen, "the harness is frozen (re-copied each measure)");
        assert!(src.exists(), "baked harness {} exists", src.display());
    }

    #[test]
    fn build_absent_means_no_declared_builds() {
        // no `[build]` block => today's behavior exactly (empty map, nothing to rebuild).
        let m: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
        "#,
        )
        .unwrap();
        assert!(m.build.is_empty());
        m.validate().unwrap();
    }

    #[test]
    fn build_block_parses_and_validates_through_the_manifest() {
        let m: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            [build.sandbox]
            backend = "cluster"
            image   = "ghcr.io/neuralmagic/alpha-sandbox"
            timeout = "45m"
            [build.sandbox.cluster]
            containerfile = "packs/alpha/Containerfile.sandbox"
            [build.sandbox.watch]
            paths = ["packs/alpha/Containerfile.sandbox"]
        "#,
        )
        .unwrap();
        m.validate().unwrap();
        assert_eq!(m.build.len(), 1);
        assert_eq!(
            m.build["sandbox"].backend,
            forge::spec::BuildBackend::Cluster
        );
    }

    #[test]
    fn a_bad_build_block_fails_manifest_validation() {
        // A `needs` pointing at nothing is caught at manifest load, not at dispatch.
        let m: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            [build.x]
            backend = "cluster"
            image   = "ghcr.io/x/x"
            needs   = ["ghost"]
            [build.x.cluster]
            containerfile = "C"
        "#,
        )
        .unwrap();
        assert!(
            m.validate().is_err(),
            "undeclared need must fail validation"
        );
    }

    #[test]
    fn rejects_unknown_keys() {
        let bad = toml::from_str::<Manifest>(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "lower"
            typo_field = 3
        "#,
        );
        assert!(bad.is_err(), "deny_unknown_fields should reject typos");
    }

    /// A dangling inject must stop `deploy render`, not ride along: the missing file would only
    /// show up inside the sandbox, mid-turn, as content the agent was told to read but cannot.
    #[test]
    fn ensure_injects_resolve_rejects_a_dangling_src() {
        let dir = tempdir("inject-dangling");
        std::fs::write(dir.join("present.txt"), "x").expect("write");
        let manifest_path = dir.join("crucible.toml");
        std::fs::write(
            &manifest_path,
            r#"
[repo]
path = "."
[judge]
measure_cmd = "./measure.nu"
direction = "higher"
objective = "value"
[agent]
backend = "command"
agent_cmd = "./bump.nu"
goal = "raise it"
[[workspace.inject]]
src = "present.txt"
dst = "present.txt"
[[workspace.inject]]
src = "traces/median_block.txt"
dst = "traces/median_block.txt"
"#,
        )
        .expect("write manifest");
        let m = Manifest::load(&manifest_path).expect("manifest parses");

        let err = ensure_injects_resolve(&m, &dir).expect_err("a missing src must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("traces/median_block.txt"),
            "the error must name the missing file: {msg}"
        );
        assert!(
            !msg.contains("present.txt"),
            "a resolvable src must not be reported: {msg}"
        );

        std::fs::create_dir_all(dir.join("traces")).expect("mkdir");
        std::fs::write(dir.join("traces/median_block.txt"), "y").expect("write");
        ensure_injects_resolve(&m, &dir).expect("all srcs present now");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-manifest-freeze-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    fn init_and_commit(dir: &Path, files: &[(&str, &str)]) {
        let repo = git2::Repository::init(dir).expect("git init");
        {
            let mut cfg = repo.config().expect("config");
            cfg.set_str("user.name", "t").expect("name");
            cfg.set_str("user.email", "t@t").expect("email");
        }
        for (name, content) in files {
            std::fs::write(dir.join(name), content).expect("write tracked file");
        }
        let mut index = repo.index().expect("index");
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .expect("add");
        index.write().expect("write index");
        let tree = repo
            .find_tree(index.write_tree().expect("tree"))
            .expect("find tree");
        let sig = repo.signature().expect("sig");
        repo.commit(Some("HEAD"), &sig, &sig, "baseline", &tree, &[])
            .expect("commit");
    }

    const GOOD_MANIFEST: &str = r#"
        [repo]
        path = "."
        [workspace]
        dir = "."
        [agent]
        backend = "command"
        agent_cmd = "true"
        goal = "committed goal"
        [judge]
        measure_cmd = "m"
        direction = "higher"
    "#;

    #[test]
    fn load_frozen_prefers_base_commit_over_a_dirty_working_tree() {
        // The BYO case: `[workspace].dir = "."` puts the manifest INSIDE the git repo it
        // describes. An uncommitted edit sitting in the working tree (a leftover from an
        // interrupted turn, or tampering) must not be trusted, load_frozen must read the
        // manifest blob at HEAD instead.
        let dir = tempdir("dirty");
        init_and_commit(&dir, &[("crucible.toml", GOOD_MANIFEST)]);
        std::fs::write(
            dir.join("crucible.toml"),
            GOOD_MANIFEST.replace("committed goal", "TAMPERED goal"),
        )
        .expect("dirty the working tree");

        let m = Manifest::load_frozen(&dir.join("crucible.toml")).expect("load_frozen");
        assert_eq!(
            m.agent.goal.as_deref(),
            Some("committed goal"),
            "must read the committed blob, not the dirty working tree"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_frozen_falls_back_to_working_tree_before_any_commit_exists() {
        // First run: the workspace repo doesn't exist yet, so there's nothing to freeze
        // against. load_frozen must not fail, it hard-warns and trusts the working tree.
        let dir = tempdir("fresh");
        std::fs::write(dir.join("crucible.toml"), GOOD_MANIFEST).expect("write manifest");
        let m = Manifest::load_frozen(&dir.join("crucible.toml")).expect("load_frozen");
        assert_eq!(m.agent.goal.as_deref(), Some("committed goal"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_frozen_is_a_no_op_when_the_manifest_lives_outside_the_workspace() {
        // The default shape (every `domains/` pack): the manifest sits next to the workspace
        // dir, never inside it. load_frozen must behave exactly like plain load.
        let dir = tempdir("outside");
        let manifest = r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "committed goal"
            [judge]
            measure_cmd = "m"
            direction = "higher"
        "#;
        std::fs::write(dir.join("crucible.toml"), manifest).expect("write manifest");
        let m = Manifest::load_frozen(&dir.join("crucible.toml")).expect("load_frozen");
        assert_eq!(m.agent.goal.as_deref(), Some("committed goal"));
        assert_eq!(m.workspace.dir, "workspace");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
