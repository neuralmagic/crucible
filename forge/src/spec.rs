//! Declarative image-build schema: the `[build.<name>]` manifest tables, cross-field validation,
//! the closed-vocabulary template expander, and the watched-path content digest that answers
//! "build needed?". Pure config + hashing (no cluster, no registry), so it unit-tests without
//! any I/O beyond reading the watched files. The crucible crate parses these into the domain
//! manifest; forge's build backends consume them.
//!
//! ```text
//!   [build.<name>]        -> BuildSpec (backend, image, timeout, needs, watch)
//!     .cluster            -> ClusterBuild (containerfile, context, platform)
//!     .github             -> GithubBuild  (repo, workflow, ref, inputs, outputs)
//! ```

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use walkdir::WalkDir;

type Result<T> = std::result::Result<T, SpecError>;

/// Everything the `[build]` schema can reject: template vocabulary, cross-field validation,
/// and the watched-path digest walk. Causes hang off `source()`, so render with anyhow's
/// `{:#}` (or [`std::error::Error::source`]) rather than a bare `{}`.
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error(
        "unknown template variable {{{{ {var} }}}} — the vocabulary is closed: \
         sha, tag, image, correlation_id, builds.<name>.digest_ref"
    )]
    UnknownTemplateVar { var: String },
    #[error("unterminated `{{{{` in template {template:?}")]
    UnterminatedTemplate { template: String },
    #[error("no digest provided for dependency build {dep:?}")]
    MissingDependencyDigest { dep: String },

    #[error(
        "[build.{build}] backend = \"cluster\" needs a [build.{build}.cluster] table \
         (containerfile, context, platform)"
    )]
    MissingClusterTable { build: String },
    #[error(
        "[build.{build}] backend = \"github-actions\" needs a [build.{build}.github] table \
         (repo, workflow, ...)"
    )]
    MissingGithubTable { build: String },
    #[error("[build.{build}].needs references itself")]
    SelfNeeds { build: String },
    #[error("[build.{build}].needs references {dep:?}, which is not a declared build")]
    UndeclaredNeeds { build: String, dep: String },
    #[error("cycle in [build] needs: {} -> {dep}", .path.join(" -> "))]
    NeedsCycle { path: Vec<String>, dep: String },

    /// A bad template in a named field; `field` is the `[build.x.y].z` the error points at.
    #[error("{field}")]
    Field {
        field: String,
        #[source]
        source: Box<SpecError>,
    },
    #[error("{field} references builds.{dep}.digest_ref, an undeclared build")]
    UndeclaredDigestRef { field: String, dep: String },
    #[error(
        "{field} uses builds.{dep}.digest_ref but {dep:?} is not in this build's needs — \
         add it so the digest is available before dispatch"
    )]
    DigestRefNotInNeeds { field: String, dep: String },

    #[error("invalid watch glob {pattern:?}")]
    InvalidWatchGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
    #[error("building the watch globset")]
    BuildGlobset(#[source] globset::Error),
    #[error("walking the workspace tree for the content digest")]
    WalkTree(#[from] walkdir::Error),
    #[error("reading watched file {}", .path.display())]
    ReadWatchedFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Which backend builds and pushes a named image. `Cluster` = a detached rootless-buildah Job on the
/// engine's own cluster (M0); `GithubActions` = a `workflow_dispatch` against a repo's build workflow
/// (parses now, dispatched from M2). Both push to the tag they're given and let crucible resolve the
/// digest from the registry; the backend never reports it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildBackend {
    Cluster,
    GithubActions,
}

/// A build timeout in kubectl-friendly form (`30m`, `1h`, `300s`), parsed + range-checked at
/// deserialize so a typo is a manifest error instead of an infinite or zero wait at dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout(Duration);

impl Timeout {
    pub fn as_duration(self) -> Duration {
        self.0
    }
}

impl<'de> Deserialize<'de> for Timeout {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_duration(&s).map(Timeout).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "invalid timeout {s:?} — want a friendly duration like \"30m\", \"1h\", \"300s\""
            ))
        })
    }
}

