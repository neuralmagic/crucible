//! Declarative image builds: the controller's `building` state machinery.
//!
//! A pack that declares `[build.<name>]` blocks blocks measurement until every declared image has a
//! pinned digest. The `[build]` table is parsed by `forge::spec` (the canonical parser
//! plus `validate_builds` + `content_digest`), so the controller, the `crucible build` CLI, and the
//! manifest loader all share ONE schema — no divergence in timeouts, sub-tables, `needs`, or
//! templates by construction.
//!
//! **Backends run synchronously, off-thread.** `forge::build::dispatch_cluster` and
//! `forge::github::dispatch_github` are one-shot BLOCKING calls (dispatch + wait + resolve the
//! pushed digest, each spinning its own runtime), so a backend's [`BuildBackend::dispatch`] runs the
//! whole build on `tokio::task::spawn_blocking` — that keeps the async reconcile executor free and
//! avoids the nested-runtime panic. The daemon's reconcile worker is serial, so a build occupies one
//! `building` pass end to end; the `builds` ledger, the per-kind cap, park-on-failure, and startup
//! adoption around it are unchanged. Production is [`ForgeBuildBackend`] (installed at daemon
//! assembly when `--build-backends` is set with creds), routing on the build's `backend`; with none
//! installed the [`StubBuildBackend`] refuses dispatch with a typed [`BackendNotInstalled`] the
//! driver parks on — never a silent wedge.
//!
//! **Allowed-orgs gate.** Before any dispatch, [`check_build_allowed`] validates the target image's
//! registry org (and, for github-actions, the workflow repo's org) against the controller's
//! `allowed_orgs` whitelist — the ADR's named security gate, since a manifest is agent-influenced
//! content post-approval. A rejected build never dispatches; it parks with the reason.

#![allow(clippy::disallowed_macros)]

use crate::builds::model::{BuildBackendKind, BuildRow, BuildState, NewBuild};
use crate::client::Db;
use crate::config::ControllerCfg;
use crate::event_log::Event;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// What a dispatchable build needs: the persisted identity (the `builds` row) plus the backend
/// dispatch detail parsed from `forge::spec`. The dispatch detail is present in-memory from the plan;
/// a row reconstructed for the adopt/resolve path carries [`DispatchSpec::FromRow`] and can be
/// resolved against the registry but never re-dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRequest {
    /// The `[build.<name>]` key.
    pub(crate) name: String,
    /// The image the build pushes to (`ghcr.io/org/foo`, no tag).
    pub(crate) image: String,
    /// The tag pushed (defaulted from the context digest when the manifest declares none, so an
    /// unchanged context re-pushes the same tag idempotently).
    tag: String,
    /// The "build-needed" identity: `forge::spec::content_digest` over the declared `watch.paths` at
    /// the pack checkout. An empty watch set is `forge`'s always-rebuild sentinel.
    context_digest: String,
    backend: BuildBackendKind,
    timeout: Duration,
    /// The builds this one depends on (`needs`): reconcile won't dispatch it until every dep has a
    /// pinned digest, and `{{ builds.<dep>.digest_ref }}` in this build's templates resolves to that
    /// dep's digest.
    needs: Vec<String>,
    /// The backend-specific dispatch detail (not persisted).
    dispatch: DispatchSpec,
}

/// The backend-specific dispatch detail a build carries, mapped from `forge::spec`. Not persisted:
/// the `builds` row stores only the identity, so a row reconstructed for adoption/resolution is
/// [`DispatchSpec::FromRow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchSpec {
    /// A `[build.<name>].cluster` sub-table (the git-cloned context is supplied controller-side).
    Cluster {
        containerfile: String,
        context: String,
        platform: String,
    },
    /// A `[build.<name>].github` sub-table. `inputs` are template-expanded before dispatch;
    /// `correlation_id` is filled at dispatch prep (the workflow echoes it into the run name).
    Github {
        repo: String,
        workflow: String,
        git_ref: String,
        inputs: BTreeMap<String, String>,
        correlation_id: String,
    },
    /// Reconstructed from a persisted row (the adopt/resolve path) — no dispatch detail.
    FromRow,
}

impl BuildRequest {
    /// The `image:tag` the backend pushes to (before the registry resolves it to `image@sha256:…`).
    fn image_tag(&self) -> String {
        format!("{}:{}", self.image, self.tag)
    }
}

/// A dispatched build's progress, as the backend observed it — the build analogue of
/// [`crate::runs::workpod::TurnPhase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildProgress {
    /// Still building; poll again on the next level-triggered pass.
    Running,
    /// The image was pushed; the controller resolves + pins its digest ([`BuildBackend::resolve_digest`]).
    Succeeded,
    /// The build failed; `evidence` points at the build log (a Job/run URL or a log tail).
    Failed { evidence: Option<String> },
}

/// The cluster/CI boundary a declared build is dispatched over. Async, `dyn`, so
/// `async_trait` boxes the futures — the [`crate::runs::workpod::PodDispatcher`] discipline. Production is
/// [`ForgeBuildBackend`] (routes on the build's `backend`); with none installed the
/// [`StubBuildBackend`] refuses. A test installs a fake.
///
/// The real backend's `dispatch` runs the whole forge build synchronously (off-thread via
/// `spawn_blocking`), so `poll` sees an already-finished build and `resolve_digest` re-pins the
/// pushed tag from the registry (the single source of truth).
#[async_trait::async_trait]
pub trait BuildBackend: Send + Sync {
    /// Dispatch a build and run it to completion, returning the dispatch identity the row records +
    /// startup adoption lists against (a cluster Job name / an Actions correlation id).
    async fn dispatch(&self, namespace: &str, req: &BuildRequest) -> Result<String>;

    /// Poll a dispatched build's progress. The real backend's `dispatch` already ran to completion,
    /// so this reports `Succeeded`; a fake models an in-flight build.
    async fn poll(
        &self,
        namespace: &str,
        req: &BuildRequest,
        dispatch_id: &str,
    ) -> Result<BuildProgress>;

    /// Resolve the pushed tag to a pinned `image@sha256:…` (`forge::oci::pin_digest`). The registry
    /// is the single source of truth for both backends, so this never reads Job logs.
    async fn resolve_digest(&self, req: &BuildRequest) -> Result<String>;

    /// The dispatch identities the backend currently knows are in flight — the startup adoption
    /// LIST (never from memory). A dispatched row whose id is absent here is reconciled against the
    /// registry by adoption. The synchronous cluster backend reports none (a row still `dispatched`
    /// at restart is a crash mid-build; adoption resolves it from the registry).
    async fn adopt(&self, namespace: &str) -> Result<Vec<String>>;
}

/// A permanent dispatch refusal the reconcile driver ([`drive_scope_builds`]) fails the build on
/// immediately — parking the issue with the reason — rather than charging retry attempts against a
/// condition no re-dispatch can clear: no backend installed ([`StubBuildBackend`]), a missing
/// credential, or an allowed-orgs rejection ([`check_build_allowed`]).
#[derive(Debug)]
pub struct BackendNotInstalled(String);

impl std::fmt::Display for BackendNotInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BackendNotInstalled {}

const STUB_MSG: &str = "build backend not installed: set CONTROLLER_BUILD_BACKENDS=true with the per-backend creds \
     (push authfile / github token) so the controller installs the forge backends; until then a \
     pack must declare no [build] block";

/// The default backend when none is installed: refuses dispatch with a typed [`BackendNotInstalled`]
/// the driver parks on — never a silent wedge.
pub struct StubBuildBackend;

