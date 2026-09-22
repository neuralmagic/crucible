//! The controller-engine contract version check.
//!
//! The controller is compiled against one contract version ([`CONTROLLER_CONTRACT_VERSION`]). Every
//! engine it launches work from (the deploy profile's loop image, the local engine binary) carries
//! its own, and the two match only when equal. The registry reads each dispatch target's version,
//! keeps the result for `GET /api/config`, and refuses a launch whose target does not match before
//! any row, clone, or spend.
//!
//! An agent sandbox image is NOT a dispatch target: openshell runs it as the agent container, it
//! carries no crucible engine (prod's is a third-party claude-sandbox derivative), and it can never
//! report a contract version.
//!
//! ```text
//!   startup:   profile [image].loop, engine binary -> check -> records
//!   dispatch:  admit(kind, target) -> cached record (re-read only after a read failure)
//!              mismatch    -> ContractRejection -> ledger event + park (never retried)
//!              read failed -> AdmitFailure::Unreadable -> retried on the next pass, never parked
//!   GET /api/config: snapshot of every record
//! ```
//!
//! The deploy profile is read once at process start, so a loop-image pin change is a restart and
//! the startup check is the "pin changed" check.

use crate::client::Db;
use crate::model::{ParkReason, ParkedBy};
use crate::runs::workpod::WorkKind;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use utoipa::ToSchema;

/// The contract version this controller build was compiled against.
pub const CONTROLLER_CONTRACT_VERSION: &str = crucible_contract::CONTRACT_VERSION;

/// The OCI label an engine image carries its contract version under.
pub const CONTRACT_VERSION_LABEL: &str = "io.crucible.contract-version";

/// What an engine reports when it carries no version at all.
const UNKNOWN_VERSION: &str = "unknown";

/// How long one dispatch-target version read may take before it counts as a read failure. A
/// blackholed registry would otherwise hang on the OS connect timeout, once per target.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One thing the controller launches work from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DispatchTarget {
    /// An image reference (`registry/repo:tag` or `registry/repo@sha256:…`).
    Image(String),
    /// The engine executable a local process launch spawns.
    Binary(PathBuf),
}

impl DispatchTarget {
    pub fn image(reference: impl Into<String>) -> Self {
        Self::Image(reference.into())
    }

    /// The reference string a record is keyed and reported by.
    pub fn reference(&self) -> String {
        match self {
            Self::Image(r) => r.clone(),
            Self::Binary(p) => p.display().to_string(),
        }
    }
}

impl std::fmt::Display for DispatchTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reference())
    }
}

/// A version read that could not complete: the registry, the auth path, or the binary failed.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ContractReadError(pub String);

/// The boundary a version read crosses. Production reads OCI config labels and runs the engine
/// binary; a test installs an in-memory table.
#[async_trait::async_trait]
pub trait ContractReader: Send + Sync {
    /// `Ok(None)` is a target that carries no version (no label, an engine too old to print one).
    async fn contract_version(
        &self,
        target: &DispatchTarget,
    ) -> Result<Option<String>, ContractReadError>;
}

/// Whether an engine version matches this controller: equality, no range.
pub fn versions_match(engine: Option<&str>) -> bool {
    engine == Some(CONTROLLER_CONTRACT_VERSION)
}

/// The per-target result `GET /api/config` reports.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, ToSchema)]
pub struct DispatchContract {
    pub reference: String,
    /// The version the target carries, or `unknown`.
    pub engine_version: String,
    pub controller_version: String,
    #[serde(rename = "match")]
    pub matches: bool,
    /// Why the read failed, when it did. A failed read reports `unknown` and does not match.
    pub error: Option<String>,
    pub checked_at: String,
}

impl DispatchContract {
    fn from_read(target: &DispatchTarget, read: Result<Option<String>, ContractReadError>) -> Self {
        let (engine, error) = match read {
            Ok(v) => (v, None),
            Err(e) => (None, Some(e.0)),
        };
        Self {
            reference: target.reference(),
            matches: versions_match(engine.as_deref()),
            engine_version: engine.unwrap_or_else(|| UNKNOWN_VERSION.to_string()),
            controller_version: CONTROLLER_CONTRACT_VERSION.to_string(),
            error,
            checked_at: crate::clock::now_rfc3339(),
        }
    }
}