/// Parse a friendly duration (`30m`/`1h`/`300s`) into a positive [`Duration`]; `None` on anything
/// unparseable or non-positive (a zero or negative build timeout is a config error).
fn parse_duration(s: &str) -> Option<Duration> {
    let d: jiff::SignedDuration = s.trim().parse().ok()?;
    let d = Duration::try_from(d).ok()?;
    (!d.is_zero()).then_some(d)
}

/// One `[build.<name>]` table: the backend + destination image, plus ordering (`needs`) and the
/// rebuild predicate (`watch`). Exactly one per-backend table is required, matching `backend`
/// (validated in [`validate_builds`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSpec {
    pub backend: BuildBackend,
    /// Destination repository WITHOUT a tag, e.g. `ghcr.io/example/app-sandbox`.
    pub image: String,
    /// Wall-clock cap on the build (default 30m). A wedged build cannot hold a deployment slot
    /// past this.
    #[serde(default = "default_timeout")]
    pub timeout: Timeout,
    /// Ordering + digest-availability dependencies: a build isn't dispatchable until each `needs`
    /// entry has a recorded digest. Every entry must name a declared build (validated).
    #[serde(default)]
    pub needs: Vec<String>,
    /// The path set whose content digest decides "build needed" (§2). Empty = always rebuild.
    #[serde(default)]
    pub watch: WatchCfg,
    /// `[build.<name>.cluster]`, required when `backend = "cluster"`.
    #[serde(default)]
    pub cluster: Option<ClusterBuild>,
    /// `[build.<name>.github]`, required when `backend = "github-actions"`.
    #[serde(default)]
    pub github: Option<GithubBuild>,
}

fn default_timeout() -> Timeout {
    Timeout(Duration::from_secs(30 * 60))
}

/// `[build.<name>.watch]`: the files whose content defines this image's build. Globs are matched
/// against workspace-relative paths (`**` spans directories).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchCfg {
    #[serde(default)]
    pub paths: Vec<String>,
}

/// `[build.<name>.cluster]`: the detached rootless-buildah Job's inputs. `containerfile` and
/// `context` are resolved relative to the cloned source tree; `platform` is the buildah `--platform`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterBuild {
    pub containerfile: String,
    #[serde(default = "default_context")]
    pub context: String,
    #[serde(default = "default_platform")]
    pub platform: String,
}

fn default_context() -> String {
    ".".to_string()
}
fn default_platform() -> String {
    "linux/amd64".to_string()
}

impl ClusterBuild {
    /// The template-expandable fields, as `(label, value)`. The closed vocabulary is validated over
    /// these at manifest load and expanded over them on the dispatch path, so no unvalidated template
    /// string can reach the build Job.
    fn templatable_fields(&self) -> [(&'static str, &str); 3] {
        [
            ("containerfile", &self.containerfile),
            ("context", &self.context),
            ("platform", &self.platform),
        ]
    }
}

/// `[build.<name>.github]`: a `workflow_dispatch` target. `inputs` values are template-expanded
/// (closed vocabulary) before dispatch; `outputs` declares how an unmodifiable workflow reports its
/// digest / correlation (defaults honor the contract-v1 workflow).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubBuild {
    /// `owner/repo` holding the build workflow. NOTE: an allowed-orgs whitelist is NOT YET ENFORCED
    /// anywhere in this branch; enforcing it is a prerequisite before the M1 controller dispatches
    /// this path autonomously (tracked as M1 integration-hardening follow-up work).
    pub repo: String,
    /// The workflow file to dispatch (must exist on `ref`; crucible never generates it).
    pub workflow: String,
    #[serde(default = "default_ref", rename = "ref")]
    pub git_ref: String,
    /// `workflow_dispatch` inputs, forwarded after template expansion; nothing else is passed.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    #[serde(default)]
    pub outputs: Option<GithubOutputs>,
}

fn default_ref() -> String {
    "main".to_string()
}

/// The declared escape hatches for a workflow that can't honor the contract v1 defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubOutputs {
    #[serde(default)]
    pub digest: DigestSource,
    #[serde(default)]
    pub correlation: CorrelationSource,
}

