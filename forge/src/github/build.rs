//! The GithubActions build backend: dispatch a `workflow_dispatch` build workflow and wait for
//! its run. A backend's only job is "build + push to the tag it was given"; crucible resolves the
//! digest itself from the registry via [`crate::oci::pin_digest`], so a conforming workflow
//! reports nothing back.
//!
//! ```text
//!   preflight: workflow file at ref ─▶ inputs ─▶ validate the manifest's mapping
//!   POST .../dispatches ─▶ run id ─▶ GET .../runs/{id} until completed
//!        ├─ success ─▶ Ok(run)       (the caller pins the digest)
//!        └─ else    ─▶ RunFailed     (run url + conclusion)
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::github::workflow::{InputType, Workflow, WorkflowInput};
use crate::github::{Client, DispatchedRun, GithubError, Repo, RunConclusion, RunStatus, Token};
use crate::spec::{DigestKind, DigestSource};

pub const ALLOWED_ORGS_ENV: &str = "FORGE_GITHUB_ALLOWED_ORGS";

const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Why a build dispatch was refused or its run did not pass.
#[derive(Debug, thiserror::Error)]
pub enum BuildFailure {
    #[error(
        "${ALLOWED_ORGS_ENV} entry {entry:?} is not a github owner \
         ([A-Za-z0-9-], no \"/\")"
    )]
    MalformedAllowedOrg { entry: String },
    #[error(
        "github dispatch target {repo:?} refused: the operator org allowlist \
         ${ALLOWED_ORGS_ENV} is unset or empty, so no dispatch target is permitted; set it to \
         the comma-separated owners this engine may dispatch to"
    )]
    EmptyOrgAllowlist { repo: String },
    #[error(
        "github dispatch target {repo:?} refused: owner {owner:?} is outside the operator org \
         allowlist ${ALLOWED_ORGS_ENV} = [{allowed}]"
    )]
    OrgNotAllowed {
        repo: String,
        owner: String,
        allowed: String,
    },
    #[error(
        "github workflow {workflow:?} must be a workflow filename like \"crucible-build.yml\" \
         ([A-Za-z0-9._-]+.ya?ml, no path separators or \"..\")"
    )]
    MalformedWorkflowFilename { workflow: String },
    #[error(
        "build {build:?}: digest source `artifact` (name={artifact:?}, path={path:?}) is not \
         implemented; the registry-tag path is the default and the single source of \
         truth; declare `digest = \"registry-tag\"` or leave [outputs] unset"
    )]
    UnsupportedDigestSource {
        build: String,
        artifact: String,
        path: String,
    },
    #[error(
        "workflow input validation failed for {workflow} in {repo} at ref {git_ref}:\n  - {}",
        .errors.join("\n  - ")
    )]
    InputValidation {
        repo: String,
        workflow: String,
        git_ref: String,
        errors: Vec<String>,
    },
    #[error("build {build:?}: workflow run {run_id} finished {} - see {html_url}", .conclusion.map_or("with no conclusion", RunConclusion::as_str))]
    RunFailed {
        build: String,
        run_id: u64,
        conclusion: Option<RunConclusion>,
        html_url: String,
    },
    #[error(
        "build {build:?}: workflow run {run_id} did not finish within {timeout:?} \
         (last status: {}) - see {html_url}", .status.map_or("unknown", RunStatus::as_str)
    )]
    RunUnfinished {
        build: String,
        run_id: u64,
        timeout: Duration,
        status: Option<RunStatus>,
        html_url: String,
    },
}

/// A resolved GithubActions build: everything the dispatch needs with no manifest or template
/// indirection left.
#[derive(Debug, Clone)]
pub struct GithubBuildRequest {
    pub name: String,
    pub repo: RepoTarget,
    pub workflow: String,
    pub git_ref: String,
    pub inputs: BTreeMap<String, String>,
    pub correlation_id: String,
    pub digest: DigestSource,
    pub image_ref: String,
    pub timeout: Duration,
}

