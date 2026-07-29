//! The GithubActions build backend: a thin GitHub REST client for exactly the calls a
//! `workflow_dispatch` build needs, plus the workflow-file introspection that validates the manifest's
//! declared input mapping. A backend's only job is "build + push to the tag it was given"; crucible
//! resolves the digest itself from the registry via [`crate::oci::pin_digest`] (the single source of
//! truth for both backends), so a conforming workflow reports nothing back through logs or artifacts.
//!
//! ```text
//!   POST .../workflows/<file>/dispatches   (204, no run id)
//!        │  correlation_id rides an input, echoed into the run name
//!        ▼
//!   GET  .../workflows/<file>/runs?event=workflow_dispatch   → find OUR run
//!        ▼
//!   GET  .../actions/runs/<id>   poll until status=completed
//!        ├─ conclusion=success ─▶ pin_digest(image:tag) ─▶ image@sha256:…  (caller, off-runtime)
//!        └─ else               ─▶ Err(run url + conclusion)  → controller park evidence
//! ```
//!
//! **Correlation race caveat**: `RunName` (the default) is exact (a unique correlation id echoed
//! into the run name) and is skew- and concurrency-immune. `DispatchWindow` (the escape hatch for a
//! workflow whose run name we can't shape) takes the newest run created after our local dispatch
//! timestamp: it is only sound when dispatches to that workflow are serialized, and it assumes our
//! clock and GitHub's agree closely; it stays opt-in for exactly those reasons.
//!
//! The network functions stay deliberately thin so the decision logic (run-name matching,
//! dispatch-window selection, terminal-state mapping, and workflow-input introspection) is pure and
//! unit-tested without any I/O or a fake server.

use crate::build::BuildSuccess;
use crate::oci::{self, RETRY_DELAYS, with_retry};
use crate::spec::{CorrelationSource, DigestKind, DigestSource};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

const GITHUB_API: &str = "https://api.github.com";
/// The pinned REST API version header GitHub asks every request to send.
const API_VERSION: &str = "2022-11-28";
/// GitHub rejects requests without a User-Agent.
const USER_AGENT: &str = "crucible-forge";
/// How often we re-poll the runs list / a run's state.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// TCP connect cap for a single request; a stalled connect must not park the build forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-request wall cap. Well under a poll interval's expectations; the OVERALL build cap stays the
/// deadline logic in `correlate`/`poll_run`. A hung socket now errors (transient → `with_retry`) instead
/// of voiding the `GithubBuildRequest.timeout` guarantee by parking `.await` indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on a fetched workflow file, so an attacker-chosen repo can't hand us a giant/alias-bombed YAML
/// that exhausts memory before `serde_norway` even parses it. A real workflow is a few KB.
const MAX_WORKFLOW_BYTES: u64 = 1024 * 1024;
/// A bounded window (capped by the build timeout) to find our dispatched run before giving up. The
/// run should appear within seconds; if it never does, the dispatch didn't take.
const CORRELATION_WINDOW: Duration = Duration::from_secs(120);
/// Cap on an error body echoed into a message, so a giant HTML 500 page can't flood the log.
const ERR_BODY_CHARS: usize = 500;

/// A resolved GithubActions build: everything the dispatch needs with no manifest/template indirection
/// left. Built by the caller (the `crucible build` CLI in M2, the controller in M1) from the spec +
/// the template-expanded inputs.
#[derive(Debug, Clone)]
pub struct GithubBuildRequest {
    /// The `[build.<name>]` name, for messages only.
    pub name: String,
    /// `owner/repo` holding the build workflow.
    pub repo: String,
    /// The workflow file to dispatch (e.g. `crucible-build.yml`), resolved under `.github/workflows/`.
    pub workflow: String,
    /// The git ref to dispatch against (branch, tag, or SHA).
    pub git_ref: String,
    /// The `workflow_dispatch` inputs, already template-expanded; nothing else is forwarded.
    pub inputs: BTreeMap<String, String>,
    /// The opaque id we generated for this dispatch; the workflow echoes it into the run name so we can
    /// find our run (`RunName` correlation).
    pub correlation_id: String,
    /// How our dispatch is correlated to a run.
    pub correlation: CorrelationSource,
    /// How the pushed digest is discovered (default: re-resolve the pushed tag from the registry).
    pub digest: DigestSource,
    /// The full `registry/repo:tag` the workflow pushes: what [`crate::oci::pin_digest`] resolves.
    pub image_ref: String,
    /// Wall-clock cap on the whole dispatch→completion wait (from the spec's `timeout`).
    pub timeout: Duration,
}

