//! `crucible.toml`: the portable domain manifest. Parses the contract's config (see
//! `docs/crucible-contract.md`) and builds the engine's World + Judge from it, so onboarding a
//! repo is config, not Rust. Unknown keys are rejected (typo protection).

mod broker;
mod deploy;
mod judge;
mod measure;
mod openshell;
mod relay;
mod search;
mod selftest;
mod wiring;
mod workflow;
mod world;

pub use broker::{BrokerCfg, broker_endpoint_from_url, broker_port, resolve_broker_url};
pub use deploy::DeployCfg;
pub use judge::JudgeCfg;
pub use measure::MeasureCfg;
pub use openshell::OpenshellCfg;
pub use relay::RelayFile;
pub use search::SearchCfg;
pub use selftest::SelftestCfg;
pub use workflow::{WorkflowCaps, WorkflowCfg, WorkflowType};
pub use world::WorldCfg;

use crate::command_judge::Direction;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

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
    pub judge: JudgeCfg,
    #[serde(default)]
    pub world: WorldCfg,
    /// Build/deploy targets for the deploy renderer. Optional, a config-tuning domain that never
    /// rebuilds an image leaves it unset.
    #[serde(default)]
    pub deploy: Option<DeployCfg>,
    /// Wide-round search config. Optional, most domains run pure-deep.
    #[serde(default)]
    pub search: Option<SearchCfg>,
    /// Pack-authored iteration graph. Absent means the legacy four-stage autoresearch
    /// graph; the workflow type and engine operations are capability-admitted.
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
}

/// A single-repo run's publish-on-keep config: the fork the kept commits are pushed to as a draft PR.
/// The composite analogue is `[[component]].pr_repo`; this is the single-domain manifest's version, so
/// a scoped single-repo pack (or a hand-written domain) can name its own fork instead of relying on a
/// `--pr-repo` flag. When set it takes precedence over any `--pr-repo` the caller passes (see
/// [`crate::run`]'s `run_from_manifest`).
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
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            dir: default_workspace_dir(),
            setup_cmd: None,
            inject: Vec::new(),
        }
    }
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
    // Replace the directory entry rather than copying through it. An agent can replace a
    // frozen file with a symlink; `fs::copy(src, dst)` would follow that symlink and write
    // outside the intended destination instead of restoring the frozen file.
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
            bail!("inject destination {} is a directory", dst.display());
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCfg {
    #[serde(default = "default_model")]
    pub model: String,
    /// The agent harness that runs each turn: `claude` (default) or `hermes`. Per-domain so a
    /// harness swap is a manifest edit (and an ablation axis), not engine surgery; the CLI
    /// `--harness` overrides it.
    #[serde(default)]
    pub harness: crate::harness::Harness,
    /// Hermes-harness tuning; ignored (and harmless) when `harness = "claude"`.
    #[serde(default)]
    pub hermes: HermesCfg,
    /// Reasoning-effort tier passed to Claude Code as `--effort <level>` (low|medium|high|xhigh|max).
    /// Unset = the engine default (`medium`); a `--effort` CLI flag overrides both. Set this to opt a
    /// known-hard domain up to `high`/`max`.
    #[serde(default)]
    pub reasoning_effort: Option<crate::agent::ReasoningEffort>,
    #[serde(default)]
    pub method_prompt: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub goal_file: Option<String>,
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
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default)]
    pub sandbox_image: Option<String>,
    #[serde(default)]
    pub agent_cmd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Files to materialize into the agent's sandbox before each turn (e.g. a cluster
    /// kubeconfig), each rendered host-side and targeted-uploaded to its `dest`. See
    /// [`crate::relay`].
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
    crate::harness::Harness::default()
        .default_model()
        .to_string()
}
fn default_backend() -> String {
    "local".to_string()
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

/// Cross-field checks shared by [`Manifest::validate`] and [`CompositeManifest::validate`]: the
/// broker-bin requirement plus the three sub-validators. Callers add their own shape-specific
/// checks (composite component count/uniqueness) around this.
fn validate_common(
    agent: &AgentCfg,
    judge: &JudgeCfg,
    search: &Option<SearchCfg>,
    workflow: &Option<WorkflowCfg>,
    build: &BTreeMap<String, forge::spec::BuildSpec>,
) -> Result<()> {
    if agent.broker.enabled && agent.broker.bin.is_empty() {
        bail!(
            "[agent.broker].bin is required when the broker is enabled (the domain's broker binary)"
        );
    }
    search::validate_search(search)?;
    if let Some(w) = workflow {
        w.validate()?;
    }
    selftest::validate_selftest(&judge.selftest)?;
    forge::spec::validate_builds(build)?;
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
        validate_common(
            &self.agent,
            &self.judge,
            &self.search,
            &self.workflow,
            &self.build,
        )
    }

    pub fn direction(&self) -> Result<Direction> {
        judge::parse_direction(&self.judge.direction)
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
    /// Pack-authored iteration graph. Absent means the legacy four-stage autoresearch
    /// graph; the workflow type and engine operations are capability-admitted.
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
            bail!("a composite needs at least two [[component]] entries");
        }
        let mut seen = std::collections::BTreeSet::new();
        for c in &self.components {
            if !seen.insert(c.domain.as_str()) {
                bail!("duplicate component domain `{}`", c.domain);
            }
        }
        validate_common(
            &self.agent,
            &self.judge,
            &self.search,
            &self.workflow,
            &self.build,
        )
    }

    pub fn direction(&self) -> Result<Direction> {
        judge::parse_direction(&self.judge.direction)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(m.judge.measure_cmd, "./measure.nu");
        assert_eq!(m.direction().unwrap(), Direction::Higher);
        assert_eq!(m.workspace.dir, "workspace"); // default
        assert!(m.world.snapshot_cmd.is_none(), "no [world] -> GitWorld");
        assert!(m.publish.is_none(), "no [publish] -> no PR target");
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
        assert_eq!(
            m.agent.reasoning_effort,
            Some(crate::agent::ReasoningEffort::Xhigh)
        );
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