/// How a build's pushed digest is discovered. Default: re-resolve the pushed tag from the registry
/// (the single source of truth). The artifact form is the escape hatch for a workflow whose push tag
/// we can't observe; it uploads the digest as a named artifact file instead.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum DigestSource {
    Kind(DigestKind),
    Artifact {
        from: ArtifactMarker,
        name: String,
        path: String,
    },
}

impl Default for DigestSource {
    fn default() -> Self {
        DigestSource::Kind(DigestKind::RegistryTag)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DigestKind {
    RegistryTag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactMarker {
    Artifact,
}

/// How our dispatch is correlated to a workflow run. Default: the run name echoes the
/// `correlation_id` input (contract v1). `dispatch-window` serializes dispatches per workflow when
/// the run name can't carry the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CorrelationSource {
    #[default]
    RunName,
    DispatchWindow,
}

/// The closed template vocabulary (§2). Anything outside this set is a manifest error: the config is
/// agent-influenced content post-pack-approval, so an open substitution language would be a
/// string-injection path into someone's CI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateVar {
    Sha,
    Tag,
    Image,
    CorrelationId,
    /// `builds.<name>.digest_ref`: the pinned output of a dependency build.
    BuildDigestRef(String),
}

/// Parse one `{{ … }}` body into its [`TemplateVar`]; errors on anything outside the vocabulary.
fn parse_var(inner: &str) -> Result<TemplateVar> {
    let t = inner.trim();
    match t {
        "sha" => Ok(TemplateVar::Sha),
        "tag" => Ok(TemplateVar::Tag),
        "image" => Ok(TemplateVar::Image),
        "correlation_id" => Ok(TemplateVar::CorrelationId),
        _ => {
            if let Some(name) = t
                .strip_prefix("builds.")
                .and_then(|r| r.strip_suffix(".digest_ref"))
                && !name.is_empty()
                && !name.contains('.')
            {
                return Ok(TemplateVar::BuildDigestRef(name.to_string()));
            }
            Err(SpecError::UnknownTemplateVar { var: t.to_owned() })
        }
    }
}

/// Every `{{ … }}` variable in `s`, in order; errors on an unterminated `{{` or an out-of-vocabulary
/// variable. Used at validation to reject bad templates before any dispatch.
fn template_vars(s: &str) -> Result<Vec<TemplateVar>> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| SpecError::UnterminatedTemplate {
                template: s.to_owned(),
            })?;
        out.push(parse_var(&after[..end])?);
        rest = &after[end + 2..];
    }
    Ok(out)
}

/// The concrete values the closed vocabulary expands to for one dispatch.
#[derive(Debug, Clone, Default)]
pub struct TemplateContext {
    pub sha: String,
    pub tag: String,
    pub image: String,
    pub correlation_id: String,
    /// Resolved `digest_ref` per dependency build name.
    pub build_digests: BTreeMap<String, String>,
}