/// A fresh random correlation id (uuid v4). `workflow_dispatch` returns 204 with no run id, so this
/// value, echoed into the run name, is how we find our run.
pub fn new_correlation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Dispatch a GithubActions build and drive it to a terminal state, then, on success, resolve the
/// digest-pinned ref from the registry. On a failed/cancelled/timed-out run, the error carries the run
/// URL + conclusion (the controller's park evidence). `authfile`, when given, supplies creds for a
/// private destination repo (same docker-config format buildah uses).
///
/// The digest is resolved in this sync wrapper, AFTER the async run finishes, because
/// [`crate::oci::pin_digest`] spins its own runtime; nesting it inside the dispatch runtime would
/// panic.
pub fn dispatch_github(
    req: &GithubBuildRequest,
    token: &str,
    authfile: Option<&Path>,
) -> Result<BuildSuccess> {
    ensure_repo(&req.repo)?;
    ensure_workflow_filename(&req.workflow)?;
    // Reject an unsupported digest source BEFORE dispatching, so we never waste a whole build only to
    // bail at digest resolution (and never wedge an M1 deployment slot on it).
    ensure_digest_supported(&req.name, &req.digest)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the github-dispatch runtime")?;
    rt.block_on(run_build(req, token))?;

    // Only the registry-tag path reaches here (`ensure_digest_supported` rejected the rest up-front).
    let digest_ref = oci::pin_digest(&req.image_ref, authfile)
        .with_context(|| format!("resolving the pushed digest for {}", req.image_ref))?;
    Ok(BuildSuccess {
        image_ref: req.image_ref.clone(),
        digest_ref,
    })
}

/// dispatch → correlate → poll to a terminal success. Digest resolution is the caller's job (see
/// [`dispatch_github`]).
async fn run_build(req: &GithubBuildRequest, token: &str) -> Result<()> {
    install_crypto_provider();
    let client = build_client()?;

    let dispatched_at = jiff::Timestamp::now();
    post_dispatch(&client, req, token).await?;
    eprintln!(
        "==> dispatched {} to {} (workflow {}, ref {}); correlating…",
        req.name, req.repo, req.workflow, req.git_ref
    );

    let start = Instant::now();
    let corr_deadline = start + req.timeout.min(CORRELATION_WINDOW);
    let overall_deadline = start + req.timeout;
    let run_id = correlate(&client, req, token, dispatched_at, corr_deadline).await?;
    eprintln!("==> correlated run {run_id}; polling to completion…");
    poll_run(&client, req, token, run_id, overall_deadline).await
}