#[async_trait::async_trait]
impl BuildBackend for StubBuildBackend {
    async fn dispatch(&self, _ns: &str, _req: &BuildRequest) -> Result<String> {
        Err(BackendNotInstalled(STUB_MSG.to_string()).into())
    }
    async fn poll(&self, _ns: &str, _req: &BuildRequest, _id: &str) -> Result<BuildProgress> {
        bail!("{STUB_MSG}")
    }
    async fn resolve_digest(&self, _req: &BuildRequest) -> Result<String> {
        bail!("{STUB_MSG}")
    }
    async fn adopt(&self, _ns: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// The real build backend: routes a build on its `backend` to `forge::build::dispatch_cluster`
/// (a detached rootless-buildah Job) or `forge::github::dispatch_github` (`workflow_dispatch` +
/// poll). Both forge fns are one-shot BLOCKING, so [`Self::dispatch`] runs them on `spawn_blocking`.
/// Creds live here (resolved from controller config, never the manifest).
pub struct ForgeBuildBackend {
    /// The registry PUSH authfile the cluster Job's secret seeds from (and github digest resolution
    /// uses for a private dest). `None` fails a cluster dispatch with a clear reason.
    push_authfile: Option<PathBuf>,
    /// The GitHub token the github-actions backend dispatches with. `None` fails a github dispatch.
    github_token: Option<String>,
    /// The git repo a cluster build clones its context from (`git_url@git_ref`). `None` fails a
    /// cluster dispatch.
    git_url: Option<String>,
    git_ref: String,
    /// The rootless-buildah builder image + the git-clone init image the cluster Job runs.
    builder_image: String,
    git_image: String,
    /// The cluster Job's ttl-after-finished reaper window.
    ttl_seconds: i32,
    /// Optional controller-side git token FILE (mounted from a deploy secret, never the manifest).
    /// When set, a cluster build's CLONE init container authenticates with it, so a PRIVATE context
    /// repo (e.g. neuralmagic/crucible for the self-host loop) clones. `None` ⇒ anonymous clone.
    git_token_file: Option<PathBuf>,
    /// The operator's allowed-orgs whitelist, which forge re-checks when it types a github
    /// dispatch target.
    allowed_orgs: Vec<String>,
}

impl ForgeBuildBackend {
    /// Build a `forge::build::ClusterBuildRequest` from a cluster build + this backend's config.
    fn cluster_request(
        &self,
        namespace: &str,
        req: &BuildRequest,
    ) -> Result<forge::build::ClusterBuildRequest> {
        let DispatchSpec::Cluster {
            containerfile,
            context,
            platform,
        } = &req.dispatch
        else {
            bail!("build `{}` is not a cluster build", req.name);
        };
        let git_url = self.git_url.clone().ok_or_else(|| {
            BackendNotInstalled(format!(
                "cluster build `{}` needs a context git url: set CONTROLLER_BUILD_CONTEXT_GIT_URL",
                req.name
            ))
        })?;
        Ok(forge::build::ClusterBuildRequest {
            name: req.name.clone(),
            image: req.image.clone(),
            tag: req.tag.clone(),
            containerfile: containerfile.clone(),
            context: context.clone(),
            platform: platform.clone(),
            git_url,
            git_ref: self.git_ref.clone(),
            namespace: namespace.to_string(),
            correlation_id: forge::github::new_correlation_id(),
            builder_image: self.builder_image.clone(),
            git_image: self.git_image.clone(),
            ttl_seconds: self.ttl_seconds,
            timeout: req.timeout,
            git_token_file: self.git_token_file.clone(),
        })
    }

    /// Build a `forge::github::GithubBuildRequest` from a github build + this backend's config. The
    /// `inputs` are already template-expanded by [`drive_scope_builds`] before dispatch.
    fn github_request(&self, req: &BuildRequest) -> Result<forge::github::GithubBuildRequest> {
        let DispatchSpec::Github {
            repo,
            workflow,
            git_ref,
            inputs,
            correlation_id,
        } = &req.dispatch
        else {
            bail!("build `{}` is not a github build", req.name);
        };
        let correlation_id = if correlation_id.is_empty() {
            forge::github::new_correlation_id()
        } else {
            correlation_id.clone()
        };
        let repo =
            forge::github::OrgAllowlist::parse(&self.allowed_orgs.join(","))?.authorize(repo)?;
        Ok(forge::github::GithubBuildRequest {
            name: req.name.clone(),
            repo,
            workflow: workflow.clone(),
            git_ref: git_ref.clone(),
            inputs: inputs.clone(),
            correlation_id,
            correlation: forge::spec::CorrelationSource::default(),
            digest: forge::spec::DigestSource::default(),
            image_ref: req.image_tag(),
            timeout: req.timeout,
        })
    }

    /// The push authfile as a required path (cluster dispatch + private-dest digest resolution).
    fn authfile(&self) -> Result<PathBuf> {
        self.push_authfile.clone().ok_or_else(|| {
            BackendNotInstalled(
                "build push authfile not configured: set CONTROLLER_BUILD_PUSH_AUTHFILE"
                    .to_string(),
            )
            .into()
        })
    }
}

#[async_trait::async_trait]
impl BuildBackend for ForgeBuildBackend {
    async fn dispatch(&self, namespace: &str, req: &BuildRequest) -> Result<String> {
        match req.backend {
            BuildBackendKind::Cluster => {
                let cbr = self.cluster_request(namespace, req)?;
                let dispatch_id = cbr.job_name();
                let authfile = self.authfile()?;
                // The forge cluster build is blocking (spins its own runtime); off-thread it.
                tokio::task::spawn_blocking(move || {
                    forge::build::dispatch_cluster(&cbr, &authfile)
                })
                .await
                .context("joining the cluster build task")??;
                Ok(dispatch_id)
            }
            BuildBackendKind::GithubActions => {
                let gbr = self.github_request(req)?;
                let dispatch_id = gbr.correlation_id.clone();
                let token = self.github_token.clone().ok_or_else(|| {
                    BackendNotInstalled(
                        "github build token not configured: set CONTROLLER_BUILD_GITHUB_TOKEN"
                            .to_string(),
                    )
                })?;
                let authfile = self.push_authfile.clone();
                tokio::task::spawn_blocking(move || {
                    forge::github::dispatch_github(&gbr, &token, authfile.as_deref())
                })
                .await
                .context("joining the github build task")??;
                Ok(dispatch_id)
            }
        }
    }

    async fn poll(&self, _ns: &str, _req: &BuildRequest, _id: &str) -> Result<BuildProgress> {
        // `dispatch` ran the build to completion synchronously, so a dispatched row this process
        // launched is done. `resolve_digest` pins the pushed tag next.
        Ok(BuildProgress::Succeeded)
    }

    async fn resolve_digest(&self, req: &BuildRequest) -> Result<String> {
        let image_tag = req.image_tag();
        let authfile = self.push_authfile.clone();
        tokio::task::spawn_blocking(move || forge::oci::pin_digest(&image_tag, authfile.as_deref()))
            .await
            .context("joining the digest-pin task")?
    }

    async fn adopt(&self, _ns: &str) -> Result<Vec<String>> {
        // Synchronous dispatch leaves no in-flight id to list: a row still `dispatched` at startup
        // is a crash mid-build, which `adopt_builds` reconciles against the registry.
        Ok(Vec::new())
    }
}

/// Install the process-global build backend (the forge backend at daemon assembly, or a test's
/// fake). The dispatch/poll fire inside the frozen `reconcile(db, cfg, key)` step, which can't thread
/// a backend argument, so — exactly like [`crate::runs::workpod::install_dispatcher`] — it lives here as
/// the one sanctioned process-global for the build boundary.
static ACTIVE_BUILD_BACKEND: std::sync::RwLock<Option<Arc<dyn BuildBackend>>> =
    std::sync::RwLock::new(None);

/// Install the active build backend (daemon assembly, or a test's fake).
pub fn install_build_backend(backend: Arc<dyn BuildBackend>) {
    *ACTIVE_BUILD_BACKEND
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(backend);
}

/// Drop the installed build backend (a test's teardown).
pub fn reset_build_backend() {
    *ACTIVE_BUILD_BACKEND
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// The backend a dispatch resolves to: the installed one, else the [`StubBuildBackend`].
pub fn active_build_backend() -> Arc<dyn BuildBackend> {
    ACTIVE_BUILD_BACKEND
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| Arc::new(StubBuildBackend))
}

/// Assemble the real [`ForgeBuildBackend`] from controller config, or `None` when `--build-backends`
/// is off (the [`StubBuildBackend`] then parks a `[build]`-declaring pack with a clear reason).
pub fn forge_backend_from_cfg(cfg: &ControllerCfg) -> Option<ForgeBuildBackend> {
    if !cfg.build_backends {
        return None;
    }
    Some(ForgeBuildBackend {
        push_authfile: cfg.build_push_authfile.clone(),
        github_token: cfg.build_github_token.clone(),
        git_url: cfg.build_context_git_url.clone(),
        git_ref: cfg.build_context_git_ref.clone(),
        builder_image: forge::build::DEFAULT_BUILDER_IMAGE.to_string(),
        git_image: forge::build::DEFAULT_GIT_IMAGE.to_string(),
        ttl_seconds: forge::build::DEFAULT_TTL_SECONDS,
        git_token_file: cfg.build_context_git_token_file.clone(),
        allowed_orgs: cfg.allowed_orgs.clone(),
    })
}

// ---------------------------------------------------------------------------------------------
// Allowed-orgs gate — the ADR's named security check before any autonomous dispatch.
// ---------------------------------------------------------------------------------------------

/// The org segment of an image ref: the path element after the registry host. `ghcr.io/neuralmagic/x`
/// → `neuralmagic`; a hostless `library/x` → `library`. `None` when there's no org segment.
fn registry_org(image: &str) -> Option<&str> {
    let mut segs = image.split('/');
    let first = segs.next()?;
    // A leading segment that looks like a registry host (has a dot or a port, or is `localhost`) is
    // the registry; the org is the next segment. Otherwise the first segment IS the org.
    if first.contains('.') || first.contains(':') || first == "localhost" {
        segs.next().filter(|s| !s.is_empty())
    } else {
        (!first.is_empty()).then_some(first)
    }
}

/// Case-insensitive membership of `org` in the allowed-orgs whitelist. An empty whitelist denies all
/// (locked closed, matching the repo whitelist convention).
fn org_allowed(allowed_orgs: &[String], org: &str) -> bool {
    allowed_orgs
        .iter()
        .any(|o| o.trim().eq_ignore_ascii_case(org))
}

/// Validate a build against the allowed-orgs whitelist before dispatch (the security gate):
/// the target image's registry org, and — for a github build — the workflow repo's org, must both be
/// allowlisted. A rejection is a permanent [`BackendNotInstalled`] the driver parks on (a re-dispatch
/// can't clear an unallowlisted target).
pub fn check_build_allowed(req: &BuildRequest, allowed_orgs: &[String]) -> Result<()> {
    let image_org = registry_org(&req.image).ok_or_else(|| {
        BackendNotInstalled(format!(
            "build `{}` image `{}` has no org segment to check against the allowed-orgs whitelist",
            req.name, req.image
        ))
    })?;
    if !org_allowed(allowed_orgs, image_org) {
        return Err(BackendNotInstalled(format!(
            "build `{}` target org `{image_org}` (image `{}`) is not in the allowed-orgs whitelist",
            req.name, req.image
        ))
        .into());
    }
    if let DispatchSpec::Github { repo, .. } = &req.dispatch {
        let repo_org = repo.split('/').next().unwrap_or_default();
        if !org_allowed(allowed_orgs, repo_org) {
            return Err(BackendNotInstalled(format!(
                "build `{}` github workflow repo org `{repo_org}` (repo `{repo}`) is not in the \
                 allowed-orgs whitelist",
                req.name
            ))
            .into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// plan_builds — parse the pack's `[build]` table via `forge::spec` (the canonical schema).
// ---------------------------------------------------------------------------------------------

/// The pack manifest's `[build]` table, deserialized into `forge::spec::BuildSpec` (the canonical
/// schema, `deny_unknown_fields`). Every other manifest table is ignored here.
#[derive(Debug, serde::Deserialize)]
struct ManifestBuilds {
    #[serde(default)]
    build: BTreeMap<String, forge::spec::BuildSpec>,
}

/// Parse the `[build.<name>]` blocks a pack declares into dispatchable [`BuildRequest`]s via
/// `forge::spec` — one schema shared with the `crucible build` CLI. Reads the frozen pack's
/// `crucible.toml` under `pack_out`; a pack with no `[build]` table (the common case) yields an
/// empty vec → the run launches directly, exactly as before the feature. `forge::spec::validate_builds`
/// enforces per-backend sub-table presence, `needs` references, template vocabulary, and cycle-freedom.
///
/// The "build-needed" context digest is `forge::spec::content_digest` over the declared `watch.paths`
/// at the pack checkout: an empty watch set is forge's always-rebuild sentinel (a value
/// that can never equal a recorded digest). Deterministic + hermetic (file reads under `pack_out`), so
/// the caller need not `spawn_blocking` it.
pub fn plan_builds(pack_out: &Path) -> Result<Vec<BuildRequest>> {
    let manifest_path = pack_out.join("crucible.toml");
    if !manifest_path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading pack manifest {}", manifest_path.display()))?;
    let parsed: ManifestBuilds = toml::from_str(&text)
        .with_context(|| format!("parsing [build] blocks in {}", manifest_path.display()))?;
    forge::spec::validate_builds(&parsed.build)
        .with_context(|| format!("validating [build] blocks in {}", manifest_path.display()))?;

    let mut out = Vec::with_capacity(parsed.build.len());
    for (name, spec) in parsed.build {
        let backend = match spec.backend {
            forge::spec::BuildBackend::Cluster => BuildBackendKind::Cluster,
            forge::spec::BuildBackend::GithubActions => BuildBackendKind::GithubActions,
        };
        // The real build-needed predicate: hash the declared watch paths at the pack checkout.
        let context_digest = match forge::spec::content_digest(pack_out, &spec.watch.paths)
            .with_context(|| format!("hashing watch.paths for build `{name}`"))?
        {
            forge::spec::WatchDigest::Digest(d) => d,
            // Empty watch ⇒ always rebuild: a per-plan sentinel that never equals a recorded digest.
            forge::spec::WatchDigest::AlwaysRebuild => {
                format!("always-rebuild:{}", jiff::Timestamp::now().as_nanosecond())
            }
        };
        let dispatch = match (backend, &spec.cluster, &spec.github) {
            (BuildBackendKind::Cluster, Some(c), _) => DispatchSpec::Cluster {
                containerfile: c.containerfile.clone(),
                context: c.context.clone(),
                platform: c.platform.clone(),
            },
            (BuildBackendKind::GithubActions, _, Some(g)) => DispatchSpec::Github {
                repo: g.repo.clone(),
                workflow: g.workflow.clone(),
                git_ref: g.git_ref.clone(),
                inputs: g.inputs.clone(),
                correlation_id: String::new(),
            },
            // validate_builds already rejected a backend missing its sub-table.
            _ => bail!(
                "build `{name}` is missing its `{}` sub-table",
                backend.as_str()
            ),
        };
        out.push(BuildRequest {
            name,
            image: spec.image,
            tag: default_tag(&context_digest),
            context_digest,
            backend,
            timeout: spec.timeout.as_duration(),
            needs: spec.needs.clone(),
            dispatch,
        });
    }
    Ok(out)
}

/// Whether a [`plan_builds`] failure is a permanent spec/manifest error — a bad `[build]` table the
/// pack content must fix (unknown template var, `needs` cycle, missing sub-table) — rather than a
/// transient read. `plan_builds` wraps the `forge::spec::SpecError` in `.with_context`, so it sits
/// behind a context node; walk the chain rather than downcasting the root. The reconcile driver parks
/// on a `true` here instead of letting the queue retry a deterministic failure.
pub(crate) fn is_permanent_plan_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<forge::spec::SpecError>().is_some())
}

/// The tag a build pushes: `ctx-<first 12 hex of the context digest>`, so an unchanged context always
/// pushes the same tag (idempotent re-dispatch). An always-rebuild sentinel yields a
/// fresh tag each plan, which is the point (a force build never matches a recorded push).
fn default_tag(context_digest: &str) -> String {
    let hex = context_digest
        .rsplit_once(':')
        .map(|(_, h)| h)
        .unwrap_or(context_digest);
    let short: String = hex
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect();
    format!("ctx-{short}")
}

// ---------------------------------------------------------------------------------------------
// Orchestration — the plan-aware dispatch/poll driver, and startup adoption over the ledger.
// ---------------------------------------------------------------------------------------------

/// The reconcile driver's dispatch-attempt cap: once a build's failed dispatch attempts cross this,
/// it's recorded `failed` with the last error as evidence so the blocked issue parks instead of
/// re-dispatching forever. Small — a dispatch that keeps erroring is a real problem, not a blip.
const MAX_DISPATCH_ATTEMPTS: i64 = 3;

/// The aggregate progress of a scope's builds — what [`crate::issues::reconcile`] gates the run launch on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildsProgress {
    /// Every planned build succeeded with a pinned digest: the run is dispatchable.
    AllReady,
    /// At least one planned build is still in flight / undispatched (and none failed): wait,
    /// re-driven on the next level-triggered pass.
    Waiting,
    /// A build failed or timed out: the blocked issue parks with `evidence` (the build-log pointer).
    Blocked { evidence: Option<String> },
}

/// Reconcile a scope's declared builds against their ledger rows in ONE level-triggered pass
///, returning the aggregate the run launch gates on. The PLAN (`requests`, from
/// [`plan_builds`]) is authoritative — the run launches only when EVERY planned build has a
/// `succeeded` row carrying a digest, so a partial/crash-interrupted dispatch can never launch on
/// an incomplete image set.
///
/// **Phase 1 — (re)dispatch.** For every planned build with no row or a still-`pending` row lacking
/// a dispatch id (a capped or crash-interrupted dispatch), (re)dispatch it under the remaining
/// per-kind concurrency cap (`build_pod_cap` — builds are compute, the cap is the only budget, no
/// `$` ledger). The `pending` row is committed BEFORE the network dispatch, so a crash between them
/// leaves a row the next pass re-drives; the re-dispatch is idempotent via `UNIQUE(scope, name)` +
/// the `set_build_dispatched` CAS. A permanent refusal ([`BackendNotInstalled`]) fails the build at
/// once; any other dispatch error charges an attempt and fails the build past
/// [`MAX_DISPATCH_ATTEMPTS`] — either way the blocked issue parks rather than wedging.
///
/// **Phase 2 — poll + aggregate.** Poll each `dispatched` build (its own timeout cap first — a
/// wedged build must not hold a rig slot forever), pinning a digest on success or recording evidence
/// on failure, then fold the plan: `AllReady` only when every planned name is `succeeded` with a
/// digest, `Blocked` on the first failed/timed-out build, else `Waiting`.
pub(crate) async fn drive_scope_builds(
    db: &Db,
    cfg: &ControllerCfg,
    issue_key: &str,
    scope_id: i64,
    requests: &[BuildRequest],
) -> Result<BuildsProgress> {
    let backend = active_build_backend();
    let cap = i64::from(cfg.profile.build_pod_cap);

    // Phase 1: (re)dispatch every planned build missing a live row, bounded by the remaining cap.
    let existing =
        index_by_name(crate::builds::store::builds_for_scope(db.pool(), scope_id).await?);
    for req in requests {
        let row = existing.get(&req.name);
        let needs_dispatch = match row {
            None => true,
            Some(r) => r.state == BuildState::Pending && r.dispatch_id.is_none(),
        };
        if !needs_dispatch {
            continue;
        }
        // `needs` ordering: hold a build back until every dependency has pinned a
        // digest — `{{ builds.<dep>.digest_ref }}` in this build's templates resolves to it below.
        // A dep pins in an earlier pass (dispatch + resolution are one synchronous pass per build).
        if !deps_ready(req, &existing) {
            continue;
        }
        // Re-check the in-flight count per build so a re-drive fills freed slots incrementally. A
        // capped decline is NOT a failure — no attempt is charged; the issue stays `building` and
        // re-drives when a slot frees. (The whole-scope-exceeds-cap case is parked upstream in
        // reconcile before we get here, so this only ever holds back a transiently-full cap.)
        if crate::builds::store::count_builds_in_flight(db.pool()).await? >= cap {
            if let Some(m) = db.metrics() {
                m.record_build("capped");
            }
            continue;
        }
        // Commit the pending row first (crash-safe), then dispatch it.
        let id = match row {
            Some(r) => r.id,
            None => {
                crate::builds::store::insert_build(
                    db.pool(),
                    &NewBuild {
                        scope: Some(scope_id),
                        name: req.name.clone(),
                        image: req.image.clone(),
                        tag: req.tag.clone(),
                        context_digest: req.context_digest.clone(),
                        backend: req.backend,
                        timeout_secs: i64::try_from(req.timeout.as_secs()).unwrap_or(i64::MAX),
                    },
                )
                .await?
            }
        };
        // The allowed-orgs gate (the named security check) — a rejection is permanent, so the
        // issue parks with the reason rather than re-dispatching an unallowlisted target.
        if let Err(e) = check_build_allowed(req, &cfg.allowed_orgs) {
            if let Some(blocked) = fail_dispatch(db, id, issue_key, &req.name, e).await? {
                return Ok(blocked);
            }
            continue;
        }
        // Expand `{{ … }}` templates against the pinned digests of this build's deps just before
        // dispatch (cluster containerfile/context/platform, github inputs), the ADR's closed vocab.
        let dispatch_req = match prepare_dispatch(req, &scope_digests(&existing)) {
            Ok(r) => r,
            Err(e) => {
                if let Some(blocked) = fail_dispatch(db, id, issue_key, &req.name, e).await? {
                    return Ok(blocked);
                }
                continue;
            }
        };
        match backend.dispatch(&cfg.pod_namespace, &dispatch_req).await {
            Ok(dispatch_id) => {
                if crate::builds::store::set_build_dispatched(db.pool(), id, &dispatch_id).await? {
                    // Narrate the dispatch onto the issue feed (a same-state note, like the ranker's).
                    db.events()
                        .append(&Event::now(
                            issue_key,
                            "building",
                            "building",
                            Some(&format!(
                                "build `{}` dispatched on {} ({dispatch_id})",
                                req.name,
                                req.backend.as_str(),
                            )),
                            None,
                        ))
                        .await?;
                }
                if let Some(m) = db.metrics() {
                    m.record_build("dispatched");
                }
            }
            Err(e) => {
                if let Some(blocked) = fail_dispatch(db, id, issue_key, &req.name, e).await? {
                    return Ok(blocked);
                }
            }
        }
    }

    // Phase 2: poll + aggregate over the PLAN against the now-current rows.
    let rows = index_by_name(crate::builds::store::builds_for_scope(db.pool(), scope_id).await?);
    let now = jiff::Timestamp::now().as_second();
    let mut all_ready = true;
    for req in requests {
        let Some(row) = rows.get(&req.name) else {
            // No row yet (a capped dispatch this pass) — not ready; re-driven next pass.
            all_ready = false;
            continue;
        };
        match row.state {
            BuildState::Succeeded => {
                // A digest is the unblock — a Succeeded row without one is not ready (shouldn't
                // happen: set_build_succeeded always pins one, but the plan gate stays honest).
                if row.digest_ref.is_none() {
                    all_ready = false;
                }
            }
            BuildState::Failed | BuildState::TimedOut => {
                return Ok(BuildsProgress::Blocked {
                    evidence: row.evidence_url.clone(),
                });
            }
            BuildState::Pending => all_ready = false,
            BuildState::Dispatched => {
                if let Some(evidence) = timed_out_evidence(row, now) {
                    crate::builds::store::set_build_failed(
                        db.pool(),
                        row.id,
                        BuildState::TimedOut,
                        Some(&evidence),
                    )
                    .await?;
                    if let Some(m) = db.metrics() {
                        m.record_build("timed-out");
                    }
                    return Ok(BuildsProgress::Blocked {
                        evidence: Some(evidence),
                    });
                }
                let dispatch_id = row.dispatch_id.as_deref().unwrap_or_default();
                match backend
                    .poll(&cfg.pod_namespace, req, dispatch_id)
                    .await
                    .with_context(|| format!("polling build `{}`", row.name))?
                {
                    BuildProgress::Running => all_ready = false,
                    BuildProgress::Succeeded => {
                        let digest = backend.resolve_digest(req).await.with_context(|| {
                            format!("resolving digest for build `{}`", row.name)
                        })?;
                        crate::builds::store::set_build_succeeded(db.pool(), row.id, &digest)
                            .await?;
                        // Narrate the pin — the moment the build stops blocking the run.
                        db.events()
                            .append(&Event::now(
                                issue_key,
                                "building",
                                "building",
                                Some(&format!("build `{}` pinned {digest}", row.name)),
                                None,
                            ))
                            .await?;
                        if let Some(m) = db.metrics() {
                            m.record_build("succeeded");
                        }
                    }
                    BuildProgress::Failed { evidence } => {
                        crate::builds::store::set_build_failed(
                            db.pool(),
                            row.id,
                            BuildState::Failed,
                            evidence.as_deref(),
                        )
                        .await?;
                        if let Some(m) = db.metrics() {
                            m.record_build("failed");
                        }
                        return Ok(BuildsProgress::Blocked { evidence });
                    }
                }
            }
        }
    }
    if all_ready {
        Ok(BuildsProgress::AllReady)
    } else {
        Ok(BuildsProgress::Waiting)
    }
}

/// How [`fail_dispatch`] should treat a dispatch error. `Permanent` is a condition no re-dispatch can
/// clear — a malformed/non-dispatchable build spec, an RBAC/allowlist rejection, or a deterministic
/// build failure — so the build fails now with the reason and the issue parks. `Transient` is a blip
/// worth another pass (a build that timed out, a run still in flight, a 5xx / transport error), retried
/// up to [`MAX_DISPATCH_ATTEMPTS`].
enum DispatchClass {
    Permanent(String),
    Transient,
}

/// Classify a dispatch error from the forge backends (or the controller's own [`BackendNotInstalled`])
/// into retry-worthy or not. `dispatch_cluster`/`dispatch_github` and `prepare_dispatch`'s template
/// expansion hand their typed error back as the ROOT of the `anyhow::Error` (via `From`, no added
/// context), so a `downcast_ref` matches it directly. Anything unrecognized stays `Transient` — the
/// pre-typed-errors default (the attempt cap still bounds it).
fn classify_dispatch_error(err: &anyhow::Error) -> DispatchClass {
    if let Some(e) = err.downcast_ref::<BackendNotInstalled>() {
        return DispatchClass::Permanent(e.to_string());
    }
    // A bad `{{ … }}` template or spec vocabulary is deterministic — re-expanding never fixes it.
    if let Some(e) = err.downcast_ref::<forge::spec::SpecError>() {
        return DispatchClass::Permanent(e.to_string());
    }
    if let Some(e) = err.downcast_ref::<forge::github::GithubError>() {
        return classify_github_error(e);
    }
    if let Some(e) = err.downcast_ref::<forge::build::ClusterBuildError>() {
        return match e {
            // A build Job that reached a terminal FAILURE is deterministic: the Containerfile or
            // context is broken, so re-running it just burns the same compute. Park with the log tail.
            forge::build::ClusterBuildError::JobFailed { .. } => {
                DispatchClass::Permanent(e.to_string())
            }
            // A timeout may clear on a less-loaded cluster: retry, bounded by the attempt cap.
            forge::build::ClusterBuildError::JobTimedOut { .. } => DispatchClass::Transient,
        };
    }
    DispatchClass::Transient
}

/// The GitHub backend split: schema/introspection rejections and a failed run are permanent; a run
/// still in flight, a missing correlation, and transport/5xx errors are transient. Exhaustive so a
/// new `GithubError` variant forces a deliberate classification here.
fn classify_github_error(err: &forge::github::GithubError) -> DispatchClass {
    use forge::github::GithubError as G;
    match err {
        // Config the operator must fix: bad repo/workflow name, a workflow that can't be dispatched,
        // failed input validation, an unsupported digest source, or a missing/oversized/!utf8 file.
        G::MalformedRepo { .. }
        | G::MalformedAllowedOrg { .. }
        | G::EmptyOrgAllowlist { .. }
        | G::OrgNotAllowed { .. }
        | G::MalformedWorkflowFilename { .. }
        | G::UnsupportedDigestSource { .. }
        | G::InputValidation { .. }
        | G::NoTriggerBlock
        | G::NotDispatchable
        | G::InputsNotMapping
        | G::InputNameNotString
        | G::ParseYaml(_)
        | G::WorkflowNotFound { .. }
        | G::WorkflowTooLarge { .. }
        | G::WorkflowBodyOverCap { .. }
        | G::WorkflowNotUtf8(_)
        // Introspection wraps one of the above; its message names the offending field.
        | G::Introspecting { .. }
        // The dispatched run ran and failed — deterministic; park with the run URL as evidence.
        | G::RunFailed { .. } => DispatchClass::Permanent(err.to_string()),
        // A rejected dispatch is permanent on a 4xx the operator must fix (auth, 422 validation),
        // transient on a 429 rate-limit or 5xx worth another pass.
        G::DispatchRejected { status, .. } => {
            if status.is_client_error() && status.as_u16() != 429 {
                DispatchClass::Permanent(err.to_string())
            } else {
                DispatchClass::Transient
            }
        }
        // In-flight or environmental: retry.
        G::RunUnfinished { .. }
        | G::NoCorrelatedRun { .. }
        | G::Http { .. }
        | G::Runtime { .. }
        | G::Upstream { .. } => DispatchClass::Transient,
    }
}

/// Handle a dispatch error against a committed `pending` build. A [permanent][`DispatchClass`] error
/// fails the build immediately with the reason; a transient one charges an attempt and fails the build
/// once it crosses [`MAX_DISPATCH_ATTEMPTS`]. Returns `Some(Blocked)` when the build was recorded
/// `failed` (the issue parks), `None` to leave it `pending` for the next level-triggered pass to retry.
async fn fail_dispatch(
    db: &Db,
    id: i64,
    issue_key: &str,
    name: &str,
    err: anyhow::Error,
) -> Result<Option<BuildsProgress>> {
    let evidence = match classify_dispatch_error(&err) {
        DispatchClass::Permanent(reason) => reason,
        DispatchClass::Transient => {
            let attempts = crate::builds::store::bump_build_dispatch_attempt(db.pool(), id).await?;
            if attempts < MAX_DISPATCH_ATTEMPTS {
                // Transient — leave it `pending`; the next level-triggered pass retries.
                tracing::warn!(
                    %issue_key,
                    build = %name,
                    attempt = attempts,
                    error = format!("{err:#}"),
                    "build dispatch failed (retrying next pass)"
                );
                return Ok(None);
            }
            format!("build `{name}` dispatch failed {attempts}× (last error: {err:#})")
        }
    };
    crate::builds::store::set_build_failed(db.pool(), id, BuildState::Failed, Some(&evidence))
        .await?;
    if let Some(m) = db.metrics() {
        m.record_build("failed");
    }
    Ok(Some(BuildsProgress::Blocked {
        evidence: Some(evidence),
    }))
}

/// Whether every build in `req.needs` already has a `succeeded` row carrying a digest — the gate on
/// dispatching a dependent build. A dep pins in an earlier pass, so it's present in the
/// pass-start `rows` snapshot by the time this build is considered.
fn deps_ready(req: &BuildRequest, rows: &BTreeMap<String, BuildRow>) -> bool {
    req.needs.iter().all(|dep| {
        rows.get(dep)
            .is_some_and(|r| r.state == BuildState::Succeeded && r.digest_ref.is_some())
    })
}

/// The pinned digests of a scope's `succeeded` builds, keyed by build name — the `build_digests` a
/// `{{ builds.<name>.digest_ref }}` template resolves against.
fn scope_digests(rows: &BTreeMap<String, BuildRow>) -> BTreeMap<String, String> {
    rows.iter()
        .filter_map(|(name, r)| r.digest_ref.clone().map(|d| (name.clone(), d)))
        .collect()
}

/// Expand the `{{ … }}` templates in a build's dispatch spec just before dispatch, using the pinned
/// digests of its dependencies (the closed vocabulary — `forge::spec` validated it at plan
/// time). A no-op for a spec that declares no templates. `sha` isn't controller-resolved here (the
/// cluster git ref is backend config), so a `{{ sha }}` reference expands empty.
fn prepare_dispatch(
    req: &BuildRequest,
    build_digests: &BTreeMap<String, String>,
) -> Result<BuildRequest> {
    let ctx = forge::spec::TemplateContext {
        sha: String::new(),
        tag: req.tag.clone(),
        image: req.image.clone(),
        correlation_id: forge::github::new_correlation_id(),
        build_digests: build_digests.clone(),
    };
    let mut out = req.clone();
    match &mut out.dispatch {
        DispatchSpec::Cluster {
            containerfile,
            context,
            platform,
        } => {
            *containerfile = ctx.expand(containerfile)?;
            *context = ctx.expand(context)?;
            *platform = ctx.expand(platform)?;
        }
        DispatchSpec::Github {
            inputs,
            correlation_id,
            ..
        } => {
            for v in inputs.values_mut() {
                *v = ctx.expand(v)?;
            }
            *correlation_id = ctx.correlation_id.clone();
        }
        DispatchSpec::FromRow => {}
    }
    Ok(out)
}

/// A scope's build rows keyed by build name (the plan's identity — `UNIQUE(scope, name)` makes the
/// name a key within a scope). The reconcile driver looks the plan up against this.
fn index_by_name(rows: Vec<BuildRow>) -> std::collections::BTreeMap<String, BuildRow> {
    rows.into_iter().map(|r| (r.name.clone(), r)).collect()
}

/// Startup adoption by LISTING (never from memory): reconcile every `dispatched` build
/// row against the backend's live dispatch set. A row whose dispatch id the backend no longer knows
/// (its Job/run went terminal or vanished while the controller was down) is resolved — if its tag
/// now has a digest the build succeeded, else it's failed with a "vanished" note — so a build can
/// never wedge a run at `building` across a restart. Rows still live are left for the level-triggered
/// [`drive_scope_builds`] to collect. Idempotent: a re-run over already-terminal rows is a no-op.
pub async fn adopt_builds(db: &Db, cfg: &ControllerCfg) -> Result<()> {
    let backend = active_build_backend();
    let dispatched =
        crate::builds::store::builds_in_states(db.pool(), &[BuildState::Dispatched]).await?;
    if dispatched.is_empty() {
        return Ok(());
    }
    let live = backend.adopt(&cfg.pod_namespace).await?;
    for row in dispatched {
        let id = row.dispatch_id.as_deref().unwrap_or_default();
        if !id.is_empty() && live.iter().any(|l| l == id) {
            // Still in flight — drive_scope_builds re-collects it on the level-triggered pass.
            continue;
        }
        // Gone from the backend: try the registry (the single source of truth) before giving up.
        let req = row_request(&row);
        match backend.resolve_digest(&req).await {
            Ok(digest) => {
                crate::builds::store::set_build_succeeded(db.pool(), row.id, &digest).await?;
                if let Some(m) = db.metrics() {
                    m.record_build("succeeded");
                }
            }
            Err(_) => {
                let evidence = format!(
                    "build `{}` dispatch {id} vanished before it pushed",
                    row.name
                );
                crate::builds::store::set_build_failed(
                    db.pool(),
                    row.id,
                    BuildState::Failed,
                    Some(&evidence),
                )
                .await?;
                if let Some(m) = db.metrics() {
                    m.record_build("failed");
                }
            }
        }
    }
    Ok(())
}

/// The evidence string when a dispatched build has outrun its timeout cap, else `None`. `now` is
/// the current epoch second; a row with no `dispatched_at` (shouldn't happen for `dispatched`) or an
/// unparseable stamp is treated as not-timed-out (the poll retries) rather than force-failed.
fn timed_out_evidence(row: &BuildRow, now: i64) -> Option<String> {
    let started = row
        .dispatched_at
        .as_deref()?
        .parse::<jiff::Timestamp>()
        .ok()?;
    let elapsed = now - started.as_second();
    (elapsed > row.timeout_secs).then(|| {
        format!(
            "build `{}` exceeded its {}s timeout (dispatch {})",
            row.name,
            row.timeout_secs,
            row.dispatch_id.as_deref().unwrap_or("?")
        )
    })
}

/// Reconstruct the identity [`BuildRequest`] a build row was dispatched from — the resolve/adopt path
/// only needs `image`/`tag`/`backend`, so the dispatch detail is [`DispatchSpec::FromRow`] (a row can
/// be resolved against the registry but never re-dispatched from memory).
fn row_request(row: &BuildRow) -> BuildRequest {
    BuildRequest {
        name: row.name.clone(),
        image: row.image.clone(),
        tag: row.tag.clone(),
        context_digest: row.context_digest.clone(),
        backend: row.backend,
        timeout: Duration::from_secs(row.timeout_secs.max(0) as u64),
        needs: Vec::new(),
        dispatch: DispatchSpec::FromRow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builds::model::BuildState;
    use sqlx::PgPool;
    use std::sync::Mutex;

    fn db(pool: PgPool) -> Db {
        Db::new(pool)
    }

    fn cfg_with_cap(cap: u32) -> ControllerCfg {
        let mut cfg = crate::testing::cfg_from_args(["ctl"]);
        cfg.profile.build_pod_cap = cap;
        // The sample builds push to `ghcr.io/org/…`; allowlist that org so the whitelist gate admits
        // them (the deny path is covered by `check_build_allowed_*`).
        cfg.allowed_orgs = vec!["org".to_string()];
        cfg
    }

    /// A scope row to hang builds off (the FK target).
    async fn seed_scope(db: &Db) -> i64 {
        crate::issues::store::upsert_issue(
            db.pool(),
            &crate::issues::model::NewIssue {
                key: "o/r#1".into(),
                repo: "o/r".into(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: vec![],
                upstream_updated_at: None,
            },
        )
        .await
        .expect("issue");
        crate::issues::store::insert_scope(
            db.pool(),
            &crate::issues::model::NewScope {
                issue: "o/r#1".into(),
                pack_digest: Some("v1:abc".into()),
                check_outcome: Some("PASS".into()),
            },
        )
        .await
        .expect("scope")
    }

    fn sample_request(name: &str) -> BuildRequest {
        BuildRequest {
            name: name.to_string(),
            image: format!("ghcr.io/org/{name}"),
            tag: "ctx-deadbeef".to_string(),
            context_digest: "sha256:deadbeef".to_string(),
            backend: BuildBackendKind::Cluster,
            timeout: Duration::from_secs(1800),
            needs: Vec::new(),
            dispatch: DispatchSpec::Cluster {
                containerfile: "Containerfile".to_string(),
                context: ".".to_string(),
                platform: "linux/amd64".to_string(),
            },
        }
    }

    /// A controllable cluster stand-in — the build boundary's hermetic double, the analogue of the
    /// WorkPod tests' fake `PodDispatcher` (a stand-in for the cluster/registry, not an internal
    /// collaborator). `poll_reply`/`digest_reply` are canned; `live` is what `adopt` reports
    /// still in flight.
    struct FakeBackend {
        poll_reply: BuildProgress,
        digest_reply: Result<String, String>,
        live: Mutex<Vec<String>>,
    }

    impl FakeBackend {
        fn new(poll: BuildProgress, digest: Result<String, String>) -> Self {
            FakeBackend {
                poll_reply: poll,
                digest_reply: digest,
                live: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl BuildBackend for FakeBackend {
        async fn dispatch(&self, _ns: &str, req: &BuildRequest) -> Result<String> {
            let id = format!("job-{}", req.name);
            self.live.lock().expect("lock").push(id.clone());
            Ok(id)
        }
        async fn poll(&self, _ns: &str, _req: &BuildRequest, _id: &str) -> Result<BuildProgress> {
            Ok(self.poll_reply.clone())
        }
        async fn resolve_digest(&self, req: &BuildRequest) -> Result<String> {
            self.digest_reply
                .clone()
                .map(|d| format!("{}@{d}", req.image))
                .map_err(|e| anyhow::anyhow!(e))
        }
        async fn adopt(&self, _ns: &str) -> Result<Vec<String>> {
            Ok(self.live.lock().expect("lock").clone())
        }
    }

    /// A build backend that always refuses dispatch with a plain (non-typed) error — the transient
    /// failure the attempt-cap reaping counts against (distinct from the stub's permanent
    /// [`BackendNotInstalled`]). `attempts` records how many dispatches it saw.
    struct FailingDispatchBackend {
        attempts: Arc<Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl BuildBackend for FailingDispatchBackend {
        async fn dispatch(&self, _ns: &str, _req: &BuildRequest) -> Result<String> {
            *self.attempts.lock().expect("lock") += 1;
            bail!("registry auth blipped")
        }
        async fn poll(&self, _ns: &str, _req: &BuildRequest, _id: &str) -> Result<BuildProgress> {
            Ok(BuildProgress::Running)
        }
        async fn resolve_digest(&self, _req: &BuildRequest) -> Result<String> {
            bail!("no image")
        }
        async fn adopt(&self, _ns: &str) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// A backend that refuses dispatch with a TYPED permanent forge error (a non-dispatchable
    /// workflow) — distinct from the plain transient error [`FailingDispatchBackend`] returns.
    /// `attempts` records how many dispatches it saw (a permanent error should be exactly one).
    struct PermanentDispatchBackend {
        attempts: Arc<Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl BuildBackend for PermanentDispatchBackend {
        async fn dispatch(&self, _ns: &str, _req: &BuildRequest) -> Result<String> {
            *self.attempts.lock().expect("lock") += 1;
            Err(forge::github::GithubError::NotDispatchable.into())
        }
        async fn poll(&self, _ns: &str, _req: &BuildRequest, _id: &str) -> Result<BuildProgress> {
            Ok(BuildProgress::Running)
        }
        async fn resolve_digest(&self, _req: &BuildRequest) -> Result<String> {
            bail!("no image")
        }
        async fn adopt(&self, _ns: &str) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// Drive one reconcile pass over a scope's builds (the `drive_scope_builds` orchestration).
    async fn drive(
        db: &Db,
        cfg: &ControllerCfg,
        scope: i64,
        reqs: &[BuildRequest],
    ) -> Result<BuildsProgress> {
        drive_scope_builds(db, cfg, "o/r#1", scope, reqs).await
    }

    #[test]
    fn plan_builds_rejects_a_backend_without_its_subtable() {
        // `cluster` needs `[build.x.cluster]`; `forge::spec::validate_builds` rejects a backend that
        // declares no matching sub-table (one schema, shared with the CLI).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("crucible.toml"),
            "[build.x]\nbackend = \"cluster\"\nimage = \"ghcr.io/org/x\"\n",
        )
        .unwrap();
        assert!(plan_builds(dir.path()).is_err(), "cluster w/o [x.cluster]");

        // A github-actions backend without `[build.x.github]` is likewise rejected.
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(
            dir2.path().join("crucible.toml"),
            "[build.x]\nbackend = \"github-actions\"\nimage = \"ghcr.io/org/x\"\n",
        )
        .unwrap();
        assert!(plan_builds(dir2.path()).is_err(), "github w/o [x.github]");

        // A malformed timeout is caught by forge (no ad-hoc controller parse to diverge from).
        let dir3 = tempfile::tempdir().unwrap();
        std::fs::write(
            dir3.path().join("crucible.toml"),
            "[build.x]\nbackend=\"cluster\"\nimage=\"ghcr.io/org/x\"\ntimeout=\"0m\"\n[build.x.cluster]\ncontainerfile=\"Containerfile\"\n",
        )
        .unwrap();
        assert!(plan_builds(dir3.path()).is_err(), "zero timeout rejected");
    }

    #[test]
    fn plan_builds_reads_the_build_table_via_forge_spec() {
        let dir = tempfile::tempdir().unwrap();
        // A watched file so the cluster build's content_digest is a real sha256 (not always-rebuild).
        std::fs::create_dir_all(dir.path().join("domains/vllm")).unwrap();
        std::fs::write(
            dir.path().join("domains/vllm/Containerfile.sandbox"),
            "FROM scratch\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("crucible.toml"),
            r#"
[goal]
issue = "o/r#1"

[build.vllm-sandbox]
backend = "cluster"
image   = "ghcr.io/org/vllm-sandbox"
timeout = "45m"
[build.vllm-sandbox.cluster]
containerfile = "domains/vllm/Containerfile.sandbox"
platform      = "linux/arm64"
[build.vllm-sandbox.watch]
paths = ["domains/vllm/Containerfile.sandbox"]

[build.sandbox]
backend = "github-actions"
image   = "ghcr.io/org/sandbox"
needs   = ["vllm-sandbox"]
[build.sandbox.github]
repo     = "org/repo"
workflow = "crucible-build.yml"
[build.sandbox.github.inputs]
base-image = "{{ builds.vllm-sandbox.digest_ref }}"
"#,
        )
        .unwrap();
        let mut reqs = plan_builds(dir.path()).expect("plan");
        reqs.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(reqs.len(), 2);

        assert_eq!(reqs[0].name, "sandbox");
        assert_eq!(reqs[0].backend, BuildBackendKind::GithubActions);
        assert_eq!(reqs[0].needs, vec!["vllm-sandbox".to_string()]);
        assert!(reqs[0].tag.starts_with("ctx-"), "{}", reqs[0].tag);
        // An empty watch set (github build declared none) is the always-rebuild sentinel.
        assert!(reqs[0].context_digest.starts_with("always-rebuild:"));
        assert!(
            matches!(&reqs[0].dispatch, DispatchSpec::Github { repo, inputs, .. }
            if repo == "org/repo" && inputs["base-image"].contains("digest_ref"))
        );

        assert_eq!(reqs[1].name, "vllm-sandbox");
        assert_eq!(reqs[1].backend, BuildBackendKind::Cluster);
        assert_eq!(reqs[1].timeout, Duration::from_secs(45 * 60));
        // The real watch-content digest (forge::spec::content_digest), not a manifest-bytes hash.
        assert!(reqs[1].context_digest.starts_with("sha256:"));
        assert!(
            matches!(&reqs[1].dispatch, DispatchSpec::Cluster { platform, .. }
            if platform == "linux/arm64")
        );
    }

    #[test]
    fn plan_builds_is_empty_without_a_build_table_or_manifest() {
        // No manifest file at all.
        let empty = tempfile::tempdir().unwrap();
        assert!(plan_builds(empty.path()).unwrap().is_empty());
        // A manifest with no [build] table.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("crucible.toml"),
            "[goal]\nissue = \"o/r#1\"\n",
        )
        .unwrap();
        assert!(plan_builds(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn content_digest_is_stable_and_change_sensitive() {
        // The build-needed predicate: the same watched content yields the same digest across plans,
        // and editing a watched file changes it (a real rebuild signal, not a manifest-bytes hash).
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("d/Containerfile"), "FROM a\n").unwrap();
        let toml = "[build.x]\nbackend=\"cluster\"\nimage=\"ghcr.io/org/x\"\n[build.x.cluster]\ncontainerfile=\"d/Containerfile\"\n[build.x.watch]\npaths=[\"d/**\"]\n";
        std::fs::write(dir.path().join("crucible.toml"), toml).unwrap();
        let d1 = plan_builds(dir.path()).unwrap()[0].context_digest.clone();
        let d2 = plan_builds(dir.path()).unwrap()[0].context_digest.clone();
        assert_eq!(d1, d2, "stable for unchanged content");
        assert!(d1.starts_with("sha256:"));
        std::fs::write(dir.path().join("d/Containerfile"), "FROM b\n").unwrap();
        let d3 = plan_builds(dir.path()).unwrap()[0].context_digest.clone();
        assert_ne!(d1, d3, "a watched-file edit is a new context");
    }

    #[test]
    fn check_build_allowed_admits_a_listed_org_and_rejects_others() {
        let orgs = vec!["neuralmagic".to_string(), "wren".to_string()];
        let mut ok = sample_request("x");
        ok.image = "ghcr.io/neuralmagic/vllm-sandbox".to_string();
        assert!(
            check_build_allowed(&ok, &orgs).is_ok(),
            "listed org admitted"
        );
        // Case-insensitive.
        let mut ci = sample_request("x");
        ci.image = "ghcr.io/NeuralMagic/x".to_string();
        assert!(check_build_allowed(&ci, &orgs).is_ok());
        // An off-list image org is rejected (permanently — a BackendNotInstalled the driver parks on).
        let mut deny = sample_request("x");
        deny.image = "ghcr.io/attacker/x".to_string();
        let err = check_build_allowed(&deny, &orgs).unwrap_err();
        assert!(err.downcast_ref::<BackendNotInstalled>().is_some());
        assert!(format!("{err:#}").contains("allowed-orgs"), "{err:#}");
        // An empty whitelist denies all (locked closed).
        assert!(check_build_allowed(&ok, &[]).is_err());
    }

    #[test]
    fn check_build_allowed_also_gates_the_github_workflow_repo_org() {
        let orgs = vec!["neuralmagic".to_string()];
        // The image org is allowed, but the github workflow repo's org is NOT — still rejected.
        let req = BuildRequest {
            name: "sandbox".to_string(),
            image: "ghcr.io/neuralmagic/sandbox".to_string(),
            tag: "ctx-x".to_string(),
            context_digest: "sha256:x".to_string(),
            backend: BuildBackendKind::GithubActions,
            timeout: Duration::from_secs(60),
            needs: Vec::new(),
            dispatch: DispatchSpec::Github {
                repo: "attacker/ci".to_string(),
                workflow: "build.yml".to_string(),
                git_ref: "main".to_string(),
                inputs: BTreeMap::new(),
                correlation_id: String::new(),
            },
        };
        let err = check_build_allowed(&req, &orgs).unwrap_err();
        assert!(format!("{err:#}").contains("repo org"), "{err:#}");
    }

    #[test]
    fn forge_backend_routes_cluster_and_github_requests() {
        // Routing selection (cluster vs github) without a cluster: the pure request-mappers pick the
        // right forge request type per DispatchSpec.
        let backend = ForgeBuildBackend {
            push_authfile: Some(PathBuf::from("/tmp/auth.json")),
            github_token: Some("t".to_string()),
            git_url: Some("https://github.com/org/repo".to_string()),
            git_ref: "main".to_string(),
            builder_image: "buildah".to_string(),
            git_image: "git".to_string(),
            ttl_seconds: 900,
            git_token_file: Some(PathBuf::from("/var/run/secrets/build-git/token")),
            allowed_orgs: vec!["org".to_string()],
        };
        let cluster = sample_request("vllm-sandbox");
        let cbr = backend
            .cluster_request("ns", &cluster)
            .expect("cluster req");
        assert_eq!(cbr.image, "ghcr.io/org/vllm-sandbox");
        assert_eq!(cbr.git_url, "https://github.com/org/repo");
        assert_eq!(cbr.containerfile, "Containerfile");
        assert_eq!(cbr.namespace, "ns");
        // The controller-side clone token file threads into the cluster request (never the manifest).
        assert_eq!(
            cbr.git_token_file.as_deref(),
            Some(PathBuf::from("/var/run/secrets/build-git/token").as_path()),
        );
        // A github request routes to the github mapper.
        let mut gh = sample_request("sandbox");
        gh.backend = BuildBackendKind::GithubActions;
        gh.dispatch = DispatchSpec::Github {
            repo: "org/repo".to_string(),
            workflow: "build.yml".to_string(),
            git_ref: "main".to_string(),
            inputs: BTreeMap::from([("k".to_string(), "v".to_string())]),
            correlation_id: "corr-123".to_string(),
        };
        let gbr = backend.github_request(&gh).expect("github req");
        assert_eq!(gbr.repo.to_string(), "org/repo");
        assert_eq!(
            gbr.correlation_id, "corr-123",
            "the prepared correlation is used"
        );
        assert_eq!(gbr.image_ref, "ghcr.io/org/sandbox:ctx-deadbeef");
        // A mismatched mapper call errors (a cluster req can't produce a github request).
        assert!(backend.github_request(&cluster).is_err());
    }

    #[test]
    fn prepare_dispatch_expands_a_dep_digest_into_github_inputs() {
        let mut req = BuildRequest {
            name: "sandbox".to_string(),
            image: "ghcr.io/org/sandbox".to_string(),
            tag: "ctx-x".to_string(),
            context_digest: "sha256:x".to_string(),
            backend: BuildBackendKind::GithubActions,
            timeout: Duration::from_secs(60),
            needs: vec!["base".to_string()],
            dispatch: DispatchSpec::Github {
                repo: "org/repo".to_string(),
                workflow: "build.yml".to_string(),
                git_ref: "main".to_string(),
                inputs: BTreeMap::from([(
                    "base-image".to_string(),
                    "{{ builds.base.digest_ref }}".to_string(),
                )]),
                correlation_id: String::new(),
            },
        };
        let digests = BTreeMap::from([(
            "base".to_string(),
            "ghcr.io/org/base@sha256:beef".to_string(),
        )]);
        req = prepare_dispatch(&req, &digests).expect("expand");
        let DispatchSpec::Github {
            inputs,
            correlation_id,
            ..
        } = &req.dispatch
        else {
            panic!("still github");
        };
        assert_eq!(inputs["base-image"], "ghcr.io/org/base@sha256:beef");
        assert!(!correlation_id.is_empty(), "a correlation id was minted");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn drive_records_pending_then_dispatched_rows(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Running,
            Ok("sha256:x".into()),
        )));

        let out = drive(
            &db,
            &cfg_with_cap(4),
            scope,
            &[sample_request("a"), sample_request("b")],
        )
        .await?;
        reset_build_backend();

        // Both dispatched, still building → Waiting (never AllReady on a fresh dispatch).
        assert_eq!(out, BuildsProgress::Waiting);
        let rows = crate::builds::store::builds_for_scope(db.pool(), scope).await?;
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(
                r.state,
                BuildState::Dispatched,
                "every build was dispatched"
            );
            assert!(r.dispatch_id.is_some(), "the dispatch id was recorded");
            assert!(r.digest_ref.is_none(), "no digest until it succeeds");
        }
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn drive_partial_dispatch_under_cap_never_reports_ready(pool: PgPool) -> Result<()> {
        // BLOCKER 2: a scope whose builds only partially dispatch (the cap admits fewer than the
        // plan needs) must NEVER read AllReady — the run can't launch on an incomplete image set.
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        // Cap 1, plan of 2: `a` dispatches (and even succeeds), `b` is capped out with no row.
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Succeeded,
            Ok("sha256:beef".into()),
        )));
        let out = drive(
            &db,
            &cfg_with_cap(1),
            scope,
            &[sample_request("a"), sample_request("b")],
        )
        .await?;
        reset_build_backend();

        assert_eq!(
            out,
            BuildsProgress::Waiting,
            "one build pinned but the other never dispatched → still waiting, not ready"
        );
        let rows = crate::builds::store::builds_for_scope(db.pool(), scope).await?;
        assert_eq!(rows.len(), 1, "only the admitted build has a row");
        assert_eq!(rows[0].name, "a");
        assert_eq!(rows[0].state, BuildState::Succeeded);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn drive_blocks_the_run_until_every_build_has_a_digest(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        let cfg = cfg_with_cap(4);

        // First pass: dispatch + poll still-running → Waiting (the run is blocked, no digest).
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Running,
            Ok("sha256:beef".into()),
        )));
        assert_eq!(
            drive(&db, &cfg, scope, &[sample_request("a")]).await?,
            BuildsProgress::Waiting
        );
        assert!(
            crate::builds::store::builds_for_scope(db.pool(), scope).await?[0]
                .digest_ref
                .is_none(),
            "still blocked: no digest pinned"
        );
        reset_build_backend();

        // Second pass: the backend reports success → the digest pins and the run unblocks. The
        // already-dispatched row is NOT re-dispatched (needs_dispatch is false).
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Succeeded,
            Ok("sha256:beef".into()),
        )));
        assert_eq!(
            drive(&db, &cfg, scope, &[sample_request("a")]).await?,
            BuildsProgress::AllReady
        );
        reset_build_backend();
        let rows = crate::builds::store::builds_for_scope(db.pool(), scope).await?;
        assert_eq!(rows.len(), 1, "no duplicate row from the second pass");
        assert_eq!(rows[0].state, BuildState::Succeeded);
        assert_eq!(
            rows[0].digest_ref.as_deref(),
            Some("ghcr.io/org/a@sha256:beef"),
            "the resolved digest is pinned onto the row"
        );

        // The feed narrates both edges: one dispatch note (only the first pass dispatched) and one
        // pin note (the second pass), no duplicates across the two re-drives.
        let notes: Vec<String> = db
            .events()
            .read_for_key("o/r#1")
            .await?
            .into_iter()
            .filter_map(|e| e.reason)
            .collect();
        assert_eq!(
            notes.iter().filter(|n| n.contains("dispatched on")).count(),
            1,
            "exactly one dispatch note: {notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("build `a` pinned ghcr.io/org/a@sha256:beef")),
            "a pin note carrying the digest: {notes:?}"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn drive_blocks_with_evidence_on_a_failed_build(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        let cfg = cfg_with_cap(4);
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Failed {
                evidence: Some("https://logs/build/42".into()),
            },
            Err("no image".into()),
        )));
        let progress = drive(&db, &cfg, scope, &[sample_request("a")]).await?;
        reset_build_backend();
        assert_eq!(
            progress,
            BuildsProgress::Blocked {
                evidence: Some("https://logs/build/42".into())
            }
        );
        let row = &crate::builds::store::builds_for_scope(db.pool(), scope).await?[0];
        assert_eq!(row.state, BuildState::Failed);
        assert_eq!(row.evidence_url.as_deref(), Some("https://logs/build/42"));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn drive_times_out_a_wedged_build_from_its_own_timeout(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        let cfg = cfg_with_cap(4);
        let mut req = sample_request("a");
        req.timeout = Duration::from_secs(60);
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Running, // the backend would say "still going" — the cap fires anyway
            Ok("sha256:x".into()),
        )));
        drive(&db, &cfg, scope, std::slice::from_ref(&req)).await?;
        // Back-date the dispatch stamp well past the 60s timeout.
        let id = crate::builds::store::builds_for_scope(db.pool(), scope).await?[0].id;
        sqlx::query!(
            "UPDATE builds SET dispatched_at = '2000-01-01T00:00:00Z' WHERE id = $1",
            id
        )
        .execute(db.pool())
        .await?;

