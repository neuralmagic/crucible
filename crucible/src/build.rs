//! `crucible build <name>`: dispatch a named `[build.<name>]` from the domain manifest, wait for
//! it, and print the digest-pinned ref. The cluster backend renders a detached rootless-buildah
//! Job ([`forge::build`]); the `github-actions` backend dispatches a `workflow_dispatch`,
//! correlates + polls the run, and pins the pushed tag ([`forge::github`]). `--check` runs the
//! github workflow-input introspection only, then exits. This is the exact code path the
//! controller dispatches, one implementation, two callers.

use crate::manifest::{CompositeManifest, Manifest, OutputsCfg, is_composite};
use crate::outputs::{OutputTally, RunBounds};
use anyhow::{Context, Result};
use crucible_contract::outputs::{ENV_SESSION_LOG, OutputKind};

/// What the `crucible build` CLI refuses on its own terms: a missing dependency digest, a
/// malformed `--dep`, and the two credential lookups. Everything else is plumbing and stays
/// `anyhow`.
#[derive(Debug, thiserror::Error)]
pub enum BuildCliError {
    #[error(
        "build {build:?} needs {dep:?}, but no digest was provided — pass \
         --dep {dep}=<registry/repo@sha256:…> (the controller ledger supplies this in M1)"
    )]
    MissingDepDigest { build: String, dep: String },
    #[error("--dep must be name=digest_ref, got {arg:?}")]
    MalformedDep { arg: String },
    #[error("no $GITHUB_TOKEN set and `gh auth token` failed: {stderr}")]
    GhAuthFailed { stderr: String },
    #[error("no $GITHUB_TOKEN set and `gh auth token` returned an empty token")]
    GhAuthEmpty,
    #[error(
        "no push authfile found: pass --authfile, set $REGISTRY_AUTH_FILE, or create \
         ~/.docker/config.json"
    )]
    NoAuthfile,
}
use forge::spec::{BuildBackend, BuildSpec, GithubBuild, TemplateContext};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// CLI-level options for `crucible build <name>`. Also the clap `Args` group behind
/// [`crate::Cmd::Build`].
#[derive(clap::Args)]
pub struct BuildArgs {
    /// The build to dispatch, declared as `[build.<name>]` in the manifest.
    pub name: String,
    /// Domain manifest (single-domain or composite) holding the `[build]` block.
    #[arg(long)]
    pub manifest: PathBuf,
    /// Git source the build Job clones, as `<url>[@<ref>]`. Defaults to `[repo]`.
    #[arg(long)]
    pub context_ref: Option<String>,
    /// Dependency digest_ref, `name=registry/repo@sha256:…` (repeatable).
    #[arg(long = "dep")]
    pub deps: Vec<String>,
    /// Namespace to run the build Job in (default: `$CRUCIBLE_BUILD_NAMESPACE`, else `default`).
    #[arg(long)]
    pub namespace: Option<String>,
    /// Tag to push (default: `crucible-build-<correlation>`).
    #[arg(long)]
    pub tag: Option<String>,
    /// Local docker `config.json` seeding the Job's push secret.
    #[arg(long)]
    pub authfile: Option<PathBuf>,
    /// Validate the github backend's declared input mapping against the workflow and exit.
    #[arg(long)]
    pub check: bool,
}

