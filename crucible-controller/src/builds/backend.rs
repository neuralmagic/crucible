//! The boundary a declared build is dispatched over, and the request it carries.

#![allow(clippy::disallowed_macros)]

use crate::builds::model::BuildBackendKind;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
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
    pub(in crate::builds) tag: String,
    /// The "build-needed" identity: `forge::spec::content_digest` over the declared `watch.paths` at
    /// the pack checkout. An empty watch set is `forge`'s always-rebuild sentinel.
    pub(in crate::builds) context_digest: String,
    pub(in crate::builds) backend: BuildBackendKind,
    pub(in crate::builds) timeout: Duration,
    /// The builds this one depends on (`needs`): reconcile won't dispatch it until every dep has a
    /// pinned digest, and `{{ builds.<dep>.digest_ref }}` in this build's templates resolves to that
    /// dep's digest.
    pub(in crate::builds) needs: Vec<String>,
    /// The backend-specific dispatch detail (not persisted).
    pub(in crate::builds) dispatch: DispatchSpec,
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
    pub(in crate::builds) fn image_tag(&self) -> String {
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
pub struct BackendNotInstalled(pub(in crate::builds) String);

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