        let progress = drive(&db, &cfg, scope, std::slice::from_ref(&req)).await?;
        reset_build_backend();
        assert!(
            matches!(progress, BuildsProgress::Blocked { evidence: Some(e) } if e.contains("timeout"))
        );
        assert_eq!(
            crate::builds::store::builds_for_scope(db.pool(), scope).await?[0].state,
            BuildState::TimedOut
        );
        Ok(())
    }

    /// BLOCKER 1, the exact repro both reviewers built: a dispatch error must NOT wedge the issue and
    /// must NOT poison the concurrency cap. A pending row is committed before the network dispatch;
    /// when the dispatch errors the next pass RE-DRIVES it (never leaves it stuck), and once the
    /// backend recovers the build dispatches + pins for real.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn drive_redrives_a_pending_row_after_a_dispatch_error(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        let cfg = cfg_with_cap(2);

        // Pass 1: dispatch errors (transient). The pending row survives; the pass reports Waiting,
        // NOT a wedge — and the row still counts as exactly one in-flight slot, not a leak.
        let attempts = Arc::new(Mutex::new(0u32));
        install_build_backend(Arc::new(FailingDispatchBackend {
            attempts: attempts.clone(),
        }));
        assert_eq!(
            drive(&db, &cfg, scope, &[sample_request("a")]).await?,
            BuildsProgress::Waiting,
            "a failed dispatch does not wedge — it re-drives"
        );
        let rows = crate::builds::store::builds_for_scope(db.pool(), scope).await?;
        assert_eq!(
            rows.len(),
            1,
            "the pending row was committed before dispatch"
        );
        assert_eq!(rows[0].state, BuildState::Pending);
        assert!(rows[0].dispatch_id.is_none());
        assert_eq!(rows[0].dispatch_attempts, 1, "one failed attempt charged");
        assert_eq!(
            crate::builds::store::count_builds_in_flight(db.pool()).await?,
            1,
            "the pending row counts once — no cap poison"
        );
        reset_build_backend();

        // Pass 2: the backend recovers. The SAME pending row is re-dispatched (idempotent — no second
        // row forks), and the build pins. Crucially: no wedge across the error.
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Succeeded,
            Ok("sha256:beef".into()),
        )));
        assert_eq!(
            drive(&db, &cfg, scope, &[sample_request("a")]).await?,
            BuildsProgress::AllReady
        );
        reset_build_backend();
        let rows = crate::builds::store::builds_for_scope(db.pool(), scope).await?;
        assert_eq!(rows.len(), 1, "re-dispatch reused the pending row, no fork");
        assert_eq!(rows[0].state, BuildState::Succeeded);
        assert!(rows[0].digest_ref.is_some());
        Ok(())
    }

    /// A dispatch that keeps failing must not spin forever: after [`MAX_DISPATCH_ATTEMPTS`] the build
    /// is failed with the last error as evidence, so the issue parks (Blocked) — the pending-row
    /// reaping the timeout cap can't do (no dispatched_at clock).
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn drive_parks_after_the_dispatch_attempt_cap(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        let cfg = cfg_with_cap(2);
        let attempts = Arc::new(Mutex::new(0u32));
        install_build_backend(Arc::new(FailingDispatchBackend {
            attempts: attempts.clone(),
        }));

        let mut last = BuildsProgress::Waiting;
        for _ in 0..MAX_DISPATCH_ATTEMPTS {
            last = drive(&db, &cfg, scope, &[sample_request("a")]).await?;
        }
        reset_build_backend();

        assert!(
            matches!(last, BuildsProgress::Blocked { evidence: Some(ref e) } if e.contains("dispatch failed")),
            "the attempt cap fails the build with the error as evidence: {last:?}"
        );
        assert_eq!(
            crate::builds::store::builds_for_scope(db.pool(), scope).await?[0].state,
            BuildState::Failed
        );
        assert_eq!(
            *attempts.lock().expect("lock"),
            MAX_DISPATCH_ATTEMPTS as u32
        );
        Ok(())
    }

    /// With no real backend installed, the M1 stub dispatch returns a TYPED [`BackendNotInstalled`]
    /// the driver turns into an immediate Blocked (a machine park upstream) — NEVER a wedge, and the
    /// reason carries the "not installed" pointer. This is deterministic for the first `[build]` pass.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn drive_with_the_stub_backend_parks_not_wedges(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        // No install_build_backend → the ClusterBuildBackend stub is active.
        let progress = drive(&db, &cfg_with_cap(2), scope, &[sample_request("a")]).await?;
        assert!(
            matches!(progress, BuildsProgress::Blocked { evidence: Some(ref e) } if e.contains("not installed")),
            "the stub parks with a clear reason: {progress:?}"
        );
        let row = &crate::builds::store::builds_for_scope(db.pool(), scope).await?[0];
        assert_eq!(
            row.state,
            BuildState::Failed,
            "failed at once, not left pending"
        );
        assert_eq!(
            row.dispatch_attempts, 0,
            "a permanent refusal charges no retry attempts"
        );
        Ok(())
    }

    /// A permanent TYPED dispatch error (a non-dispatchable workflow) parks on the FIRST pass with the
    /// forge message as evidence — no retry attempts burned, unlike the plain transient error the cap
    /// counts down. This is the whole point of classifying: a config bug fails fast, not 3× later.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn drive_parks_immediately_on_a_permanent_typed_dispatch_error(
        pool: PgPool,
    ) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        let attempts = Arc::new(Mutex::new(0u32));
        install_build_backend(Arc::new(PermanentDispatchBackend {
            attempts: attempts.clone(),
        }));

        let progress = drive(&db, &cfg_with_cap(2), scope, &[sample_request("a")]).await?;
        reset_build_backend();

        assert!(
            matches!(progress, BuildsProgress::Blocked { evidence: Some(ref e) } if e.contains("isn't dispatchable")),
            "a permanent typed error parks at once with the forge reason: {progress:?}"
        );
        let row = &crate::builds::store::builds_for_scope(db.pool(), scope).await?[0];
        assert_eq!(
            row.state,
            BuildState::Failed,
            "failed at once, not left pending"
        );
        assert_eq!(
            row.dispatch_attempts, 0,
            "a permanent error charges no retry attempts"
        );
        assert_eq!(
            *attempts.lock().expect("lock"),
            1,
            "dispatched exactly once — no 3× retry of a deterministic refusal"
        );
        Ok(())
    }

    /// The pure classification split: controller-local, spec, cluster, and github errors each land on
    /// the right side of permanent/transient. Guards the exact routing `fail_dispatch` relies on.
    #[test]
    fn classify_dispatch_error_splits_permanent_from_transient() {
        use forge::build::ClusterBuildError as C;
        use forge::github::GithubError as G;
        use forge::spec::SpecError as S;

        let permanent =
            |e: anyhow::Error| matches!(classify_dispatch_error(&e), DispatchClass::Permanent(_));
        let transient =
            |e: anyhow::Error| matches!(classify_dispatch_error(&e), DispatchClass::Transient);

        // Controller-local refusal + any spec/template error: always permanent.
        assert!(permanent(BackendNotInstalled("x".into()).into()));
        assert!(permanent(
            S::UnknownTemplateVar { var: "foo".into() }.into()
        ));

        // Cluster: a real build failure parks, a timeout retries.
        assert!(permanent(
            C::JobFailed {
                job: "j".into(),
                namespace: "n".into(),
                log: "boom".into(),
            }
            .into()
        ));
        assert!(transient(
            C::JobTimedOut {
                job: "j".into(),
                seconds: 600,
                namespace: "n".into(),
                log: String::new(),
            }
            .into()
        ));

        // GitHub permanent: malformed spec, non-dispatchable workflow, failed validation, failed run.
        assert!(permanent(G::MalformedRepo { repo: "bad".into() }.into()));
        assert!(permanent(G::NotDispatchable.into()));
        assert!(permanent(
            G::InputValidation {
                repo: "o/r".into(),
                workflow: "b.yml".into(),
                git_ref: "main".into(),
                errors: vec!["missing input".into()],
            }
            .into()
        ));
        assert!(permanent(
            G::RunFailed {
                build: "b".into(),
                run_id: 7,
                conclusion: "failure".into(),
                html_url: "http://x".into(),
            }
            .into()
        ));
        // A dispatch rejection splits on status: 4xx the operator must fix vs a 429/5xx retry.
        assert!(permanent(
            G::DispatchRejected {
                url: "http://x".into(),
                status: reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                body: String::new(),
            }
            .into()
        ));
        assert!(transient(
            G::DispatchRejected {
                url: "http://x".into(),
                status: reqwest::StatusCode::BAD_GATEWAY,
                body: String::new(),
            }
            .into()
        ));
        assert!(transient(
            G::DispatchRejected {
                url: "http://x".into(),
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                body: String::new(),
            }
            .into()
        ));

        // GitHub transient: run still in flight / correlation not observed yet.
        assert!(transient(
            G::RunUnfinished {
                build: "b".into(),
                run_id: 7,
                timeout: Duration::from_secs(60),
                status: Some("in_progress".into()),
                html_url: "http://x".into(),
            }
            .into()
        ));
        assert!(transient(
            G::NoCorrelatedRun {
                correlation_id: "cid".into(),
                window: Duration::from_secs(60),
                correlation: forge::spec::CorrelationSource::default(),
            }
            .into()
        ));

        // An unrecognized plain error stays transient — the pre-typed default.
        assert!(transient(anyhow::anyhow!("registry auth blipped")));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn adopt_is_idempotent_and_reconciles_a_vanished_build(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        let cfg = cfg_with_cap(4);

        // Dispatch two builds; the fake reports both live.
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Running,
            Ok("sha256:found".into()),
        )));
        drive(
            &db,
            &cfg,
            scope,
            &[sample_request("a"), sample_request("b")],
        )
        .await?;

        // First adoption: both are live → no state change (level-triggered poll will collect them).
        adopt_builds(&db, &cfg).await?;
        for r in crate::builds::store::builds_for_scope(db.pool(), scope).await? {
            assert_eq!(
                r.state,
                BuildState::Dispatched,
                "a live build is left alone"
            );
        }
        reset_build_backend();

        // A backend that lists NOTHING live but whose registry HAS the digest → the vanished builds
        // are adopted as succeeded (the registry is the source of truth). Idempotent: re-running
        // adoption over the now-terminal rows changes nothing.
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Running,
            Ok("sha256:found".into()),
        )));
        adopt_builds(&db, &cfg).await?;
        adopt_builds(&db, &cfg).await?;
        reset_build_backend();
        for r in crate::builds::store::builds_for_scope(db.pool(), scope).await? {
            assert_eq!(
                r.state,
                BuildState::Succeeded,
                "a vanished-but-pushed build is adopted from the registry"
            );
            assert!(r.digest_ref.is_some());
        }
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn adopt_fails_a_vanished_build_with_no_pushed_image(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        let cfg = cfg_with_cap(4);
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Running,
            Ok("sha256:x".into()),
        )));
        drive(&db, &cfg, scope, &[sample_request("a")]).await?;
        reset_build_backend();

        // Not live AND the registry has no digest → the build vanished; adopt fails it with evidence.
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Running,
            Err("manifest unknown".into()),
        )));
        adopt_builds(&db, &cfg).await?;
        reset_build_backend();
        let row = &crate::builds::store::builds_for_scope(db.pool(), scope).await?[0];
        assert_eq!(row.state, BuildState::Failed);
        assert!(row.evidence_url.as_deref().unwrap().contains("vanished"));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn stub_backend_refuses_dispatch_with_a_typed_error(pool: PgPool) -> Result<()> {
        // The default backend (no install) is the stub: it refuses to submit with the TYPED
        // not-installed error the driver parks on.
        let _g = crate::ENV_LOCK.lock().await;
        let _ = pool; // the sqlx harness supplies a pool; this test only needs the serialized lock.
        let err = active_build_backend()
            .dispatch("ns", &sample_request("a"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not installed"), "{err:#}");
        assert!(
            err.downcast_ref::<BackendNotInstalled>().is_some(),
            "the stub refusal is the typed BackendNotInstalled"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn drive_holds_a_build_back_until_its_needs_pin(pool: PgPool) -> Result<()> {
        // `needs` ordering: `dependent` (needs `base`) must not dispatch until `base` has a digest.
        let _g = crate::ENV_LOCK.lock().await;
        let db = db(pool);
        let scope = seed_scope(&db).await;
        let cfg = cfg_with_cap(4);
        let mut dependent = sample_request("dependent");
        dependent.needs = vec!["base".to_string()];

        // Pass 1: `base` dispatches + succeeds; `dependent` is held back (no row yet).
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Succeeded,
            Ok("sha256:base".into()),
        )));
        let out = drive(
            &db,
            &cfg,
            scope,
            &[sample_request("base"), dependent.clone()],
        )
        .await?;
        reset_build_backend();
        assert_eq!(out, BuildsProgress::Waiting, "dependent waits on base");
        let rows = crate::builds::store::builds_for_scope(db.pool(), scope).await?;
        assert_eq!(rows.len(), 1, "only base has a row this pass");
        assert_eq!(rows[0].name, "base");
        assert_eq!(rows[0].state, BuildState::Succeeded);

        // Pass 2: base is pinned, so dependent now dispatches + succeeds → the plan is ready.
        install_build_backend(Arc::new(FakeBackend::new(
            BuildProgress::Succeeded,
            Ok("sha256:dep".into()),
        )));
        let out = drive(&db, &cfg, scope, &[sample_request("base"), dependent]).await?;
        reset_build_backend();
        assert_eq!(out, BuildsProgress::AllReady);
        assert_eq!(
            crate::builds::store::builds_for_scope(db.pool(), scope)
                .await?
                .len(),
            2
        );
        Ok(())
    }
}