/// The contract block of the effective-config document.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ContractDto {
    pub controller_version: String,
    pub images: Vec<DispatchContract>,
}

/// What the engine at the boundary could not serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RejectionCause {
    /// The target reported a contract version other than this controller's.
    VersionMismatch,
    /// The target's contract version matches, but the launch needs a render option that version
    /// does not define. Carries the engine's own account of what is missing.
    UnsupportedOption(String),
}

/// A launch the engine at its dispatch target cannot serve. Deterministic: the same request
/// against the same target cannot succeed, so the item parks instead of retrying.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("contract rejection: {request} against {image}: {}", self.detail())]
pub struct ContractRejection {
    pub request: RequestKind,
    pub image: String,
    pub engine_version: String,
    pub controller_version: String,
    pub cause: RejectionCause,
}

/// Why [`ContractRegistry::admit`] would not let a launch through.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum AdmitFailure {
    /// The target reported a version, and it is not this controller's.
    #[error(transparent)]
    Rejected(#[from] ContractRejection),
    /// The target's version could not be read at all — DNS, a 401, a registry 5xx, a proxy. A
    /// transport failure, not a verdict about the target: the launch retries, and nothing parks.
    #[error("contract check: reading {reference}'s contract version failed: {error}")]
    Unreadable { reference: String, error: String },
}

impl AdmitFailure {
    /// The deterministic rejection to ledger and park on. An unreadable target is not one: it
    /// rides back as an error the caller logs and retries on the next pass.
    pub fn into_rejection(self) -> anyhow::Result<ContractRejection> {
        match self {
            Self::Rejected(rejection) => Ok(rejection),
            unreadable => Err(anyhow::Error::new(unreadable)),
        }
    }
}

/// The launch kind a rejection names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestKind {
    GroundedRank,
    Scope,
    Run,
    LocalRun,
    LocalGroundedRank,
    LocalScope,
    /// `crucible fetch`: an artifact download the controller spawns the engine for.
    Fetch,
}

impl RequestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GroundedRank => "grounded-rank",
            Self::Scope => "scope",
            Self::Run => "run",
            Self::LocalRun => "local-run",
            Self::LocalGroundedRank => "local-grounded-rank",
            Self::LocalScope => "local-scope",
            Self::Fetch => "fetch",
        }
    }
}

impl From<WorkKind> for RequestKind {
    fn from(kind: WorkKind) -> Self {
        match kind {
            WorkKind::AgentTurn(crate::runs::workpod::TurnKind::GroundedRank) => Self::GroundedRank,
            WorkKind::AgentTurn(crate::runs::workpod::TurnKind::Scope) => Self::Scope,
            WorkKind::Run => Self::Run,
        }
    }
}

impl std::fmt::Display for RequestKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ContractRejection {
    /// A launch whose target's contract version is not this controller's.
    pub fn version_mismatch(
        request: RequestKind,
        image: String,
        engine_version: String,
        controller_version: String,
    ) -> Self {
        Self {
            request,
            image,
            engine_version,
            controller_version,
            cause: RejectionCause::VersionMismatch,
        }
    }

    /// A launch the engine at `image` cannot render: its contract version matches this controller
    /// (the gate already admitted it), but `option` is not in that version. `engine_version` is
    /// what the registry recorded for the target, `None` when nothing recorded it.
    pub fn unsupported_option(
        request: RequestKind,
        image: String,
        engine_version: Option<String>,
        option: String,
    ) -> Self {
        Self {
            request,
            image,
            engine_version: engine_version.unwrap_or_else(|| UNKNOWN_VERSION.to_string()),
            controller_version: CONTROLLER_CONTRACT_VERSION.to_string(),
            cause: RejectionCause::UnsupportedOption(option),
        }
    }

    fn detail(&self) -> String {
        let versions = format!(
            "engine contract {}, controller contract {}",
            self.engine_version, self.controller_version
        );
        match &self.cause {
            RejectionCause::VersionMismatch => versions,
            RejectionCause::UnsupportedOption(option) => format!("{option} ({versions})"),
        }
    }