pub fn new_correlation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub async fn run_build(
    client: &Client,
    req: &GithubBuildRequest,
    token: &Token,
) -> Result<DispatchedRun, GithubError> {
    ensure_workflow_filename(&req.workflow)?;
    ensure_digest_supported(&req.name, &req.digest)?;
    let repo = req.repo.repo();
    let dispatched = client
        .dispatch(token, repo, &req.workflow, &req.git_ref, &req.inputs)
        .await?;
    tracing::info!(
        build = %req.name,
        repo = %repo,
        run = dispatched.run_id,
        url = %dispatched.html_url,
        "dispatched the build workflow"
    );
    let deadline = tokio::time::Instant::now() + req.timeout;
    let mut last = None;
    loop {
        match client.run(token, repo, dispatched.run_id).await {
            Ok(run) if run.status == Some(RunStatus::Completed) => {
                return match run.conclusion {
                    Some(RunConclusion::Success) => Ok(dispatched),
                    conclusion => Err(BuildFailure::RunFailed {
                        build: req.name.clone(),
                        run_id: run.id,
                        conclusion,
                        html_url: run.html_url,
                    }
                    .into()),
                };
            }
            Ok(run) => last = run.status,
            Err(e) if e.is_transient() => {
                tracing::warn!(build = %req.name, error = %e, "reading the build run failed; asking again");
            }
            Err(e) => return Err(e),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(BuildFailure::RunUnfinished {
                build: req.name.clone(),
                run_id: dispatched.run_id,
                timeout: req.timeout,
                status: last,
                html_url: dispatched.html_url,
            }
            .into());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn preflight_workflow(
    client: &Client,
    token: &Token,
    repo: &RepoTarget,
    workflow: &str,
    git_ref: &str,
    mapped: &BTreeMap<String, String>,
    digest: &DigestSource,
    name: &str,
) -> Result<Vec<String>, GithubError> {
    ensure_digest_supported(name, digest)?;
    ensure_workflow_filename(workflow)?;
    let path = format!(".github/workflows/{workflow}");
    let text = client
        .workflow_file(token, repo.repo(), &path, git_ref)
        .await?;
    let parsed = Workflow::parse(&text).map_err(|error| GithubError::Workflow {
        repo: repo.to_string(),
        workflow: workflow.to_owned(),
        git_ref: git_ref.to_owned(),
        error: Box::new(error),
    })?;
    let report = validate_input_mapping(&parsed.inputs, mapped);
    if !report.errors.is_empty() {
        return Err(BuildFailure::InputValidation {
            repo: repo.to_string(),
            workflow: workflow.to_owned(),
            git_ref: git_ref.to_owned(),
            errors: report.errors,
        }
        .into());
    }
    Ok(report.warnings)
}

/// The outcome of validating a declared input mapping. Errors refuse; warnings do not.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MappingReport {
    errors: Vec<String>,
    warnings: Vec<String>,
}

fn validate_input_mapping(
    declared: &[WorkflowInput],
    mapped: &BTreeMap<String, String>,
) -> MappingReport {
    let mut report = MappingReport::default();
    let declared_by_name: BTreeMap<&str, &WorkflowInput> =
        declared.iter().map(|i| (i.name.as_str(), i)).collect();

    for input in declared {
        if input.required && input.default.is_none() && !mapped.contains_key(&input.name) {
            report.errors.push(format!(
                "workflow input {:?} is required but the manifest maps no value for it",
                input.name
            ));
        }
    }

    for key in mapped.keys() {
        match declared_by_name.get(key.as_str()) {
            None => report.errors.push(format!(
                "manifest maps input {key:?}, which the workflow does not declare"
            )),
            Some(input) => {
                if let Some(ty @ (InputType::Boolean | InputType::Number)) = input.declared {
                    report.warnings.push(format!(
                        "workflow input {key:?} is declared type {:?}; crucible forwards a \
                         template-expanded string, which the workflow must coerce",
                        ty.as_str()
                    ));
                }
            }
        }
    }
    report
}

/// A dispatch target that has cleared the operator org allowlist. [`OrgAllowlist::authorize`] is
/// the only constructor, so a repository string taken from a pack manifest cannot reach a GitHub
/// call without having been checked against operator configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTarget(Repo);

impl RepoTarget {
    pub fn repo(&self) -> &Repo {
        &self.0
    }
}

impl std::fmt::Display for RepoTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The operator-configured set of github owners a `workflow_dispatch` build may target. Read from
/// `$FORGE_GITHUB_ALLOWED_ORGS`; no manifest field feeds it, so a pack cannot widen its own reach.
/// An empty set permits nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrgAllowlist {
    owners: BTreeSet<String>,
}

impl OrgAllowlist {
    pub fn from_env() -> Result<Self, GithubError> {
        match std::env::var(ALLOWED_ORGS_ENV) {
            Ok(raw) => Self::parse(&raw),
            Err(_) => Ok(Self::default()),
        }
    }

    pub fn parse(raw: &str) -> Result<Self, GithubError> {
        let mut owners = BTreeSet::new();
        for entry in raw.split([',', ' ', '\t', '\n', '\r']) {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if !entry.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err(BuildFailure::MalformedAllowedOrg {
                    entry: entry.to_owned(),
                }
                .into());
            }
            owners.insert(entry.to_ascii_lowercase());
        }
        Ok(OrgAllowlist { owners })
    }

    fn rendered(&self) -> String {
        self.owners.iter().cloned().collect::<Vec<_>>().join(", ")
    }

    pub fn authorize(&self, repo: &str) -> Result<RepoTarget, GithubError> {
        let target = Repo::parse(repo)?;
        if self.owners.is_empty() {
            return Err(BuildFailure::EmptyOrgAllowlist {
                repo: target.to_string(),
            }
            .into());
        }
        if !self.owners.contains(&target.owner().to_ascii_lowercase()) {
            return Err(BuildFailure::OrgNotAllowed {
                repo: target.to_string(),
                owner: target.owner().to_owned(),
                allowed: self.rendered(),
            }
            .into());
        }
        Ok(RepoTarget(target))
    }
}