/// Poll the runs list until we identify our dispatched run, honoring the (bounded) correlation window.
async fn correlate(
    client: &reqwest::Client,
    req: &GithubBuildRequest,
    token: &str,
    dispatched_at: jiff::Timestamp,
    deadline: Instant,
) -> Result<u64> {
    loop {
        let runs = with_retry("listing workflow runs", &RETRY_DELAYS, || async {
            list_workflow_runs(client, &req.repo, &req.workflow, token).await
        })
        .await?;

        let found = match req.correlation {
            CorrelationSource::RunName => runs
                .iter()
                .find(|r| run_name_matches(r, &req.correlation_id))
                .map(|r| r.id),
            CorrelationSource::DispatchWindow => {
                select_newest_after(&runs, dispatched_at).map(|r| r.id)
            }
        };
        if let Some(id) = found {
            return Ok(id);
        }
        if Instant::now() >= deadline {
            bail!(
                "no workflow run correlated to dispatch {:?} within {:?} (correlation = {:?}); the \
                 dispatch may have been rejected or the workflow doesn't echo the correlation id",
                req.correlation_id,
                req.timeout.min(CORRELATION_WINDOW),
                req.correlation
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Poll a run until it reaches a terminal state, honoring the build timeout. Success returns `Ok`; a
/// non-success conclusion (or the timeout) bails with the run URL as evidence.
async fn poll_run(
    client: &reqwest::Client,
    req: &GithubBuildRequest,
    token: &str,
    run_id: u64,
    deadline: Instant,
) -> Result<()> {
    loop {
        let run = with_retry(&format!("polling run {run_id}"), &RETRY_DELAYS, || async {
            get_run(client, &req.repo, run_id, token).await
        })
        .await?;

        match run_outcome(&run) {
            Some(RunOutcome::Success) => return Ok(()),
            Some(RunOutcome::Failure(conclusion)) => bail!(
                "build {:?}: workflow run {run_id} finished {conclusion:?} — see {}",
                req.name,
                run.html_url
            ),
            None => {}
        }
        if Instant::now() >= deadline {
            bail!(
                "build {:?}: workflow run {run_id} did not finish within {:?} (last status: {:?}) \
                 — see {}",
                req.name,
                req.timeout,
                run.status,
                run.html_url
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Fetch the workflow file at `git_ref`, introspect its `workflow_dispatch` inputs, and validate the
/// manifest's declared `mapped` input mapping against them. This is validation of a human-declared
/// mapping; it never invents mappings. The `crucible build --check` path runs exactly this.
fn validate_workflow(
    repo: &str,
    workflow: &str,
    git_ref: &str,
    mapped: &BTreeMap<String, String>,
    token: &str,
) -> Result<MappingReport> {
    ensure_repo(repo)?;
    ensure_workflow_filename(workflow)?;
    let yaml = fetch_workflow_content(repo, workflow, git_ref, token)?;
    let inputs = parse_workflow_inputs(&yaml)
        .with_context(|| format!("introspecting {workflow} in {repo} at ref {git_ref}"))?;
    Ok(validate_input_mapping(&inputs, mapped))
}

/// Run [`validate_workflow`], print any warnings to stderr, and fail on any error. Both the `--check`
/// path and the pre-dispatch preflight call this so a mis-wired manifest never reaches a dispatch.
pub fn preflight_workflow(
    repo: &str,
    workflow: &str,
    git_ref: &str,
    mapped: &BTreeMap<String, String>,
    digest: &DigestSource,
    name: &str,
    token: &str,
) -> Result<()> {
    // Reject an unsupported digest source first: cheap, no network, and it makes `--check` catch it
    // before any dispatch.
    ensure_digest_supported(name, digest)?;
    let report = validate_workflow(repo, workflow, git_ref, mapped, token)?;
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }
    if !report.errors.is_empty() {
        bail!(
            "workflow input validation failed for {workflow} in {repo} at ref {git_ref}:\n  - {}",
            report.errors.join("\n  - ")
        );
    }
    eprintln!("==> workflow inputs validate against {workflow} ({repo}@{git_ref})");
    Ok(())
}

// ─────────────────────────────── pure logic (unit-tested) ───────────────────────────────

/// A `workflow_dispatch` input as declared in the workflow YAML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowInput {
    name: String,
    /// `required:` (default false for a `workflow_dispatch` input).
    required: bool,
    /// `type:` (`string`/`boolean`/`number`/`choice`/`environment`); `None` when unspecified.
    declared_type: Option<String>,
}

/// The outcome of validating a declared input mapping. Errors are fatal (fail fast, §3); warnings go
/// to stderr and don't block.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MappingReport {
    errors: Vec<String>,
    warnings: Vec<String>,
}

/// Parse `on.workflow_dispatch.inputs` out of a workflow YAML. Handles the GitHub Actions gotcha that
/// YAML 1.1 parses the bare `on:` key as the boolean `true` (some parsers do this), by looking up both
/// the string key and the boolean key. `on` may itself be a scalar (`on: workflow_dispatch`), a sequence
/// (`on: [push, workflow_dispatch]`), or a mapping; all three are valid + dispatchable; only the mapping
/// form can carry inputs, the other two yield an empty input list. Errors only when no `workflow_dispatch`
/// trigger appears in any form (the workflow isn't dispatchable); an empty/absent `inputs` is no inputs.
/// Read a YAML boolean that may be a real bool OR a quoted string (`required: "true"`). `Value::as_bool`
/// returns `None` for the string form, so a quoted required flag would otherwise silently become
/// optional and a correlation-critical missing mapping would slip past validation.
fn parse_yaml_bool(v: &serde_norway::Value) -> Option<bool> {
    if let Some(b) = v.as_bool() {
        return Some(b);
    }
    match v.as_str()?.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_workflow_inputs(yaml: &str) -> Result<Vec<WorkflowInput>> {
    use serde_norway::Value;
    let doc: Value = serde_norway::from_str(yaml).context("parsing workflow YAML")?;

    let on = doc
        .get("on")
        .or_else(|| doc.get(Value::Bool(true)))
        .context("workflow has no `on:` trigger block")?;

    let not_dispatchable = "workflow has no `on.workflow_dispatch` — it isn't dispatchable";
    let wd = match on {
        // `on: workflow_dispatch`: dispatchable, no inputs.
        Value::String(s) if s == "workflow_dispatch" => return Ok(Vec::new()),
        Value::String(_) => bail!("{not_dispatchable}"),
        // `on: [push, workflow_dispatch]`: dispatchable iff the trigger is in the list, no inputs.
        Value::Sequence(triggers) => {
            if triggers
                .iter()
                .any(|t| t.as_str() == Some("workflow_dispatch"))
            {
                return Ok(Vec::new());
            }
            bail!("{not_dispatchable}");
        }
        // `on: { workflow_dispatch: … }`: the only form that can carry inputs.
        _ => on.get("workflow_dispatch").context(not_dispatchable)?,
    };

    // `workflow_dispatch:` may be null (dispatchable, no inputs) or a mapping possibly without inputs.
    let inputs = match wd.get("inputs") {
        Some(v) if !v.is_null() => v
            .as_mapping()
            .context("on.workflow_dispatch.inputs is not a mapping")?,
        _ => return Ok(Vec::new()),
    };

    let mut out = Vec::new();
    for (key, val) in inputs {
        let name = key
            .as_str()
            .context("a workflow_dispatch input name is not a string")?
            .to_string();
        let required = val
            .get("required")
            .and_then(parse_yaml_bool)
            .unwrap_or(false);
        let declared_type = val.get("type").and_then(Value::as_str).map(str::to_string);
        out.push(WorkflowInput {
            name,
            required,
            declared_type,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Validate a manifest's declared input mapping against the workflow's introspected inputs (§3): FAIL
/// on a required workflow input we don't provide, FAIL on a mapped input the workflow doesn't declare,
/// WARN on a non-string-shaped input (we always forward a template-expanded string). Never auto-maps.
fn validate_input_mapping(
    declared: &[WorkflowInput],
    mapped: &BTreeMap<String, String>,
) -> MappingReport {
    let mut report = MappingReport::default();
    let declared_by_name: BTreeMap<&str, &WorkflowInput> =
        declared.iter().map(|i| (i.name.as_str(), i)).collect();

    for input in declared {
        if input.required && !mapped.contains_key(&input.name) {
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
                if let Some(t) = &input.declared_type {
                    // We forward strings; a boolean/number input expects a coercible value, so flag it.
                    if !matches!(t.as_str(), "string" | "choice" | "environment") {
                        report.warnings.push(format!(
                            "workflow input {key:?} is declared type {t:?}; crucible forwards a \
                             template-expanded string, which the workflow must coerce"
                        ));
                    }
                }
            }
        }
    }
    report
}

/// Does a run's name carry our correlation id? (`RunName` correlation: the run name echoes the id.)
fn run_name_matches(run: &WorkflowRun, correlation_id: &str) -> bool {
    run.name
        .as_deref()
        .map(|n| n.contains(correlation_id))
        .unwrap_or(false)
}

/// The newest run created at/after `after` (`DispatchWindow` correlation). Runs whose `created_at`
/// doesn't parse are skipped rather than mis-selected.
fn select_newest_after(runs: &[WorkflowRun], after: jiff::Timestamp) -> Option<&WorkflowRun> {
    runs.iter()
        .filter_map(|r| {
            r.created_at
                .parse::<jiff::Timestamp>()
                .ok()
                .map(|ts| (ts, r))
        })
        .filter(|(ts, _)| *ts >= after)
        .max_by_key(|(ts, _)| *ts)
        .map(|(_, r)| r)
}

/// A run's terminal outcome, or `None` while it's still running. GitHub reports terminal-ness via
/// `status = "completed"`; `conclusion = "success"` is the only pass, everything else
/// (failure/cancelled/timed_out/…) is a fail carrying its conclusion.
fn run_outcome(run: &WorkflowRun) -> Option<RunOutcome> {
    if run.status.as_deref() != Some("completed") {
        return None;
    }
    match run.conclusion.as_deref() {
        Some("success") => Some(RunOutcome::Success),
        other => Some(RunOutcome::Failure(other.unwrap_or("unknown").to_string())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunOutcome {
    Success,
    Failure(String),
}

/// `owner/repo` sanity check: exactly one slash, both halves non-empty. A malformed repo would
/// silently build a wrong URL otherwise.
fn ensure_repo(repo: &str) -> Result<()> {
    match repo.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok(())
        }
        _ => bail!("github repo {repo:?} must be \"owner/repo\""),
    }
}

/// `workflow` must be a bare workflow filename (`[A-Za-z0-9._-]+\.ya?ml`, no path separators, no `..`)
/// before it's interpolated into an API URL. Defense-in-depth: the URL structure already contains it,
/// but a stray `/` or `..` has no business in a workflow file name and we fail closed rather than build a
/// surprising URL.
fn ensure_workflow_filename(workflow: &str) -> Result<()> {
    let stem = workflow
        .strip_suffix(".yml")
        .or_else(|| workflow.strip_suffix(".yaml"));
    let valid = match stem {
        Some(stem) => {
            !stem.is_empty()
                && !workflow.contains("..")
                && workflow
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        }
        None => false,
    };
    if valid {
        Ok(())
    } else {
        bail!(
            "github workflow {workflow:?} must be a workflow filename like \"crucible-build.yml\" \
             ([A-Za-z0-9._-]+.ya?ml, no path separators or \"..\")"
        )
    }
}

/// Reject a digest source forge can't honor in M2 up-front, so `--check`/preflight catches it and no
/// build is ever wasted (the artifact form parses fine but is unimplemented). Only the registry-tag
/// path (the default and the single source of truth) is supported.
fn ensure_digest_supported(name: &str, digest: &DigestSource) -> Result<()> {
    match digest {
        DigestSource::Kind(DigestKind::RegistryTag) => Ok(()),
        DigestSource::Artifact {
            name: artifact_name,
            path,
            ..
        } => bail!(
            "build {name:?}: digest source `artifact` (name={artifact_name:?}, path={path:?}) is not \
             implemented — the registry-tag path is the default and the single source of \
             truth; declare `digest = \"registry-tag\"` or leave [outputs] unset"
        ),
    }
}

fn dispatch_url(repo: &str, workflow: &str) -> String {
    format!("{GITHUB_API}/repos/{repo}/actions/workflows/{workflow}/dispatches")
}
fn workflow_runs_url(repo: &str, workflow: &str) -> String {
    format!("{GITHUB_API}/repos/{repo}/actions/workflows/{workflow}/runs")
}
fn run_url(repo: &str, run_id: u64) -> String {
    format!("{GITHUB_API}/repos/{repo}/actions/runs/{run_id}")
}
fn contents_url(repo: &str, workflow: &str) -> String {
    format!("{GITHUB_API}/repos/{repo}/contents/.github/workflows/{workflow}")
}

// ─────────────────────────────── network (thin) ───────────────────────────────

/// GitHub's runs-list response (only the fields we consume).
#[derive(Debug, Clone, Deserialize)]
struct WorkflowRunsResponse {
    workflow_runs: Vec<WorkflowRun>,
}

/// One Actions run (only the fields we consume). `name` is the display run name (carries the echoed
/// correlation id); `created_at` is RFC 3339.
#[derive(Debug, Clone, Deserialize)]
struct WorkflowRun {
    id: u64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    html_url: String,
}

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building the github http client")
}

/// rustls 0.23 won't auto-pick a CryptoProvider when several are linked; choose one before any TLS
/// handshake. Idempotent: a second install just returns Err, ignored (the codebase-wide pattern).
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

async fn post_dispatch(
    client: &reqwest::Client,
    req: &GithubBuildRequest,
    token: &str,
) -> Result<()> {
    let url = dispatch_url(&req.repo, &req.workflow);
    let body = serde_json::json!({ "ref": req.git_ref, "inputs": req.inputs });
    // Retry only absorbs transport errors (send() Err): a 4xx/5xx is an Ok(Response) we inspect below,
    // and a re-dispatch of an already-built context is an idempotent no-op push.
    let resp = with_retry("dispatching the workflow", &RETRY_DELAYS, || async {
        gh_get_or_post(client.post(&url), token)
            .json(&body)
            .send()
            .await
            .map_err(anyhow::Error::from)
    })
    .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "workflow_dispatch to {url} failed: HTTP {status}: {}",
            truncate(&text, ERR_BODY_CHARS)
        );
    }
    Ok(())
}

async fn list_workflow_runs(
    client: &reqwest::Client,
    repo: &str,
    workflow: &str,
    token: &str,
) -> Result<Vec<WorkflowRun>> {
    let url = workflow_runs_url(repo, workflow);
    let resp = gh_get_or_post(client.get(&url), token)
        .query(&[("event", "workflow_dispatch"), ("per_page", "30")])
        .send()
        .await
        .context("listing workflow runs")?
        .error_for_status()
        .context("listing workflow runs")?;
    let parsed: WorkflowRunsResponse = resp
        .json()
        .await
        .context("parsing the workflow-runs response")?;
    Ok(parsed.workflow_runs)
}

async fn get_run(
    client: &reqwest::Client,
    repo: &str,
    run_id: u64,
    token: &str,
) -> Result<WorkflowRun> {
    let url = run_url(repo, run_id);
    let resp = gh_get_or_post(client.get(&url), token)
        .send()
        .await
        .with_context(|| format!("fetching run {run_id}"))?
        .error_for_status()
        .with_context(|| format!("fetching run {run_id}"))?;
    resp.json()
        .await
        .with_context(|| format!("parsing run {run_id}"))
}

/// Blocking fetch of a workflow file's raw text via the contents API (its own runtime; called only
/// from the sync `--check`/preflight path, never inside the dispatch runtime).
fn fetch_workflow_content(
    repo: &str,
    workflow: &str,
    git_ref: &str,
    token: &str,
) -> Result<String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the workflow-fetch runtime")?;
    rt.block_on(async {
        install_crypto_provider();
        let client = build_client()?;
        let url = contents_url(repo, workflow);
        let resp = gh_get_or_post(client.get(&url), token)
            // Raw media type: the response body IS the file, no base64 wrapper to decode.
            .header(reqwest::header::ACCEPT, "application/vnd.github.raw")
            .query(&[("ref", git_ref)])
            .send()
            .await
            .context("fetching the workflow file")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!(".github/workflows/{workflow} not found in {repo} at ref {git_ref}");
        }
        let mut resp = resp
            .error_for_status()
            .context("fetching the workflow file")?;
        // Reject an over-cap body before buffering it: check the advertised length first (cheap), then
        // stream chunk-by-chunk so a lying/absent Content-Length can't slip a huge body past the cap.
        if let Some(len) = resp.content_length()
            && len > MAX_WORKFLOW_BYTES {
                bail!(
                    ".github/workflows/{workflow} in {repo} is {len} bytes, over the \
                     {MAX_WORKFLOW_BYTES}-byte cap"
                );
            }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .context("reading the workflow file body")?
        {
            if buf.len() as u64 + chunk.len() as u64 > MAX_WORKFLOW_BYTES {
                bail!(
                    ".github/workflows/{workflow} in {repo} exceeds the {MAX_WORKFLOW_BYTES}-byte cap"
                );
            }
            buf.extend_from_slice(&chunk);
        }
        String::from_utf8(buf).context("workflow file is not utf8")
    })
}

/// Attach the shared GitHub headers (bearer token, api version, json accept) to a request builder. The
/// token is never logged. The contents fetch overrides `Accept` with the raw media type afterward.
fn gh_get_or_post(rb: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    rb.bearer_auth(token)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
}

/// Truncate `s` to at most `max` chars, appending an ellipsis when it was cut (byte-safe on char
/// boundaries).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real contract-v1 workflow. Introspection tests parse THIS file so the contract can't drift
    /// out from under the backend.
    const CONTRACT_V1: &str = include_str!("../../.github/workflows/crucible-build.yml");

    fn run(
        name: Option<&str>,
        status: &str,
        conclusion: Option<&str>,
        created: &str,
    ) -> WorkflowRun {
        WorkflowRun {
            id: 1,
            name: name.map(str::to_string),
            status: Some(status.to_string()),
            conclusion: conclusion.map(str::to_string),
            created_at: created.to_string(),
            html_url: "https://github.com/o/r/actions/runs/1".to_string(),
        }
    }

    #[test]
    fn run_name_matches_on_substring() {
        let r = run(
            Some("crucible-build 550e8400-e29b-41d4-a716-446655440000"),
            "completed",
            Some("success"),
            "2026-07-08T00:00:00Z",
        );
        assert!(run_name_matches(&r, "550e8400-e29b-41d4-a716-446655440000"));
        assert!(!run_name_matches(
            &r,
            "deadbeef-0000-0000-0000-000000000000"
        ));
        // A run with no name never matches.
        let nameless = WorkflowRun {
            name: None,
            ..r.clone()
        };
        assert!(!run_name_matches(
            &nameless,
            "550e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn select_newest_after_picks_the_latest_in_window() {
        let after: jiff::Timestamp = "2026-07-08T12:00:00Z".parse().unwrap();
        let mut older = run(None, "in_progress", None, "2026-07-08T11:59:59Z");
        older.id = 10;
        let mut newer = run(None, "in_progress", None, "2026-07-08T12:00:05Z");
        newer.id = 11;
        let mut newest = run(None, "in_progress", None, "2026-07-08T12:00:30Z");
        newest.id = 12;
        // Unsorted input; the one before `after` is excluded; the max after it wins.
        let runs = vec![newer, older, newest];
        assert_eq!(select_newest_after(&runs, after).map(|r| r.id), Some(12));

        // Nothing after the window => None (a stale runs list right after dispatch).
        let before: jiff::Timestamp = "2026-07-08T12:00:31Z".parse().unwrap();
        assert!(select_newest_after(&runs, before).is_none());
    }

    #[test]
    fn run_outcome_maps_terminal_states() {
        assert_eq!(
            run_outcome(&run(None, "completed", Some("success"), "")),
            Some(RunOutcome::Success)
        );
        assert_eq!(
            run_outcome(&run(None, "completed", Some("failure"), "")),
            Some(RunOutcome::Failure("failure".into()))
        );
        assert_eq!(
            run_outcome(&run(None, "completed", Some("timed_out"), "")),
            Some(RunOutcome::Failure("timed_out".into()))
        );
        // A completed run missing a conclusion is still a (non-success) failure rather than a hang.
        assert_eq!(
            run_outcome(&run(None, "completed", None, "")),
            Some(RunOutcome::Failure("unknown".into()))
        );
        // Not terminal yet.
        assert_eq!(run_outcome(&run(None, "in_progress", None, "")), None);
        assert_eq!(run_outcome(&run(None, "queued", None, "")), None);
    }

    #[test]
    fn introspects_the_contract_v1_workflow() {
        // The `on:` key parses even though YAML 1.1 might read it as boolean true.
        let inputs = parse_workflow_inputs(CONTRACT_V1).unwrap();
        let names: Vec<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["containerfile", "context-ref", "correlation_id", "image"]
        );
        assert!(inputs.iter().all(|i| i.required), "all v1 inputs required");
        assert!(
            inputs
                .iter()
                .all(|i| i.declared_type.as_deref() == Some("string"))
        );
    }

    #[test]
    fn introspects_a_workflow_dispatch_with_no_inputs() {
        let yaml = "on:\n  workflow_dispatch:\njobs:\n  b:\n    runs-on: ubuntu-latest\n";
        assert_eq!(parse_workflow_inputs(yaml).unwrap(), Vec::new());
    }

    #[test]
    fn introspection_errors_when_not_dispatchable() {
        let yaml = "on:\n  push:\n    branches: [main]\njobs: {}\n";
        let err = parse_workflow_inputs(yaml).unwrap_err().to_string();
        assert!(err.contains("workflow_dispatch"), "{err}");
    }

    #[test]
    fn introspection_defaults_required_false_and_reads_type() {
        let yaml = "\
on:
  workflow_dispatch:
    inputs:
      verbose:
        type: boolean
      tag:
        description: a tag
        required: true
";
        let inputs = parse_workflow_inputs(yaml).unwrap();
        let verbose = inputs.iter().find(|i| i.name == "verbose").unwrap();
        assert!(!verbose.required, "required defaults to false");
        assert_eq!(verbose.declared_type.as_deref(), Some("boolean"));
        let tag = inputs.iter().find(|i| i.name == "tag").unwrap();
        assert!(tag.required);
        assert_eq!(tag.declared_type, None);
    }

    #[test]
    fn mapping_validation_passes_a_complete_contract_v1_mapping() {
        let inputs = parse_workflow_inputs(CONTRACT_V1).unwrap();
        let mapped = BTreeMap::from([
            ("containerfile".into(), "Containerfile".into()),
            ("context-ref".into(), "{{ sha }}".into()),
            ("image".into(), "{{ image }}:{{ tag }}".into()),
            ("correlation_id".into(), "{{ correlation_id }}".into()),
        ]);
        let report = validate_input_mapping(&inputs, &mapped);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn mapping_validation_fails_on_a_missing_required_input() {
        let inputs = parse_workflow_inputs(CONTRACT_V1).unwrap();
        // Omit the required correlation_id; the run couldn't be correlated.
        let mapped = BTreeMap::from([
            ("containerfile".into(), "Containerfile".into()),
            ("context-ref".into(), "{{ sha }}".into()),
            ("image".into(), "{{ image }}:{{ tag }}".into()),
        ]);
        let report = validate_input_mapping(&inputs, &mapped);
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            report.errors[0].contains("correlation_id"),
            "{:?}",
            report.errors
        );
        assert!(report.errors[0].contains("required"));
    }

    #[test]
    fn mapping_validation_fails_on_an_undeclared_input() {
        let inputs = parse_workflow_inputs(CONTRACT_V1).unwrap();
        let mapped = BTreeMap::from([
            ("containerfile".into(), "Containerfile".into()),
            ("context-ref".into(), "{{ sha }}".into()),
            ("image".into(), "{{ image }}:{{ tag }}".into()),
            ("correlation_id".into(), "{{ correlation_id }}".into()),
            ("bogus".into(), "x".into()),
        ]);
        let report = validate_input_mapping(&inputs, &mapped);
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
            WorkflowInput {
                name: "flag".into(),
                required: false,
                declared_type: Some("boolean".into()),
            },
            WorkflowInput {
                name: "mode".into(),
                required: false,
                declared_type: Some("choice".into()),
            },
        ];
        let mapped = BTreeMap::from([
            ("flag".into(), "{{ tag }}".into()),
            ("mode".into(), "fast".into()),
        ]);
        let report = validate_input_mapping(&inputs, &mapped);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        // boolean warns (we send a string); choice does not.
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("flag"));
    }

    #[test]
    fn ensure_repo_requires_owner_and_name() {
        assert!(ensure_repo("neuralmagic/crucible").is_ok());
        assert!(ensure_repo("crucible").is_err());
        assert!(ensure_repo("/crucible").is_err());
        assert!(ensure_repo("neuralmagic/").is_err());
        assert!(ensure_repo("a/b/c").is_err());
    }

    #[test]
    fn urls_are_constructed_per_the_rest_api() {
        assert_eq!(
            dispatch_url("neuralmagic/crucible", "crucible-build.yml"),
            "https://api.github.com/repos/neuralmagic/crucible/actions/workflows/crucible-build.yml/dispatches"
        );
        assert_eq!(
            workflow_runs_url("neuralmagic/crucible", "crucible-build.yml"),
            "https://api.github.com/repos/neuralmagic/crucible/actions/workflows/crucible-build.yml/runs"
        );
        assert_eq!(
            run_url("neuralmagic/crucible", 42),
            "https://api.github.com/repos/neuralmagic/crucible/actions/runs/42"
        );
        assert_eq!(
            contents_url("neuralmagic/crucible", "crucible-build.yml"),
            "https://api.github.com/repos/neuralmagic/crucible/contents/.github/workflows/crucible-build.yml"
        );
    }

    #[test]
    fn correlation_ids_are_unique_uuids() {
        let a = new_correlation_id();
        let b = new_correlation_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36, "uuid v4 hyphenated form");
    }

    #[test]
    fn truncate_caps_long_bodies() {
        assert_eq!(truncate("short", 500), "short");
        let long = "x".repeat(600);
        let t = truncate(&long, 500);
        assert_eq!(t.chars().count(), 501, "500 chars + ellipsis");
        assert!(t.ends_with('…'));
    }

    #[test]
    fn introspects_scalar_on_workflow_dispatch() {
        // `on: workflow_dispatch` (scalar) is dispatchable and carries no inputs.
        let yaml = "on: workflow_dispatch\njobs:\n  b:\n    runs-on: ubuntu-latest\n";
        assert_eq!(parse_workflow_inputs(yaml).unwrap(), Vec::new());
    }

    #[test]
    fn introspects_sequence_on_with_workflow_dispatch() {
        // `on: [push, workflow_dispatch]` (sequence) is dispatchable and carries no inputs.
        let yaml = "on: [push, workflow_dispatch]\njobs:\n  b:\n    runs-on: ubuntu-latest\n";
        assert_eq!(parse_workflow_inputs(yaml).unwrap(), Vec::new());
    }

    #[test]
    fn introspection_rejects_scalar_and_sequence_without_workflow_dispatch() {
        // A scalar trigger that isn't workflow_dispatch is genuinely non-dispatchable.
        let scalar = "on: push\njobs: {}\n";
        let err = parse_workflow_inputs(scalar).unwrap_err().to_string();
        assert!(err.contains("workflow_dispatch"), "{err}");
        // A sequence with no workflow_dispatch entry is likewise non-dispatchable.
        let seq = "on: [push, pull_request]\njobs: {}\n";
        let err = parse_workflow_inputs(seq).unwrap_err().to_string();
        assert!(err.contains("workflow_dispatch"), "{err}");
    }

    #[test]
    fn introspection_reads_quoted_required_flag() {
        // A quoted `required: "true"` must not silently become optional.
        let yaml = "\
on:
  workflow_dispatch:
    inputs:
      tag:
        required: \"true\"
      maybe:
        required: \"false\"
      plain:
        required: true
";
        let inputs = parse_workflow_inputs(yaml).unwrap();
        let tag = inputs.iter().find(|i| i.name == "tag").unwrap();
        assert!(tag.required, "quoted \"true\" is required");
        let maybe = inputs.iter().find(|i| i.name == "maybe").unwrap();
        assert!(!maybe.required, "quoted \"false\" is optional");
        let plain = inputs.iter().find(|i| i.name == "plain").unwrap();
        assert!(plain.required, "bare true is required");
    }

    #[test]
    fn parse_yaml_bool_reads_real_and_stringly_booleans() {
        use serde_norway::Value;
        assert_eq!(parse_yaml_bool(&Value::Bool(true)), Some(true));
        assert_eq!(parse_yaml_bool(&Value::String("true".into())), Some(true));
        assert_eq!(parse_yaml_bool(&Value::String("TRUE".into())), Some(true));
        assert_eq!(parse_yaml_bool(&Value::String("false".into())), Some(false));
        assert_eq!(parse_yaml_bool(&Value::String("yes".into())), None);
        assert_eq!(parse_yaml_bool(&Value::Null), None);
    }

    #[test]
    fn ensure_workflow_filename_accepts_and_rejects() {
        assert!(ensure_workflow_filename("crucible-build.yml").is_ok());
        assert!(ensure_workflow_filename("build.yaml").is_ok());
        assert!(ensure_workflow_filename("ci_1.2.yml").is_ok());
        // No extension, path separators, traversal, or empty stem.
        assert!(ensure_workflow_filename("crucible-build").is_err());
        assert!(ensure_workflow_filename("crucible-build.txt").is_err());
        assert!(ensure_workflow_filename("dir/build.yml").is_err());
        assert!(ensure_workflow_filename("../secrets.yml").is_err());
        assert!(ensure_workflow_filename(".yml").is_err());
        assert!(ensure_workflow_filename("build .yml").is_err());
        assert!(ensure_workflow_filename("").is_err());
    }

    #[test]
    fn ensure_digest_supported_rejects_artifact() {
        use crate::spec::ArtifactMarker;
        // The default registry-tag path is fine.
        assert!(ensure_digest_supported("b", &DigestSource::default()).is_ok());
        // The artifact form is rejected up-front (so preflight/--check catches it, no wasted dispatch).
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
    fn build_client_sets_timeouts() {
        // reqwest doesn't expose its timeout config, so we assert the client builds and the timeout
        // constants are sane: a bounded connect under a bounded per-request cap, both non-zero.
        assert!(build_client().is_ok());
        assert!(CONNECT_TIMEOUT > Duration::ZERO);
        assert!(REQUEST_TIMEOUT > Duration::ZERO);
        assert!(CONNECT_TIMEOUT <= REQUEST_TIMEOUT);
    }
}