impl TemplateContext {
    /// Expand `{{ … }}` in `template` against this context; errors on an unterminated brace, an
    /// out-of-vocabulary variable, or a `builds.<name>.digest_ref` with no provided digest.
    pub fn expand(&self, template: &str) -> Result<String> {
        let mut out = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after
                .find("}}")
                .ok_or_else(|| SpecError::UnterminatedTemplate {
                    template: template.to_owned(),
                })?;
            out.push_str(&self.resolve(&parse_var(&after[..end])?)?);
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn resolve(&self, var: &TemplateVar) -> Result<String> {
        Ok(match var {
            TemplateVar::Sha => self.sha.clone(),
            TemplateVar::Tag => self.tag.clone(),
            TemplateVar::Image => self.image.clone(),
            TemplateVar::CorrelationId => self.correlation_id.clone(),
            TemplateVar::BuildDigestRef(name) => self
                .build_digests
                .get(name)
                .cloned()
                .ok_or_else(|| SpecError::MissingDependencyDigest { dep: name.clone() })?,
        })
    }
}

/// Validate a whole `[build]` map (§2): per-backend table presence, `needs` references declared
/// builds, `needs` is cycle-free, and every template stays in the closed vocabulary with its
/// `builds.<name>` refs pointing at a declared dependency. Run at manifest load so a bad build block
/// fails before any dispatch.
pub fn validate_builds(builds: &BTreeMap<String, BuildSpec>) -> Result<()> {
    for (name, spec) in builds {
        match spec.backend {
            BuildBackend::Cluster if spec.cluster.is_none() => {
                return Err(SpecError::MissingClusterTable {
                    build: name.clone(),
                });
            }
            BuildBackend::GithubActions if spec.github.is_none() => {
                return Err(SpecError::MissingGithubTable {
                    build: name.clone(),
                });
            }
            _ => {}
        }

        for dep in &spec.needs {
            if dep == name {
                return Err(SpecError::SelfNeeds {
                    build: name.clone(),
                });
            }
            if !builds.contains_key(dep) {
                return Err(SpecError::UndeclaredNeeds {
                    build: name.clone(),
                    dep: dep.clone(),
                });
            }
        }

        // `image` is expanded like any other field (the cluster backend does so before it builds
        // the push ref), so it gets the same closed-vocabulary check. A `builds.<dep>.digest_ref`
        // here is load-bearing for ordering: without `dep` in `needs`, this build dispatches
        // before that digest exists.
        check_template_field(&format!("[build.{name}].image"), &spec.image, spec, builds)?;

        if let Some(gh) = &spec.github {
            for (key, value) in &gh.inputs {
                let where_ = format!("[build.{name}.github.inputs].{key}");
                check_template_field(&where_, value, spec, builds)?;
            }
        }

        // The cluster fields are the closed-vocabulary injection defense (§2): validate them the same
        // way as github inputs so no unvalidated template string reaches the build Job.
        if let Some(cluster) = &spec.cluster {
            for (field, value) in cluster.templatable_fields() {
                let where_ = format!("[build.{name}.cluster].{field}");
                check_template_field(&where_, value, spec, builds)?;
            }
        }
    }
    detect_cycles(builds)
}

/// Validate one templatable field's `{{ … }}` refs against the closed vocabulary: every variable must
/// be in-vocabulary, and every `builds.<dep>.digest_ref` must name a declared build that is listed in
/// this build's `needs` (so the digest is available before dispatch). `where_` names the field for the
/// error. Shared by the github-inputs and cluster-field validation paths.
fn check_template_field(
    where_: &str,
    value: &str,
    spec: &BuildSpec,
    builds: &BTreeMap<String, BuildSpec>,
) -> Result<()> {
    let vars = template_vars(value).map_err(|source| SpecError::Field {
        field: where_.to_owned(),
        source: Box::new(source),
    })?;
    for var in vars {
        let TemplateVar::BuildDigestRef(dep) = &var else {
            continue;
        };
        if !builds.contains_key(dep) {
            return Err(SpecError::UndeclaredDigestRef {
                field: where_.to_owned(),
                dep: dep.clone(),
            });
        }
        if !spec.needs.contains(dep) {
            return Err(SpecError::DigestRefNotInNeeds {
                field: where_.to_owned(),
                dep: dep.clone(),
            });
        }
    }
    Ok(())
}

/// Plain DFS cycle check over the `needs` edges (a dep list, not a general DAG engine). Reports the
/// offending path.
fn detect_cycles(builds: &BTreeMap<String, BuildSpec>) -> Result<()> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        OnStack,
        Done,
    }
    let mut marks: BTreeMap<&str, Mark> = BTreeMap::new();
    for name in builds.keys() {
        if !marks.contains_key(name.as_str()) {
            visit(name, builds, &mut marks, &mut Vec::new())?;
        }
    }

    fn visit<'a>(
        name: &'a str,
        builds: &'a BTreeMap<String, BuildSpec>,
        marks: &mut BTreeMap<&'a str, Mark>,
        path: &mut Vec<&'a str>,
    ) -> Result<()> {
        marks.insert(name, Mark::OnStack);
        path.push(name);
        if let Some(spec) = builds.get(name) {
            for dep in &spec.needs {
                match marks.get(dep.as_str()) {
                    Some(Mark::OnStack) => {
                        return Err(SpecError::NeedsCycle {
                            path: path.iter().map(|step| (*step).to_owned()).collect(),
                            dep: dep.clone(),
                        });
                    }
                    Some(Mark::Done) => {}
                    None => visit(dep, builds, marks, path)?,
                }
            }
        }
        path.pop();
        marks.insert(name, Mark::Done);
        Ok(())
    }
    Ok(())
}