/// Resolve the named build, enforce dependency-digest availability, and dispatch its backend.
pub fn run(args: BuildArgs) -> Result<()> {
    let loaded = load_builds(&args.manifest)?;
    let spec: &BuildSpec = loaded.builds.get(&args.name).with_context(|| {
        format!(
            "no [build.{}] in {} (declared builds: {})",
            args.name,
            args.manifest.display(),
            loaded.builds.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    })?;

    let provided = parse_deps(&args.deps)?;

    if spec.backend == BuildBackend::GithubActions {
        let gh = spec.github.as_ref().context(
            "github-actions build has no [github] table (should have failed manifest validation)",
        )?;
        let mut tally = dispatch_tally(&loaded);
        return run_github(&args, spec, gh, &provided, &mut tally);
    }

    if args.check {
        eprintln!(
            "==> nothing to introspect for build {:?} (backend = cluster); --check is github-actions only",
            args.name
        );
        return Ok(());
    }

    // Ledger stand-in for the CLI: every `needs` dependency must have a digest supplied via `--dep`
    // (the controller ledger supplies these in M1).
    for need in &spec.needs {
        if !provided.contains_key(need) {
            return Err(BuildCliError::MissingDepDigest {
                build: args.name.clone(),
                dep: need.clone(),
            }
            .into());
        }
    }

    let cluster = spec
        .cluster
        .as_ref()
        .context("cluster build has no [cluster] table (should have failed manifest validation)")?;

    let (git_url, git_ref) = resolve_context(&args.context_ref, &loaded.default_repo)?;
    let correlation_id = correlation_id();
    let tag = args
        .tag
        .clone()
        .unwrap_or_else(|| format!("crucible-build-{correlation_id}"));
    let namespace = args
        .namespace
        .clone()
        .or_else(|| std::env::var("CRUCIBLE_BUILD_NAMESPACE").ok())
        .unwrap_or_else(|| "default".to_string());
    let authfile = resolve_authfile(&args.authfile)?;

    // Expand the closed-vocabulary templates in the cluster fields before they reach the Job, this is
    // where the provided `--dep` digests are consumed (`{{ builds.<name>.digest_ref }}`), and it makes
    // the §2 injection defense real on the cluster path (nothing unexpanded flows in).
    let ctx = TemplateContext {
        sha: git_ref.clone(),
        tag: tag.clone(),
        image: spec.image.clone(),
        correlation_id: correlation_id.clone(),
        build_digests: provided,
    };
    let containerfile = ctx
        .expand(&cluster.containerfile)
        .context("expanding [cluster].containerfile")?;
    let context = ctx
        .expand(&cluster.context)
        .context("expanding [cluster].context")?;
    let platform = ctx
        .expand(&cluster.platform)
        .context("expanding [cluster].platform")?;
    let image = ctx.expand(&spec.image).context("expanding [build].image")?;

    let req = forge::build::ClusterBuildRequest {
        name: args.name.clone(),
        image,
        tag,
        containerfile,
        context,
        platform,
        git_url,
        git_ref,
        namespace,
        correlation_id,
        builder_image: env_or("FORGE_BUILDER_IMAGE", forge::build::DEFAULT_BUILDER_IMAGE),
        git_image: env_or("FORGE_GIT_IMAGE", forge::build::DEFAULT_GIT_IMAGE),
        ttl_seconds: forge::build::DEFAULT_TTL_SECONDS,
        timeout: spec.timeout.as_duration(),
        // Optional private-repo clone auth: a token file (never the value) so a private context repo
        // clones. Absent ⇒ anonymous clone (public repos unchanged).
        git_token_file: std::env::var_os("FORGE_GIT_TOKEN_FILE").map(std::path::PathBuf::from),
    };

    eprintln!(
        "==> dispatching cluster build {:?} -> {} (namespace {}, job {})",
        args.name,
        req.image_ref(),
        req.namespace,
        req.job_name()
    );
    let success = forge::build::dispatch_cluster(&req, &authfile)?;
    eprintln!("==> build succeeded; pinned digest:");
    println!("{}", success.digest_ref);
    Ok(())
}

/// Dispatch (or, with `--check`, only validate) a `github-actions` build: introspect the workflow's
/// declared inputs against the manifest's mapping, then expand the closed-vocabulary templates and
/// dispatch → correlate → poll → pin. The pushed tag is `spec.image:<tag>` regardless of which input
/// carries it, so the digest re-resolves deterministically from the registry.
fn run_github(
    args: &BuildArgs,
    spec: &BuildSpec,
    gh: &GithubBuild,
    provided: &BTreeMap<String, String>,
    tally: &mut OutputTally,
) -> Result<()> {
    let target = forge::github::OrgAllowlist::from_env()?.authorize(&gh.repo)?;
    // RFC-0001:C-OUTPUTS mediation point; `--check` only introspects, so it dispatches nothing.
    if !args.check {
        let repo = target.to_string();
        tally.admit(TOOL, OutputKind::WorkflowDispatch, Some(&repo))?;
    }
    let token = resolve_github_token()?;

    // Introspection validates the human-declared wiring against the workflow at the dispatched ref, and
    // rejects an unsupported digest source up-front so `--check` catches it before any dispatch.
    let digest = gh
        .outputs
        .as_ref()
        .map(|o| o.digest.clone())
        .unwrap_or_default();
    forge::github::preflight_workflow(
        &target,
        &gh.workflow,
        &gh.git_ref,
        &gh.inputs,
        &digest,
        &args.name,
        &token,
    )?;
    if args.check {
        return Ok(());
    }

    for need in &spec.needs {
        if !provided.contains_key(need) {
            return Err(BuildCliError::MissingDepDigest {
                build: args.name.clone(),
                dep: need.clone(),
            }
            .into());
        }
    }

    let correlation_id = forge::github::new_correlation_id();
    let tag = args
        .tag
        .clone()
        .unwrap_or_else(|| format!("crucible-build-{correlation_id}"));
    let image_ref = format!("{}:{}", spec.image, tag);

    // Closed template vocabulary (§2): sha = the dispatched ref, plus tag/image/correlation_id and any
    // dependency digest_refs supplied via --dep.
    let ctx = TemplateContext {
        sha: gh.git_ref.clone(),
        tag,
        image: spec.image.clone(),
        correlation_id: correlation_id.clone(),
        build_digests: provided.clone(),
    };
    let mut inputs = BTreeMap::new();
    for (key, template) in &gh.inputs {
        let value = ctx
            .expand(template)
            .with_context(|| format!("expanding [build.{}.github.inputs].{key}", args.name))?;
        inputs.insert(key.clone(), value);
    }

    let req = forge::github::GithubBuildRequest {
        name: args.name.clone(),
        repo: target,
        workflow: gh.workflow.clone(),
        git_ref: gh.git_ref.clone(),
        inputs,
        correlation_id,
        correlation: gh
            .outputs
            .as_ref()
            .map(|o| o.correlation)
            .unwrap_or_default(),
        digest,
        image_ref,
        timeout: spec.timeout.as_duration(),
    };

    let authfile = args.authfile.clone().or_else(default_authfile);
    let success = forge::github::dispatch_github(&req, &token, authfile.as_deref())?;
    eprintln!("==> build succeeded; pinned digest:");
    println!("{}", success.digest_ref);
    Ok(())
}

/// Resolve the GitHub token: `$GITHUB_TOKEN`, else the `gh auth token` output. Never logged.
fn resolve_github_token() -> Result<String> {
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let out = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("no $GITHUB_TOKEN set and running `gh auth token` failed (is gh installed?)")?;
    if !out.status.success() {
        return Err(BuildCliError::GhAuthFailed {
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        }
        .into());
    }
    let token = String::from_utf8(out.stdout)
        .context("`gh auth token` output is not utf8")?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(BuildCliError::GhAuthEmpty.into());
    }
    Ok(token)
}