    /// The ledger evidence: the rejection's fields under `kind = "contract rejection"`.
    pub fn evidence(&self) -> String {
        serde_json::json!({
            "kind": "contract rejection",
            "request": self.request,
            "image": self.image,
            "engine_version": self.engine_version,
            "controller_version": self.controller_version,
            "cause": self.cause,
        })
        .to_string()
    }

    pub fn park_reason(&self) -> ParkReason {
        match &self.cause {
            RejectionCause::VersionMismatch => ParkReason::ContractRejected {
                image: self.image.clone(),
                engine_version: self.engine_version.clone(),
                controller_version: self.controller_version.clone(),
            },
            RejectionCause::UnsupportedOption(option) => ParkReason::UnsupportedTurnOption {
                option: option.clone(),
            },
        }
    }
}

/// Every dispatch target's contract record, keyed by reference.
pub struct ContractRegistry {
    reader: Arc<dyn ContractReader>,
    records: RwLock<BTreeMap<String, DispatchContract>>,
}

impl ContractRegistry {
    pub fn new(reader: Arc<dyn ContractReader>) -> Self {
        Self {
            reader,
            records: RwLock::new(BTreeMap::new()),
        }
    }

    /// Read `target`'s version now, under [`READ_TIMEOUT`], and replace its record.
    pub async fn check(&self, target: &DispatchTarget) -> DispatchContract {
        let read = tokio::time::timeout(READ_TIMEOUT, self.reader.contract_version(target))
            .await
            .unwrap_or_else(|_| {
                Err(ContractReadError(format!(
                    "reading {}'s contract version timed out after {READ_TIMEOUT:?}",
                    target.reference()
                )))
            });
        let record = DispatchContract::from_read(target, read);
        if !record.matches {
            tracing::warn!(
                reference = %record.reference,
                engine = %record.engine_version,
                controller = %record.controller_version,
                error = record.error.as_deref().unwrap_or(""),
                "dispatch target contract version does not match this controller"
            );
        }
        self.records
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(record.reference.clone(), record.clone());
        record
    }

    /// Check every target in turn. Each read is bounded by [`READ_TIMEOUT`], so the whole sweep is
    /// bounded by the target count; it still must not sit on the path to binding a listener.
    pub async fn check_all(&self, targets: impl IntoIterator<Item = DispatchTarget>) {
        for target in targets {
            self.check(&target).await;
        }
    }

    /// The record for `target`: cached, unless there is none or the cached read failed.
    pub async fn record(&self, target: &DispatchTarget) -> DispatchContract {
        let cached = self
            .records
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&target.reference())
            .cloned();
        match cached {
            Some(r) if r.error.is_none() => r,
            _ => self.check(target).await,
        }
    }

    /// The launch gate: `Ok` when `target` matches this controller, [`AdmitFailure::Rejected`] when
    /// it reported another version, [`AdmitFailure::Unreadable`] when the read itself failed.
    pub async fn admit(
        &self,
        request: RequestKind,
        target: &DispatchTarget,
    ) -> Result<(), AdmitFailure> {
        let record = self.record(target).await;
        if record.matches {
            return Ok(());
        }
        if let Some(error) = record.error {
            return Err(AdmitFailure::Unreadable {
                reference: record.reference,
                error,
            });
        }
        Err(AdmitFailure::Rejected(ContractRejection::version_mismatch(
            request,
            record.reference,
            record.engine_version,
            record.controller_version,
        )))
    }

    pub fn snapshot(&self) -> ContractDto {
        ContractDto {
            controller_version: CONTROLLER_CONTRACT_VERSION.to_string(),
            images: self
                .records
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .cloned()
                .collect(),
        }
    }
}