fn ensure_workflow_filename(workflow: &str) -> Result<(), BuildFailure> {
    let stem = workflow
        .strip_suffix(".yml")
        .or_else(|| workflow.strip_suffix(".yaml"));
    let valid = stem.is_some_and(|stem| {
        !stem.is_empty()
            && !workflow.contains("..")
            && workflow
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    });
    match valid {
        true => Ok(()),
        false => Err(BuildFailure::MalformedWorkflowFilename {
            workflow: workflow.to_owned(),
        }),
    }
}

fn ensure_digest_supported(name: &str, digest: &DigestSource) -> Result<(), BuildFailure> {
    match digest {
        DigestSource::Kind(DigestKind::RegistryTag) => Ok(()),
        DigestSource::Artifact {
            name: artifact_name,
            path,
            ..
        } => Err(BuildFailure::UnsupportedDigestSource {
            build: name.to_owned(),
            artifact: artifact_name.clone(),
            path: path.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::github::GithubError;
    use crate::github::build::*;
    use crate::github::workflow::{InputType, Workflow, WorkflowInput};
    use crate::spec::DigestSource;

    const CONTRACT_V1: &str = include_str!("../../../.github/workflows/crucible-build.yml");

    fn contract_inputs() -> Vec<WorkflowInput> {
        Workflow::parse(CONTRACT_V1).unwrap().inputs
    }

    fn input(name: &str, declared: Option<InputType>) -> WorkflowInput {
        WorkflowInput {
            name: name.into(),
            declared,
            description: None,
            required: false,
            default: None,
            options: Vec::new(),
        }
    }

    #[test]
    fn mapping_validation_passes_a_complete_contract_v1_mapping() {
        let mapped = BTreeMap::from([
            ("containerfile".into(), "Containerfile".into()),
            ("context-ref".into(), "{{ sha }}".into()),
            ("image".into(), "{{ image }}:{{ tag }}".into()),
            ("correlation_id".into(), "{{ correlation_id }}".into()),
        ]);
        let report = validate_input_mapping(&contract_inputs(), &mapped);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn mapping_validation_fails_on_a_missing_required_input() {
        let mapped = BTreeMap::from([
            ("containerfile".into(), "Containerfile".into()),
            ("context-ref".into(), "{{ sha }}".into()),
            ("image".into(), "{{ image }}:{{ tag }}".into()),
        ]);
        let report = validate_input_mapping(&contract_inputs(), &mapped);
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(report.errors[0].contains("correlation_id"));
        assert!(report.errors[0].contains("required"));
    }

    #[test]
    fn a_required_input_with_a_default_needs_no_mapping() {
        let mut tag = input("tag", None);
        tag.required = true;
        tag.default = Some(serde_json::json!("latest"));
        let report = validate_input_mapping(&[tag], &BTreeMap::new());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }

    #[test]
    fn mapping_validation_fails_on_an_undeclared_input() {
        let mapped = BTreeMap::from([
            ("containerfile".into(), "Containerfile".into()),
            ("context-ref".into(), "{{ sha }}".into()),
            ("image".into(), "{{ image }}:{{ tag }}".into()),
            ("correlation_id".into(), "{{ correlation_id }}".into()),
            ("bogus".into(), "x".into()),
        ]);
        let report = validate_input_mapping(&contract_inputs(), &mapped);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("bogus") && e.contains("does not declare")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn mapping_validation_warns_on_a_non_string_type() {
        let inputs = vec![
            input("flag", Some(InputType::Boolean)),
            input("mode", Some(InputType::Choice)),
            input("count", Some(InputType::Number)),
        ];
        let mapped = BTreeMap::from([
            ("flag".into(), "{{ tag }}".into()),
            ("mode".into(), "fast".into()),
            ("count".into(), "3".into()),
        ]);
        let report = validate_input_mapping(&inputs, &mapped);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("count"));
        assert!(report.warnings[1].contains("flag"));
    }

    #[test]
    fn allowlist_refuses_an_out_of_allowlist_target() {
        let allowlist = OrgAllowlist::parse("neuralmagic, vllm-project").unwrap();
        let err = allowlist.authorize("attacker/exfil").unwrap_err();
        assert!(
            matches!(&err, GithubError::Build(BuildFailure::OrgNotAllowed { repo, owner, .. })
                if repo == "attacker/exfil" && owner == "attacker"),
            "{err:?}"
        );
        let rendered = err.to_string();
        assert!(rendered.contains("attacker/exfil"), "{rendered}");
        assert!(rendered.contains("neuralmagic, vllm-project"), "{rendered}");
        assert!(rendered.contains(ALLOWED_ORGS_ENV), "{rendered}");
    }

    #[test]
    fn allowlist_admits_a_configured_owner_case_insensitively() {
        let allowlist = OrgAllowlist::parse("neuralmagic\nvllm-project").unwrap();
        let cleared = allowlist.authorize("NeuralMagic/crucible").unwrap();
        assert_eq!(cleared.repo().owner(), "NeuralMagic");
        assert_eq!(cleared.to_string(), "NeuralMagic/crucible");
        assert!(allowlist.authorize("vllm-project/vllm").is_ok());
        assert!(matches!(
            allowlist.authorize("vllm-project"),
            Err(GithubError::MalformedRepo { .. })
        ));
    }

    #[test]
    fn an_unset_or_empty_allowlist_permits_nothing() {
        for raw in ["", "   ", ",,"] {
            let allowlist = OrgAllowlist::parse(raw).unwrap();
            assert!(allowlist.owners.is_empty(), "{raw:?}");
            let err = allowlist.authorize("neuralmagic/crucible").unwrap_err();
            assert!(
                matches!(&err, GithubError::Build(BuildFailure::EmptyOrgAllowlist { repo }) if repo == "neuralmagic/crucible"),
                "{err:?}"
            );
            assert!(err.to_string().contains(ALLOWED_ORGS_ENV));
        }
        assert!(
            OrgAllowlist::default()
                .authorize("neuralmagic/crucible")
                .is_err()
        );
    }

    #[test]
    fn allowlist_rejects_entries_that_are_not_owners() {
        for bad in ["neuralmagic/crucible", "org*", "https://github.com/org"] {
            let err = OrgAllowlist::parse(bad).unwrap_err();
            assert!(
                matches!(&err, GithubError::Build(BuildFailure::MalformedAllowedOrg { entry }) if entry == bad),
                "{err:?}"
            );
        }
    }

    #[test]
    fn correlation_ids_are_unique_uuids() {
        let a = new_correlation_id();
        let b = new_correlation_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36, "uuid v4 hyphenated form");
    }

    #[test]
    fn ensure_workflow_filename_accepts_and_rejects() {
        assert!(ensure_workflow_filename("crucible-build.yml").is_ok());
        assert!(ensure_workflow_filename("build.yaml").is_ok());
        assert!(ensure_workflow_filename("ci_1.2.yml").is_ok());
        for bad in [
            "crucible-build",
            "crucible-build.txt",
            "dir/build.yml",
            "../secrets.yml",
            ".yml",
            "build .yml",
            "",
        ] {
            assert!(ensure_workflow_filename(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ensure_digest_supported_rejects_artifact() {
        use crate::spec::ArtifactMarker;
        assert!(ensure_digest_supported("b", &DigestSource::default()).is_ok());
        let artifact = DigestSource::Artifact {
            from: ArtifactMarker::Artifact,
            name: "digest".into(),
            path: "digest.txt".into(),
        };
        let err = ensure_digest_supported("mybuild", &artifact)
            .unwrap_err()
            .to_string();
        assert!(err.contains("artifact"), "{err}");
        assert!(err.contains("mybuild"), "{err}");
        assert!(err.contains("not implemented"), "{err}");
    }

    #[test]
    fn a_failed_run_names_its_conclusion_and_url() {
        let failed = BuildFailure::RunFailed {
            build: "b".into(),
            run_id: 7,
            conclusion: Some(crate::github::RunConclusion::TimedOut),
            html_url: "https://github.com/o/r/actions/runs/7".into(),
        }
        .to_string();
        assert!(failed.contains("timed_out"), "{failed}");
        assert!(failed.contains("/runs/7"), "{failed}");
        let unfinished = BuildFailure::RunUnfinished {
            build: "b".into(),
            run_id: 7,
            timeout: std::time::Duration::from_secs(60),
            status: Some(crate::github::RunStatus::Queued),
            html_url: "u".into(),
        }
        .to_string();
        assert!(unfinished.contains("queued"), "{unfinished}");
    }
}