/// The push authfile for the registry digest resolve, without erroring when absent (a public
/// destination resolves anonymously): `$REGISTRY_AUTH_FILE` if it exists, else `~/.docker/config.json`.
fn default_authfile() -> Option<PathBuf> {
    if let Ok(f) = std::env::var("REGISTRY_AUTH_FILE") {
        let p = PathBuf::from(f);
        if p.exists() {
            return Some(p);
        }
    }
    let p = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".docker/config.json");
    p.exists().then_some(p)
}

/// The manifest's clonable default repo: `(url, ref)` when `[repo]` names a url, else `None`.
type DefaultRepo = Option<(String, String)>;

/// What a dispatch reads out of the manifest: the declared builds, the clonable default repo (a
/// composite has no single `[repo]`, so `--context-ref` is required there), and the pack's
/// `[outputs]` declaration.
struct LoadedBuilds {
    builds: BTreeMap<String, BuildSpec>,
    default_repo: DefaultRepo,
    outputs: OutputsCfg,
}

fn load_builds(path: &Path) -> Result<LoadedBuilds> {
    if is_composite(path) {
        let m = CompositeManifest::load(path)?;
        Ok(LoadedBuilds {
            builds: m.build,
            default_repo: None,
            outputs: m.outputs,
        })
    } else {
        let m = Manifest::load(path)?;
        let default_repo = m.repo.url.clone().map(|url| {
            (
                url,
                m.repo.git_ref.clone().unwrap_or_else(|| "main".to_string()),
            )
        });
        Ok(LoadedBuilds {
            builds: m.build,
            default_repo,
            outputs: m.outputs,
        })
    }
}