/// The dispatch targets a controller configuration names: the deploy profile's loop image and the
/// engine binary local launches spawn. Sandbox images are agent containers, not engines, and are
/// deliberately absent.
pub fn configured_targets(
    cfg: &crate::config::ControllerCfg,
    engine_bin: PathBuf,
) -> Vec<DispatchTarget> {
    let mut targets = Vec::new();
    if let Some(profile) = &cfg.deploy_profile {
        match loop_image(profile) {
            Ok(image) => targets.push(DispatchTarget::Image(image)),
            Err(e) => tracing::warn!(
                profile = %profile.display(),
                error = format!("{e:#}"),
                "contract check: the deploy profile's loop image could not be read"
            ),
        }
    }
    targets.push(DispatchTarget::Binary(engine_bin));
    targets.sort();
    targets.dedup();
    targets
}

/// The `[image].loop` reference of the deploy profile at `path`.
pub fn loop_image(path: &Path) -> anyhow::Result<String> {
    Ok(crucible::deploy::DeployProfile::load(path)?
        .image
        .loop_image)
}

/// The loop image of the profile at `path` as a dispatch target. A profile that does not load
/// yields no target: the render that follows fails on the same parse error and is ledgered as a
/// render failure.
pub fn profile_target(path: &Path) -> Option<DispatchTarget> {
    loop_image(path).ok().map(DispatchTarget::Image)
}

/// Record a refusal on `issue_key`'s ledger and park the issue. The event carries the rejection
/// as its reason and the structured evidence; the park keeps the item off every automatic retry
/// until a human unparks it or a restart finds the target matching again.
pub async fn refuse(db: &Db, issue_key: &str, rejection: &ContractRejection) -> anyhow::Result<()> {
    let issue = crate::issues::store::get_issue(db.pool(), issue_key).await?;
    let status = issue
        .as_ref()
        .map(|i| i.status.as_str())
        .unwrap_or(crate::model::Status::New.as_str());
    db.events()
        .append(&crate::event_log::Event::now(
            issue_key,
            status,
            status,
            Some(&rejection.to_string()),
            Some(&rejection.evidence()),
        ))
        .await?;
    if let Some(issue) = issue
        && issue.status != crate::model::Status::Parked
    {
        crate::issues::transitions::park(
            db.pool(),
            db.events(),
            issue_key,
            issue.status,
            &rejection.park_reason(),
            ParkedBy::Machine,
        )
        .await?;
    }
    if let Some(m) = db.metrics() {
        m.record_turn(rejection.request.as_str(), "contract-rejected");
    }
    Ok(())
}

/// Unpark every issue a contract rejection parked whose target now matches: the pin changed and
/// this restart's check found the new image compatible.
pub async fn release_parked(db: &Db, registry: &ContractRegistry) -> anyhow::Result<usize> {
    let snapshot = registry.snapshot();
    let mut released = 0;
    for (key, reason) in crate::runs::store::contract_parked(db.pool()).await? {
        let ParkReason::ContractRejected { image, .. } = ParkReason::parse(&reason) else {
            continue;
        };
        let matches = snapshot
            .images
            .iter()
            .any(|r| r.reference == image && r.matches);
        if matches
            && crate::issues::transitions::unpark(
                db.pool(),
                db.events(),
                &key,
                Some("dispatch image contract now matches the controller"),
                None,
            )
            .await?
        {
            released += 1;
        }
    }
    Ok(released)
}

/// The production reader: an image's version is the [`CONTRACT_VERSION_LABEL`] on its OCI config,
/// read through the same authfile resolution the render's digest pinning uses; a binary's is what
/// `crucible --contract-version` prints.
pub struct LiveContractReader {
    /// The registry authfile manifest reads authenticate with. `None` falls back to the process's
    /// own docker/containers config — anonymous on a pod, which 401s on a private registry.
    authfile: Option<PathBuf>,
}

impl LiveContractReader {
    pub fn new(authfile: Option<PathBuf>) -> Self {
        Self { authfile }
    }
}

#[async_trait::async_trait]
impl ContractReader for LiveContractReader {
    async fn contract_version(
        &self,
        target: &DispatchTarget,
    ) -> Result<Option<String>, ContractReadError> {
        match target {
            DispatchTarget::Image(reference) => {
                image_label(reference, self.authfile.as_deref()).await
            }
            DispatchTarget::Binary(bin) => binary_version(bin).await,
        }
    }
}