/// The "build-needed" verdict for a watched path set (§2). `AlwaysRebuild` is the empty-`watch` case:
/// with no declared paths there is nothing to compare, so the contract is "rebuild every time", a
/// force signal that never equals a prior digest. `Digest` is the content hash of a non-empty set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchDigest {
    /// No watched paths declared: always rebuild (never compares equal to a recorded digest).
    AlwaysRebuild,
    /// The sha256 of the watched path set's content.
    Digest(String),
}

/// Content digest of the watched path set at `root` (§2, the "build-needed" predicate): glob-expand
/// `patterns` against workspace-relative paths, sort the matched files, and fold each file's relative
/// path + length + bytes into one sha256. Deterministic: an unchanged tree yields an unchanged
/// digest, and any watched edit (or a rename that changes a path) changes it. Length-prefixing the
/// bytes keeps two files' contents from aliasing across a boundary. An empty `patterns` yields
/// [`WatchDigest::AlwaysRebuild`] rather than the constant hash of nothing, so "empty watch = always
/// rebuild" holds instead of silently latching STABLE.
pub fn content_digest(root: &Path, patterns: &[String]) -> Result<WatchDigest> {
    if patterns.is_empty() {
        return Ok(WatchDigest::AlwaysRebuild);
    }
    use globset::{Glob, GlobSetBuilder};
    let mut gsb = GlobSetBuilder::new();
    for p in patterns {
        gsb.add(Glob::new(p).map_err(|source| SpecError::InvalidWatchGlob {
            pattern: p.clone(),
            source,
        })?);
    }
    let set = gsb.build().map_err(SpecError::BuildGlobset)?;

    let mut files: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if set.is_match(rel) {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();

    let mut hasher = Sha256::new();
    for f in &files {
        let rel = f.strip_prefix(root).unwrap_or(f);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0u8]);
        let bytes = std::fs::read(f).map_err(|source| SpecError::ReadWatchedFile {
            path: f.clone(),
            source,
        })?;
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(WatchDigest::Digest(format!(
        "sha256:{}",
        hex::encode(hasher.finalize())
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builds_from(toml_str: &str) -> BTreeMap<String, BuildSpec> {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default)]
            build: BTreeMap<String, BuildSpec>,
        }
        let w: Wrap = toml::from_str(toml_str).expect("parse build map");
        w.build
    }

    #[test]
    fn parses_a_cluster_build_with_defaults() {
        let b = builds_from(
            r#"
            [build.alpha-sandbox]
            backend = "cluster"
            image   = "ghcr.io/example/alpha-sandbox"
            [build.alpha-sandbox.cluster]
            containerfile = "packs/alpha/Containerfile.sandbox"
            [build.alpha-sandbox.watch]
            paths = ["packs/alpha/Containerfile.sandbox", "packs/alpha/tools/**"]
        "#,
        );
        let s = &b["alpha-sandbox"];
        assert_eq!(s.backend, BuildBackend::Cluster);
        assert_eq!(s.image, "ghcr.io/example/alpha-sandbox");
        assert_eq!(s.timeout.as_duration(), Duration::from_secs(1800));
        let c = s.cluster.as_ref().unwrap();
        assert_eq!(c.context, ".", "context defaults to repo root");
        assert_eq!(c.platform, "linux/amd64");
        assert_eq!(s.watch.paths.len(), 2);
        validate_builds(&b).unwrap();
    }

    #[test]
    fn parses_a_github_build_with_outputs() {
        let b = builds_from(
            r#"
            [build.app-sandbox]
            backend = "cluster"
            image   = "ghcr.io/x/app-sandbox"
            [build.app-sandbox.cluster]
            containerfile = "Containerfile"

            [build.sandbox]
            backend = "github-actions"
            image   = "ghcr.io/x/sandbox"
            needs   = ["app-sandbox"]
            [build.sandbox.github]
            repo     = "neuralmagic/crucible"
            workflow = "crucible-build.yml"
            ref      = "main"
            [build.sandbox.github.inputs]
            containerfile = "Containerfile.sandbox"
            base-image    = "{{ builds.app-sandbox.digest_ref }}"
            image         = "{{ image }}:{{ tag }}"
            [build.sandbox.github.outputs]
            digest      = "registry-tag"
            correlation = "run-name"
        "#,
        );
        let gh = b["sandbox"].github.as_ref().unwrap();
        assert_eq!(gh.repo, "neuralmagic/crucible");
        assert_eq!(gh.git_ref, "main");
        let out = gh.outputs.as_ref().unwrap();
        assert_eq!(out.digest, DigestSource::Kind(DigestKind::RegistryTag));
        assert_eq!(out.correlation, CorrelationSource::RunName);
        validate_builds(&b).unwrap();
    }

    #[test]
    fn parses_the_artifact_digest_escape_hatch() {
        let b = builds_from(
            r#"
            [build.x]
            backend = "github-actions"
            image   = "ghcr.io/x/x"
            [build.x.github]
            repo     = "o/r"
            workflow = "w.yml"
            [build.x.github.outputs]
            digest = { from = "artifact", name = "digest", path = "digest.txt" }
        "#,
        );
        let out = b["x"].github.as_ref().unwrap().outputs.as_ref().unwrap();
        assert_eq!(
            out.digest,
            DigestSource::Artifact {
                from: ArtifactMarker::Artifact,
                name: "digest".into(),
                path: "digest.txt".into(),
            }
        );
    }

    #[test]
    fn rejects_a_bad_timeout() {
        let bad = toml::from_str::<BuildSpec>(
            r#"
            backend = "cluster"
            image   = "x"
            timeout = "soon"
            [cluster]
            containerfile = "C"
        "#,
        );
        assert!(bad.is_err(), "unparseable timeout is a deserialize error");
    }

    #[test]
    fn cluster_backend_requires_its_table() {
        let b = builds_from(
            r#"
            [build.x]
            backend = "cluster"
            image   = "ghcr.io/x/x"
        "#,
        );
        let err = validate_builds(&b).unwrap_err().to_string();
        assert!(err.contains("[build.x.cluster]"), "{err}");
    }

    #[test]
    fn github_backend_requires_its_table() {
        let b = builds_from(
            r#"
            [build.x]
            backend = "github-actions"
            image   = "ghcr.io/x/x"
        "#,
        );
        let err = validate_builds(&b).unwrap_err().to_string();
        assert!(err.contains("[build.x.github]"), "{err}");
    }

    #[test]
    fn needs_must_reference_a_declared_build() {
        let b = builds_from(
            r#"
            [build.x]
            backend = "cluster"
            image   = "ghcr.io/x/x"
            needs   = ["ghost"]
            [build.x.cluster]
            containerfile = "C"
        "#,
        );
        let err = validate_builds(&b).unwrap_err().to_string();
        assert!(err.contains("not a declared build"), "{err}");
    }

    #[test]
    fn detects_a_needs_cycle() {
        let b = builds_from(
            r#"
            [build.a]
            backend = "cluster"
            image   = "ghcr.io/x/a"
            needs   = ["b"]
            [build.a.cluster]
            containerfile = "C"
            [build.b]
            backend = "cluster"
            image   = "ghcr.io/x/b"
            needs   = ["a"]
            [build.b.cluster]
            containerfile = "C"
        "#,
        );
        let err = validate_builds(&b).unwrap_err().to_string();
        assert!(err.contains("cycle in [build] needs"), "{err}");
    }

    #[test]
    fn template_digest_ref_must_be_a_declared_need() {
        // The dep exists but isn't listed in `needs`: the digest wouldn't be available at dispatch.
        let b = builds_from(
            r#"
            [build.base]
            backend = "cluster"
            image   = "ghcr.io/x/base"
            [build.base.cluster]
            containerfile = "C"
            [build.top]
            backend = "github-actions"
            image   = "ghcr.io/x/top"
            [build.top.github]
            repo     = "o/r"
            workflow = "w.yml"
            [build.top.github.inputs]
            base = "{{ builds.base.digest_ref }}"
        "#,
        );
        let err = validate_builds(&b).unwrap_err().to_string();
        assert!(err.contains("not in this build's needs"), "{err}");
    }

    #[test]
    fn rejects_an_unknown_template_variable() {
        let err = template_vars("{{ secret }}").unwrap_err().to_string();
        assert!(err.contains("vocabulary is closed"), "{err}");
        assert!(template_vars("{{ builds.a.b.digest_ref }}").is_err());
        assert!(
            template_vars("{{ tag ").is_err(),
            "unterminated is an error"
        );
    }

    #[test]
    fn parses_the_closed_vocabulary() {
        assert_eq!(template_vars("{{sha}}").unwrap(), vec![TemplateVar::Sha]);
        assert_eq!(
            template_vars("{{ image }}:{{ tag }}").unwrap(),
            vec![TemplateVar::Image, TemplateVar::Tag]
        );
        assert_eq!(
            template_vars("{{ builds.foo.digest_ref }}").unwrap(),
            vec![TemplateVar::BuildDigestRef("foo".to_string())]
        );
    }

    #[test]
    fn expands_the_closed_vocabulary_and_errors_on_missing_digest() {
        let mut ctx = TemplateContext {
            sha: "abc123".into(),
            tag: "m4".into(),
            image: "ghcr.io/x/y".into(),
            correlation_id: "cid-7".into(),
            build_digests: BTreeMap::new(),
        };
        assert_eq!(
            ctx.expand("{{ image }}:{{ tag }}").unwrap(),
            "ghcr.io/x/y:m4"
        );
        assert_eq!(ctx.expand("built at {{ sha }}").unwrap(), "built at abc123");
        assert_eq!(ctx.expand("run-{{correlation_id}}").unwrap(), "run-cid-7");
        // A digest ref with nothing provided is a hard error rather than an empty string.
        assert!(ctx.expand("{{ builds.base.digest_ref }}").is_err());
        ctx.build_digests
            .insert("base".into(), "ghcr.io/x/base@sha256:deadbeef".into());
        assert_eq!(
            ctx.expand("FROM {{ builds.base.digest_ref }}").unwrap(),
            "FROM ghcr.io/x/base@sha256:deadbeef"
        );
    }

    #[test]
    fn content_digest_is_deterministic_and_content_sensitive() {
        let root = std::env::temp_dir().join(format!("forge-cd-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tools")).unwrap();
        std::fs::write(root.join("Containerfile"), b"FROM base\n").unwrap();
        std::fs::write(root.join("tools/a.sh"), b"echo a\n").unwrap();
        std::fs::write(root.join("ignored.txt"), b"not watched\n").unwrap();

        let patterns = vec!["Containerfile".to_string(), "tools/**".to_string()];
        let a = content_digest(&root, &patterns).unwrap();
        let b = content_digest(&root, &patterns).unwrap();
        assert_eq!(a, b, "same tree => same digest");
        let WatchDigest::Digest(a_hash) = &a else {
            panic!("non-empty watch should hash, got {a:?}");
        };
        assert!(a_hash.starts_with("sha256:"));

        // An unwatched file doesn't move it.
        std::fs::write(root.join("ignored.txt"), b"changed but unwatched\n").unwrap();
        assert_eq!(content_digest(&root, &patterns).unwrap(), a);

        // A watched edit does.
        std::fs::write(root.join("tools/a.sh"), b"echo different\n").unwrap();
        assert_ne!(content_digest(&root, &patterns).unwrap(), a);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_watch_forces_a_rebuild() {
        // No watched paths must mean "always rebuild". The constant hash of an empty set would
        // latch STABLE and never rebuild, inverting the documented contract.
        let root = std::env::temp_dir().join(format!("forge-cd-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Containerfile"), b"FROM base\n").unwrap();

        assert_eq!(
            content_digest(&root, &[]).unwrap(),
            WatchDigest::AlwaysRebuild
        );
        // AlwaysRebuild is distinct from any real digest, so a build-needed check always fires.
        let real = content_digest(&root, &["Containerfile".to_string()]).unwrap();
        assert_ne!(content_digest(&root, &[]).unwrap(), real);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `image` is template-expanded before the push ref is built, so it gets the same
    /// closed-vocabulary check as the cluster fields, and its `digest_ref` refs are held to the
    /// same `needs` rule (without which this build dispatches before the digest exists).
    #[test]
    fn image_is_closed_vocabulary_validated() {
        let bad_var = builds_from(
            r#"
            [build.x]
            backend = "cluster"
            image   = "ghcr.io/x/{{ secret }}"
            [build.x.cluster]
            containerfile = "C"
        "#,
        );
        let err = format!(
            "{:#}",
            anyhow::Error::new(validate_builds(&bad_var).unwrap_err())
        );
        assert!(err.contains("vocabulary is closed"), "{err}");
        assert!(err.contains("[build.x].image"), "{err}");

        let missing_need = builds_from(
            r#"
            [build.base]
            backend = "cluster"
            image   = "ghcr.io/x/base"
            [build.base.cluster]
            containerfile = "C"
            [build.top]
            backend = "cluster"
            image   = "{{ builds.base.digest_ref }}"
            [build.top.cluster]
            containerfile = "C"
        "#,
        );
        let err = validate_builds(&missing_need).unwrap_err().to_string();
        assert!(err.contains("not in this build's needs"), "{err}");
        assert!(err.contains("[build.top].image"), "{err}");

        // Declared need: valid. A plain untemplated image stays valid too.
        let ok = builds_from(
            r#"
            [build.base]
            backend = "cluster"
            image   = "ghcr.io/x/base"
            [build.base.cluster]
            containerfile = "C"
            [build.top]
            backend = "cluster"
            image   = "{{ builds.base.digest_ref }}"
            needs   = ["base"]
            [build.top.cluster]
            containerfile = "C"
        "#,
        );
        assert!(validate_builds(&ok).is_ok());
    }

    #[test]
    fn cluster_fields_are_closed_vocabulary_validated() {
        // An out-of-vocabulary template in a cluster field is a manifest error, same as github inputs.
        let bad = builds_from(
            r#"
            [build.x]
            backend = "cluster"
            image   = "ghcr.io/x/x"
            [build.x.cluster]
            containerfile = "{{ secret }}"
        "#,
        );
        // The field label is the outer context; the vocabulary cause is its source (shown with `{:#}`).
        let err = format!(
            "{:#}",
            anyhow::Error::new(validate_builds(&bad).unwrap_err())
        );
        assert!(err.contains("vocabulary is closed"), "{err}");
        assert!(err.contains("[build.x.cluster].containerfile"), "{err}");

        // A digest_ref in a cluster field must name a declared need.
        let missing_need = builds_from(
            r#"
            [build.base]
            backend = "cluster"
            image   = "ghcr.io/x/base"
            [build.base.cluster]
            containerfile = "C"
            [build.top]
            backend = "cluster"
            image   = "ghcr.io/x/top"
            [build.top.cluster]
            containerfile = "C"
            context       = "{{ builds.base.digest_ref }}"
        "#,
        );
        let err = validate_builds(&missing_need).unwrap_err().to_string();
        assert!(err.contains("not in this build's needs"), "{err}");

        // With the need declared, it validates.
        let ok = builds_from(
            r#"
            [build.base]
            backend = "cluster"
            image   = "ghcr.io/x/base"
            [build.base.cluster]
            containerfile = "C"
            [build.top]
            backend = "cluster"
            image   = "ghcr.io/x/top"
            needs   = ["base"]
            [build.top.cluster]
            containerfile = "C"
            context       = "{{ builds.base.digest_ref }}"
        "#,
        );
        validate_builds(&ok).unwrap();
    }
}