/// The mediation point a refused dispatch is recorded against on the session log.
const TOOL: &str = "crucible build";

/// The pack's `[outputs]` folded onto the engine default table.
fn dispatch_bounds(loaded: &LoadedBuilds) -> RunBounds {
    let defaults = crate::manifest::outputs::default_targets(None, &loaded.builds);
    RunBounds::new(
        crate::manifest::outputs::resolve(&loaded.outputs, &defaults),
        BTreeMap::new(),
    )
}

/// This dispatch's tally, recording onto the session log the run projected
/// (`BROKER_SESSION_LOG`); a dispatch outside a run has none.
fn dispatch_tally(loaded: &LoadedBuilds) -> OutputTally {
    let session_log = std::env::var(ENV_SESSION_LOG)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    OutputTally::new(dispatch_bounds(loaded), session_log)
}

/// `--dep name=digest_ref` pairs into a map; errors on a malformed entry.
fn parse_deps(deps: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for d in deps {
        let (name, digest) = d
            .split_once('=')
            .with_context(|| format!("--dep must be name=digest_ref, got {d:?}"))?;
        if name.is_empty() || digest.is_empty() {
            return Err(BuildCliError::MalformedDep { arg: d.clone() }.into());
        }
        out.insert(name.to_string(), digest.to_string());
    }
    Ok(out)
}

/// Resolve the git source the Job clones: `--context-ref <url>[@<ref>]` wins, else the manifest's
/// `[repo]` url + ref. A local `[repo].path` (no url) can't be cloned in-cluster, so `--context-ref`
/// is required in that case.
fn resolve_context(flag: &Option<String>, default: &DefaultRepo) -> Result<(String, String)> {
    if let Some(cr) = flag {
        return Ok(split_context_ref(cr));
    }
    default.clone().context(
        "no --context-ref given and the manifest [repo] has no clonable url (a local [repo].path \
         can't be cloned by the build Job) — pass --context-ref <url>[@<ref>]",
    )
}

/// Split `<url>[@<ref>]` into `(url, ref)`. A trailing `@<seg>` is only a ref when `<seg>` is
/// unambiguous, a git ref (branch/tag/sha) contains no `/` or `:`, so a segment carrying either is
/// path/userinfo, not a ref. This keeps an scp-style URL (`git@github.com:o/r.git`, and the
/// `user@host/path` https form) from being mangled into `("git", "github.com:…")`. Defaults to `main`.
fn split_context_ref(cr: &str) -> (String, String) {
    if let Some((url, git_ref)) = cr.rsplit_once('@') {
        let looks_like_ref = !git_ref.is_empty() && !git_ref.contains(['/', ':']);
        if looks_like_ref && !url.is_empty() {
            return (url.to_string(), git_ref.to_string());
        }
    }
    (cr.to_string(), "main".to_string())
}