async fn image_label(
    reference: &str,
    authfile: Option<&Path>,
) -> Result<Option<String>, ContractReadError> {
    use oci_client::client::{ClientConfig, linux_amd64_resolver};
    let parsed: oci_client::Reference = reference
        .parse()
        .map_err(|e| ContractReadError(format!("parsing image ref {reference}: {e}")))?;
    let auth = forge::oci::resolve_auth(authfile, parsed.registry()).map_err(|e| {
        ContractReadError(format!("resolving registry auth for {reference}: {e:#}"))
    })?;
    let client = oci_client::Client::new(ClientConfig {
        platform_resolver: Some(Box::new(linux_amd64_resolver)),
        connect_timeout: Some(READ_TIMEOUT),
        read_timeout: Some(READ_TIMEOUT),
        ..ClientConfig::default()
    });
    let (_manifest, _digest, config) = client
        .pull_manifest_and_config(&parsed, &auth)
        .await
        .map_err(|e| ContractReadError(format!("reading the OCI config of {reference}: {e}")))?;
    let config: serde_json::Value = serde_json::from_str(&config)
        .map_err(|e| ContractReadError(format!("decoding the OCI config of {reference}: {e}")))?;
    Ok(config
        .get("config")
        .and_then(|c| c.get("Labels"))
        .and_then(|l| l.get(CONTRACT_VERSION_LABEL))
        .and_then(|v| v.as_str())
        .map(str::to_string))
}

async fn binary_version(bin: &Path) -> Result<Option<String>, ContractReadError> {
    let output = tokio::process::Command::new(bin)
        .arg("--contract-version")
        .output()
        .await
        .map_err(|e| {
            ContractReadError(format!("running {} --contract-version: {e}", bin.display()))
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!version.is_empty()).then_some(version))
}

/// An in-memory reader for tests: a table of target reference -> read result.
#[cfg(test)]
pub(crate) struct TableReader {
    pub(crate) table: std::sync::Mutex<BTreeMap<String, Result<Option<String>, ContractReadError>>>,
    pub(crate) reads: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl TableReader {
    pub(crate) fn new(
        entries: impl IntoIterator<Item = (String, Result<Option<String>, ContractReadError>)>,
    ) -> Self {
        Self {
            table: std::sync::Mutex::new(entries.into_iter().collect()),
            reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// A reader whose every target matches this controller.
    pub(crate) fn matching_everything() -> Self {
        Self::new([])
    }

    pub(crate) fn set(&self, reference: &str, read: Result<Option<String>, ContractReadError>) {
        self.table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(reference.to_string(), read);
    }

    pub(crate) fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl ContractReader for TableReader {
    async fn contract_version(
        &self,
        target: &DispatchTarget,
    ) -> Result<Option<String>, ContractReadError> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&target.reference())
            .cloned()
            .unwrap_or(Ok(Some(CONTROLLER_CONTRACT_VERSION.to_string())))
    }
}

/// A registry every target matches: the default a test installs when contract checking is not
/// what it exercises.
#[cfg(test)]
pub(crate) fn permissive() -> Arc<ContractRegistry> {
    Arc::new(ContractRegistry::new(Arc::new(
        TableReader::matching_everything(),
    )))
}

#[cfg(test)]
mod tests {
    use crate::runs::contract::*;

    fn image(r: &str) -> DispatchTarget {
        DispatchTarget::image(r)
    }

    fn rejection(failure: AdmitFailure) -> ContractRejection {
        match failure {
            AdmitFailure::Rejected(r) => r,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn versions_match_only_on_equality() {
        assert!(versions_match(Some(CONTROLLER_CONTRACT_VERSION)));
        assert!(!versions_match(None));
        assert!(!versions_match(Some("0.0.0")));
        assert!(!versions_match(Some(&format!(
            "{CONTROLLER_CONTRACT_VERSION}-rc1"
        ))));
        assert!(!versions_match(Some(&format!(
            " {CONTROLLER_CONTRACT_VERSION}"
        ))));
    }

    #[tokio::test]
    async fn a_matching_image_is_admitted_and_reported() {
        let reader = Arc::new(TableReader::new([(
            "quay.io/x/loop:1".to_string(),
            Ok(Some(CONTROLLER_CONTRACT_VERSION.to_string())),
        )]));
        let registry = ContractRegistry::new(reader);
        let record = registry.check(&image("quay.io/x/loop:1")).await;
        assert!(record.matches);
        assert_eq!(record.engine_version, CONTROLLER_CONTRACT_VERSION);
        assert_eq!(record.controller_version, CONTROLLER_CONTRACT_VERSION);
        assert_eq!(record.error, None);
        assert!(
            registry
                .admit(RequestKind::Run, &image("quay.io/x/loop:1"))
                .await
                .is_ok()
        );
        let dto = registry.snapshot();
        assert_eq!(dto.controller_version, CONTROLLER_CONTRACT_VERSION);
        assert_eq!(dto.images, vec![record]);
    }

    #[tokio::test]
    async fn a_missing_label_is_unknown_and_rejected() {
        let reader = Arc::new(TableReader::new([(
            "quay.io/x/sandbox:1".to_string(),
            Ok(None),
        )]));
        let registry = ContractRegistry::new(reader.clone());
        let err = rejection(
            registry
                .admit(RequestKind::Scope, &image("quay.io/x/sandbox:1"))
                .await
                .expect_err("no label is a mismatch"),
        );
        assert_eq!(err.engine_version, "unknown");
        assert_eq!(err.controller_version, CONTROLLER_CONTRACT_VERSION);
        assert_eq!(err.image, "quay.io/x/sandbox:1");
        assert_eq!(
            err.to_string(),
            format!(
                "contract rejection: scope against quay.io/x/sandbox:1: engine contract unknown, controller contract {CONTROLLER_CONTRACT_VERSION}"
            )
        );
        let evidence: serde_json::Value =
            serde_json::from_str(&err.evidence()).expect("evidence is json");
        assert_eq!(evidence["kind"], "contract rejection");
        assert_eq!(evidence["request"], "scope");
        assert_eq!(evidence["image"], "quay.io/x/sandbox:1");
        assert_eq!(evidence["engine_version"], "unknown");
        assert_eq!(evidence["controller_version"], CONTROLLER_CONTRACT_VERSION);
        // A label-less image is a settled answer: one read, then the cache.
        registry
            .admit(RequestKind::Scope, &image("quay.io/x/sandbox:1"))
            .await
            .expect_err("still rejected");
        assert_eq!(reader.reads(), 1);
    }

    #[tokio::test]
    async fn a_different_version_is_rejected_with_both_versions() {
        let reader = Arc::new(TableReader::new([(
            "quay.io/x/loop:old".to_string(),
            Ok(Some("0.9.0".to_string())),
        )]));
        let registry = ContractRegistry::new(reader);
        let err = rejection(
            registry
                .admit(RequestKind::GroundedRank, &image("quay.io/x/loop:old"))
                .await
                .expect_err("mismatch"),
        );
        assert_eq!(err.engine_version, "0.9.0");
        assert_eq!(err.controller_version, CONTROLLER_CONTRACT_VERSION);
        assert_eq!(
            err.park_reason(),
            ParkReason::ContractRejected {
                image: "quay.io/x/loop:old".to_string(),
                engine_version: "0.9.0".to_string(),
                controller_version: CONTROLLER_CONTRACT_VERSION.to_string(),
            }
        );
    }

    #[tokio::test]
    async fn a_registry_error_is_a_transport_failure_retried_on_the_next_dispatch() {
        let reader = Arc::new(TableReader::new([(
            "quay.io/x/loop:1".to_string(),
            Err(ContractReadError("401 unauthorized".to_string())),
        )]));
        let registry = ContractRegistry::new(reader.clone());
        registry
            .check_all([image("quay.io/x/loop:1"), image("quay.io/x/sandbox:1")])
            .await;
        let dto = registry.snapshot();
        assert_eq!(dto.images.len(), 2);
        let loop_rec = &dto.images[0];
        assert_eq!(loop_rec.reference, "quay.io/x/loop:1");
        assert!(!loop_rec.matches);
        assert_eq!(loop_rec.engine_version, "unknown");
        assert_eq!(loop_rec.error.as_deref(), Some("401 unauthorized"));
        assert!(dto.images[1].matches, "the other image is unaffected");

        let err = registry
            .admit(RequestKind::Run, &image("quay.io/x/loop:1"))
            .await
            .expect_err("an unreadable image is refused");
        assert_eq!(
            err,
            AdmitFailure::Unreadable {
                reference: "quay.io/x/loop:1".to_string(),
                error: "401 unauthorized".to_string(),
            },
            "a read failure is transport, never a contract rejection"
        );
        err.clone()
            .into_rejection()
            .expect_err("a transport failure never parks");

        // The registry heals: the next dispatch re-reads and admits.
        reader.set(
            "quay.io/x/loop:1",
            Ok(Some(CONTROLLER_CONTRACT_VERSION.to_string())),
        );
        assert!(
            registry
                .admit(RequestKind::Run, &image("quay.io/x/loop:1"))
                .await
                .is_ok()
        );
        assert!(registry.snapshot().images[0].matches);
        assert_eq!(reader.reads(), 4);
    }

    /// A read that never returns is a read failure, not a hang: the check is bounded, so a
    /// blackholed registry cannot hold a startup sweep (or a dispatch) open indefinitely.
    #[tokio::test(start_paused = true)]
    async fn a_hanging_read_times_out_into_a_read_failure() {
        struct Hang;
        #[async_trait::async_trait]
        impl ContractReader for Hang {
            async fn contract_version(
                &self,
                _target: &DispatchTarget,
            ) -> Result<Option<String>, ContractReadError> {
                std::future::pending().await
            }
        }
        let registry = ContractRegistry::new(Arc::new(Hang));
        let record = registry.check(&image("quay.io/x/loop:1")).await;
        assert!(!record.matches);
        assert_eq!(record.engine_version, UNKNOWN_VERSION);
        assert!(
            record
                .error
                .as_deref()
                .is_some_and(|e| e.contains("timed out")),
            "{:?}",
            record.error
        );
        assert!(matches!(
            registry
                .admit(RequestKind::Run, &image("quay.io/x/loop:1"))
                .await,
            Err(AdmitFailure::Unreadable { .. })
        ));
    }

    #[tokio::test]
    async fn a_binary_target_is_keyed_by_its_path() {
        let reader = Arc::new(TableReader::new([(
            "/opt/bin/crucible".to_string(),
            Ok(Some("0.1.0".to_string())),
        )]));
        let registry = ContractRegistry::new(reader);
        let target = DispatchTarget::Binary(PathBuf::from("/opt/bin/crucible"));
        let err = rejection(
            registry
                .admit(RequestKind::LocalRun, &target)
                .await
                .expect_err("mismatch"),
        );
        assert_eq!(err.image, "/opt/bin/crucible");
        assert_eq!(err.request, RequestKind::LocalRun);
    }

    #[tokio::test]
    async fn the_live_reader_reports_the_engine_binary_version() {
        let dir = tempfile::tempdir().expect("tmp");
        let bin = dir.path().join("crucible");
        crate::testing::write_exec(
            &bin,
            "#!/bin/sh\nif [ \"$1\" = --contract-version ]; then echo 9.9.9; exit 0; fi\nexit 1\n",
        );
        let v = LiveContractReader::new(None)
            .contract_version(&DispatchTarget::Binary(bin.clone()))
            .await
            .expect("binary ran");
        assert_eq!(v.as_deref(), Some("9.9.9"));

        let silent = dir.path().join("old-crucible");
        crate::testing::write_exec(&silent, "#!/bin/sh\nexit 2\n");
        let v = LiveContractReader::new(None)
            .contract_version(&DispatchTarget::Binary(silent))
            .await
            .expect("binary ran");
        assert_eq!(v, None, "an engine that cannot print a version is unknown");

        let err = LiveContractReader::new(None)
            .contract_version(&DispatchTarget::Binary(dir.path().join("missing")))
            .await
            .expect_err("a missing binary is a read error");
        assert!(err.0.contains("--contract-version"), "{err}");
    }

    #[tokio::test]
    async fn the_live_reader_rejects_an_unparseable_reference() {
        let err = LiveContractReader::new(None)
            .contract_version(&image("not a ref"))
            .await
            .expect_err("bad ref");
        assert!(err.0.starts_with("parsing image ref"), "{err}");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_restart_releases_only_the_parks_whose_image_now_matches(
        pool: sqlx::PgPool,
    ) -> anyhow::Result<()> {
        use crate::issues::model::NewIssue;
        use crate::model::{ParkedBy, Status};
        let db = Db::new(pool);
        for key in ["owner/repo#1", "owner/repo#2", "owner/repo#3"] {
            crate::issues::store::upsert_issue(
                db.pool(),
                &NewIssue {
                    key: key.to_string(),
                    repo: "owner/repo".to_string(),
                    priority: 0,
                    evidence_url: None,
                    title: None,
                    author: None,
                    body: None,
                    labels: Vec::new(),
                    upstream_updated_at: None,
                },
            )
            .await?;
        }
        let parks = [
            (
                "owner/repo#1",
                ParkReason::ContractRejected {
                    image: "quay.io/x/loop:fixed".to_string(),
                    engine_version: "0.9.0".to_string(),
                    controller_version: CONTROLLER_CONTRACT_VERSION.to_string(),
                },
            ),
            (
                "owner/repo#2",
                ParkReason::ContractRejected {
                    image: "quay.io/x/loop:still-old".to_string(),
                    engine_version: "0.9.0".to_string(),
                    controller_version: CONTROLLER_CONTRACT_VERSION.to_string(),
                },
            ),
            ("owner/repo#3", ParkReason::UpstreamClosed),
        ];
        for (key, reason) in &parks {
            assert!(
                crate::issues::transitions::park(
                    db.pool(),
                    db.events(),
                    key,
                    Status::New,
                    reason,
                    ParkedBy::Machine,
                )
                .await?
            );
        }

        let registry = ContractRegistry::new(Arc::new(TableReader::new([
            (
                "quay.io/x/loop:fixed".to_string(),
                Ok(Some(CONTROLLER_CONTRACT_VERSION.to_string())),
            ),
            (
                "quay.io/x/loop:still-old".to_string(),
                Ok(Some("0.9.0".to_string())),
            ),
        ])));
        registry
            .check_all([
                image("quay.io/x/loop:fixed"),
                image("quay.io/x/loop:still-old"),
            ])
            .await;
        assert_eq!(release_parked(&db, &registry).await?, 1);
        for (key, expected) in [
            ("owner/repo#1", Status::New),
            ("owner/repo#2", Status::Parked),
            ("owner/repo#3", Status::Parked),
        ] {
            let issue = crate::issues::store::get_issue(db.pool(), key)
                .await?
                .expect("exists");
            assert_eq!(issue.status, expected, "{key}");
        }
        // Idempotent: a second pass finds nothing left to release.
        assert_eq!(release_parked(&db, &registry).await?, 0);
        Ok(())
    }

    #[test]
    fn configured_targets_names_the_loop_image_and_binary_but_no_sandbox() {
        let dir = tempfile::tempdir().expect("tmp");
        let profile = crate::testing::fixtures::write_deploy_profile(dir.path());
        let cfg = crate::testing::cfg_from_args([
            "ctl",
            "--deploy-profile",
            &profile.to_string_lossy(),
            "--grounded-sandbox-image",
            "quay.io/x/sandbox:g",
            "--scope-sandbox-image",
            "quay.io/x/sandbox:s",
        ]);
        let targets = configured_targets(&cfg, crate::issues::engine::resolve_bin());
        let loop_image = loop_image(&profile).expect("profile loads");
        assert!(targets.contains(&DispatchTarget::Image(loop_image)));
        assert!(
            !targets.iter().any(|t| t.reference().contains("x/sandbox")),
            "an agent sandbox image is not a dispatch target: {targets:?}"
        );
        assert!(
            targets
                .iter()
                .any(|t| matches!(t, DispatchTarget::Binary(_))),
            "the engine binary is a target"
        );
    }
}