/// The push authfile: `--authfile` wins, else `$REGISTRY_AUTH_FILE`, else `~/.docker/config.json`.
fn resolve_authfile(flag: &Option<PathBuf>) -> Result<PathBuf> {
    if let Some(f) = flag {
        return Ok(f.clone());
    }
    if let Ok(f) = std::env::var("REGISTRY_AUTH_FILE") {
        let p = PathBuf::from(f);
        if p.exists() {
            return Ok(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let p = PathBuf::from(home).join(".docker/config.json");
    if p.exists() {
        return Ok(p);
    }
    Err(BuildCliError::NoAuthfile.into())
}

/// A short, unique-enough correlation id (low 32 bits of the nanosecond clock, hex). Uniquifies the
/// Job/secret names; the digest, not the tag, is what consumers pin.
fn correlation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:08x}", nanos & 0xffff_ffff)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_deps_reads_pairs_and_rejects_garbage() {
        let ok = parse_deps(&[
            "vllm=ghcr.io/x/vllm@sha256:aaa".to_string(),
            "base=ghcr.io/x/base@sha256:bbb".to_string(),
        ])
        .unwrap();
        assert_eq!(ok["vllm"], "ghcr.io/x/vllm@sha256:aaa");
        assert_eq!(ok["base"], "ghcr.io/x/base@sha256:bbb");
        assert!(parse_deps(&["no-equals".to_string()]).is_err());
        assert!(parse_deps(&["=nodigest".to_string()]).is_err());
        assert!(parse_deps(&["noname=".to_string()]).is_err());
    }

    #[test]
    fn resolve_context_prefers_the_flag_and_splits_the_ref() {
        let default = Some(("https://github.com/o/r".to_string(), "dev".to_string()));
        assert_eq!(
            resolve_context(
                &Some("https://github.com/a/b@feature".to_string()),
                &default
            )
            .unwrap(),
            ("https://github.com/a/b".to_string(), "feature".to_string())
        );
        // No @ref => default branch main.
        assert_eq!(
            resolve_context(&Some("https://github.com/a/b".to_string()), &None).unwrap(),
            ("https://github.com/a/b".to_string(), "main".to_string())
        );
        // No flag => the manifest repo.
        assert_eq!(
            resolve_context(&None, &default).unwrap(),
            ("https://github.com/o/r".to_string(), "dev".to_string())
        );
        // No flag and no clonable repo => error.
        assert!(resolve_context(&None, &None).is_err());
    }

    #[test]
    fn split_context_ref_handles_https_and_scp_forms() {
        // https with an explicit @ref.
        assert_eq!(
            split_context_ref("https://github.com/a/b@feature"),
            ("https://github.com/a/b".to_string(), "feature".to_string())
        );
        // scp-style URL with no explicit ref must NOT be mangled at the userinfo `@`.
        assert_eq!(
            split_context_ref("git@github.com:o/r.git"),
            ("git@github.com:o/r.git".to_string(), "main".to_string())
        );
        // scp-style URL WITH an explicit @ref.
        assert_eq!(
            split_context_ref("git@github.com:o/r.git@dev"),
            ("git@github.com:o/r.git".to_string(), "dev".to_string())
        );
        // https userinfo form (@ before the path) is not a ref split.
        assert_eq!(
            split_context_ref("https://user@host.com/o/r"),
            ("https://user@host.com/o/r".to_string(), "main".to_string())
        );
    }

    #[test]
    fn template_context_expands_cluster_fields_from_deps() {
        // The `--dep` digest is consumed to expand `{{ builds.<name>.digest_ref }}` in a cluster field.
        let provided = parse_deps(&["base=ghcr.io/x/base@sha256:abc".to_string()]).unwrap();
        let ctx = TemplateContext {
            sha: "main".into(),
            tag: "t1".into(),
            image: "ghcr.io/x/top".into(),
            correlation_id: "cid".into(),
            build_digests: provided,
        };
        assert_eq!(
            ctx.expand("{{ builds.base.digest_ref }}").unwrap(),
            "ghcr.io/x/base@sha256:abc"
        );
        assert_eq!(
            ctx.expand("{{ image }}:{{ tag }}").unwrap(),
            "ghcr.io/x/top:t1"
        );
    }

    #[test]
    fn correlation_id_is_hex_and_short() {
        let c = correlation_id();
        assert_eq!(c.len(), 8);
        assert!(c.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    /// A manifest whose single github build points at `o/r`, plus whatever `[outputs]` the case
    /// declares.
    fn loaded(outputs: &str) -> LoadedBuilds {
        let text = format!(
            r#"
            [repo]
            path = "."
            [agent]
            goal = "g"
            [judge]
            measure_cmd = "m"
            direction = "higher"
            [build.epp]
            backend = "github-actions"
            image = "ghcr.io/o/epp"
            [build.epp.github]
            repo = "o/r"
            workflow = "build.yml"
            {outputs}
        "#
        );
        let m: Manifest = toml::from_str(&text).expect("manifest parses");
        LoadedBuilds {
            builds: m.build,
            default_repo: None,
            outputs: m.outputs,
        }
    }

    #[test]
    fn a_dispatch_beyond_the_declared_count_is_refused() {
        let loaded =
            loaded("[outputs.workflow-dispatch]\ncount = 1\ntarget = { fixed = \"o/r\" }\n");
        let mut tally = OutputTally::new(dispatch_bounds(&loaded), None);
        assert!(
            tally
                .admit(TOOL, OutputKind::WorkflowDispatch, Some("o/r"))
                .is_ok()
        );
        let err = tally
            .admit(TOOL, OutputKind::WorkflowDispatch, Some("o/r"))
            .expect_err("the second dispatch is over the count");
        assert_eq!(err.bound(), "[outputs.workflow-dispatch].count = 1");
    }

    #[test]
    fn a_dispatch_outside_the_resolved_target_is_refused_naming_both() {
        let loaded = loaded("");
        let mut tally = OutputTally::new(dispatch_bounds(&loaded), None);
        let err = tally
            .admit(TOOL, OutputKind::WorkflowDispatch, Some("evil/r"))
            .expect_err("a repo outside the resolved target");
        let detail = err.to_string();
        assert!(detail.contains("evil/r"), "{detail}");
        assert!(detail.contains("o/r"), "{detail}");
        assert!(
            tally
                .admit(TOOL, OutputKind::WorkflowDispatch, Some("o/r"))
                .is_ok(),
            "the refused dispatch spent no budget"
        );
    }

    #[test]
    fn a_refused_dispatch_lands_on_the_session_log() {
        let dir =
            std::env::temp_dir().join(format!("crucible-build-bounds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let log = dir.join("session.jsonl");
        let _ = std::fs::remove_file(&log);
        let loaded =
            loaded("[outputs.workflow-dispatch]\ncount = 0\ntarget = { fixed = \"o/r\" }\n");
        let mut tally = OutputTally::new(dispatch_bounds(&loaded), Some(log.clone()));
        assert!(
            tally
                .admit(TOOL, OutputKind::WorkflowDispatch, Some("o/r"))
                .is_err()
        );
        let text = std::fs::read_to_string(&log).expect("the row landed");
        match crucible_contract::session::decode(text.trim()).expect("decodes") {
            crucible_contract::session::SessionEvent::OutputRefused {
                output_kind,
                bound,
                tool,
                ..
            } => {
                assert_eq!(output_kind, "workflow-dispatch");
                assert_eq!(tool, TOOL);
                assert_eq!(bound, "[outputs.workflow-dispatch].count = 0");
            }
            other => panic!("wrong event: {other:?}"),
        }
        let _ = std::fs::remove_file(&log);
    }
}
