//! Publish-on-keep: durable artifacts from an ephemeral loop pod. Two best-effort channels
//! (a publish failure logs but never fails the run, and runs even on interrupt/budget-stop):
//!
//!   * **S3, always.** Session log, results, summary, and per-iter diffs under
//!     `runs/<goal-slug>/<run_id>/`, plus `index/<run_id>.json` and a per-goal `latest.json`.
//!     Creds are IRSA web-identity (`AWS_ROLE_ARN` + the projected token).
//!   * **draft PR, keep only.** Each kept candidate's commits pushed as
//!     `autoresearch/<goal-slug>/<kept-sha12>` with the pristine base pinned as `…-base`, and a
//!     draft PR opened between them via `gh` (PAT in `AUTORESEARCH_PR_TOKEN`/`GITHUB_TOKEN`).
//!     Pinning the base branch makes each PR's diff exactly that candidate's commits. Branches
//!     key on the kept commit sha (not the run id) so a resume replaying the finish path maps
//!     the same kept diff to the same branch, and the already-published set from the session
//!     log turns the republish into a no-op instead of a duplicate PR.
//!
//! `run_id` (`<YYYYMMDDTHHMMSSZ>-<goal-slug>`) is the join key for the S3 prefix and the
//! PR↔S3 cross-reference. Time-first so keys sort chronologically.

use crate::reporter::{Reporter, Row};
use crate::{Args, Paths};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A run identity: `<YYYYMMDDTHHMMSSZ>-<goal-slug>`. Time-first so a plain lexical
/// sort of run ids (or S3 keys) is chronological across all goals, and the date is
/// human-readable. The branch name and the PR↔S3 join key derive from this too.
pub fn run_id(goal: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}-{}", utc_stamp(secs), slug(goal))
}

/// Format a Unix timestamp as compact UTC `YYYYMMDDTHHMMSSZ` via jiff. Falls back to
/// the epoch on the (impossible-for-real-clocks) out-of-range error so a run id is
/// always produced.
fn utc_stamp(epoch_secs: u64) -> String {
    jiff::Timestamp::from_second(epoch_secs as i64)
        .map(|ts| ts.strftime("%Y%m%dT%H%M%SZ").to_string())
        .unwrap_or_else(|_| format!("epoch{epoch_secs}"))
}

/// First non-empty goal line reduced to a short kebab token (alnum runs joined by
/// dashes), so it's safe in an S3 key and a git ref. Falls back to `run`.
fn slug(goal: &str) -> String {
    let line = goal
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("run")
        .trim_start_matches('#')
        .trim();
    let mut s = String::new();
    let mut prev_dash = false;
    for c in line.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !s.is_empty() && !prev_dash {
            s.push('-');
            prev_dash = true;
        }
        if s.len() >= 40 {
            break;
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "run".into() } else { s }
}

/// One resolved per-component publish target for a composite run: a component's checkout plus the
/// base/head shas (from the snapshot tokens) and the fork it PRs against (from the manifest). Built by
/// [`composite_targets`] zipping the world's [`crate::crucible::PublishComponent`]s with the manifest
/// fork map; an empty list means "single-repo run" (the `base_sha`/`kept_shas` path handles it).
pub struct PublishTarget {
    pub name: String,
    pub workspace: std::path::PathBuf,
    pub base_sha: String,
    pub head_sha: String,
    pub repo: String,
}

/// Join the world's per-component publish set with the manifest's `(name, owner/repo)` fork map: a
/// component with no fork mapping is dropped (it doesn't get a PR). Order follows the world's components.
pub fn composite_targets(
    components: Vec<crate::crucible::PublishComponent>,
    repos: &[(String, String)],
) -> Vec<PublishTarget> {
    components
        .into_iter()
        .filter_map(|c| {
            repos
                .iter()
                .find(|(n, _)| *n == c.name)
                .map(|(_, repo)| PublishTarget {
                    name: c.name,
                    workspace: c.workspace,
                    base_sha: c.base_sha,
                    head_sha: c.head_sha,
                    repo: repo.clone(),
                })
        })
        .collect()
}

/// Everything a publish needs from the finished run. Borrowed: the loop owns it all.
pub struct Record<'a> {
    pub args: &'a Args,
    pub paths: &'a Paths,
    pub run_id: &'a str,
    pub goal: &'a str,
    pub model: &'a str,
    pub gate: String,
    pub rows: &'a [Row],
    pub baseline_score: f64,
    pub best_score: f64,
    pub improved: bool,
    /// SHAs of the kept commits, newest last (for `summary.json`).
    pub kept_shas: &'a [String],
    /// The pristine upstream SHA the workspace was checked out at (segment 0's baseline, before any
    /// agent commit). The true PR base: the draft PR is opened with a base branch pinned here so its
    /// diff is exactly the kept commits, pristine-relative. `None` on resume (recorded by the original
    /// run), the PR open then falls back to the configured upstream ref.
    pub base_sha: Option<&'a str>,
    /// Composite multi-fork publish targets: one per touched component, each opening a cross-linked
    /// draft PR against its fork. Empty for a single-repo run (the `base_sha`/`kept_shas` single-PR
    /// path runs instead). Built by [`composite_targets`].
    pub components: &'a [PublishTarget],
    /// Head branches this run (or the segments it resumed) already opened PRs from, restored from
    /// the session log's `pr_links` events. A candidate whose branch is in here is already
    /// published: skip it instead of opening a duplicate PR.
    pub published_branches: &'a [String],
    pub cost_usd: f64,
    pub elapsed: Duration,
    /// The run's comparability key ([`crate::identity::RunIdentity::digest`]), so a
    /// report/leaderboard can group or gate runs by world identity without re-deriving it.
    pub identity_digest: &'a str,
    /// Content hash of the pack-declared `[agent].seed_diff` iteration 1 was handed
    /// ([`crate::identity::RunIdentity::seed_hash`]). Empty = an unseeded run; non-empty makes
    /// the PR disclose the seeding (run 6 published a hand-steered diff with no disclosure).
    pub seed_hash: &'a str,
}

/// Best-effort publish. Logs progress/failures through `r` and never returns an
/// error: S3 failure degrades to "results on the deployment only"; a push failure is loud
/// (the deliverable didn't ship) but the kept change is still on the deployment + in S3.
/// Returns whatever draft PRs opened (empty when nothing was kept, no PR repo is
/// configured, or the open failed), the caller uses these to auto-spawn a feedback
/// watcher (`--watch-feedback`).
pub fn publish<R: Reporter>(r: &mut R, rec: &Record<'_>) -> Vec<PrLink> {
    // PRs open BEFORE the S3 record so their URLs land in `summary.json` (the report deep-links to
    // them). S3 still runs unconditionally afterwards, so the "the run happened" record ships even
    // when the PR step is skipped or fails, only now it's enriched with whatever PRs opened.
    let prs = open_prs(r, rec);

    if !rec.args.results_bucket.is_empty() {
        match record(rec, &prs) {
            Ok(uri) => r.note(&format!("published run record to {uri}")),
            Err(e) => r.note(&format!(
                "record publish failed (results stay in the workspace): {e:#}"
            )),
        }
    }
    prs
}

/// Open the draft PR(s) for a kept run and return their links (for the record). Best-effort: a push/
/// open failure logs through `r` and yields no link, never aborting the publish. Empty when nothing
/// was kept or no PR repo is configured.
fn open_prs<R: Reporter>(r: &mut R, rec: &Record<'_>) -> Vec<PrLink> {
    // Gate on any kept row (not on this session's `kept_shas`), so a resumed run whose keeps happened
    // earlier still ships the deliverable.
    let kept_any = rec.rows.iter().any(|row| row.decision == "keep");
    if !kept_any {
        return Vec::new();
    }
    // Composite multi-fork (one draft PR per touched component) vs single-repo (one PR). A composite
    // run carries per-component targets; a single-repo run carries `pr_repo` + the single base/head.
    if !rec.components.is_empty() {
        match open_composite_prs(r, rec) {
            Ok(prs) => {
                r.note(&format!(
                    "opened {} linked draft PR(s) across forks",
                    prs.len()
                ));
                prs
            }
            Err(e) => {
                r.note(&format!(
                    "composite PR publish FAILED (kept change is in git memory + S3, recoverable): {e:#}"
                ));
                Vec::new()
            }
        }
    } else if !rec.args.pr_repo.is_empty() {
        match open_single_repo_prs(r, rec) {
            Ok(prs) => {
                r.note(&format!("opened {} draft PR(s)", prs.len()));
                prs
            }
            Err(e) => {
                r.note(&format!(
                    "PR publish FAILED (kept change is in git memory + S3, recoverable): {e:#}"
                ));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    }
}

/// Incremental cross-run memory: snapshot the run's durable S3 state (RESULTS.md + summary + the
/// per-goal `latest.json` pointer) after a decided iteration, so a fresh run inherits this run's
/// tried ideas even if the loop pod is killed before [`publish`] fires at end-of-run. Without this,
/// `publish` only runs on a clean exit, so a `kubectl delete pod` (SIGKILL) publishes nothing and the
/// next run seeds empty. No git push here, the branch is the end-of-run deliverable, not a per-iter
/// cost, and best-effort like the rest of publishing: a hiccup logs and the loop keeps going.
pub fn publish_progress<R: Reporter>(r: &mut R, rec: &Record<'_>) {
    if rec.args.results_bucket.is_empty() {
        return;
    }
    // No PRs at this incremental checkpoint, they open only at end-of-run.
    match record(rec, &[]) {
        Ok(_) => r.note("updated cross-run memory (latest.json)"),
        Err(e) => r.note(&format!(
            "progress publish failed (end-of-run will retry): {e:#}"
        )),
    }
}

// --- S3 channel ------------------------------------------------------------

#[derive(Serialize)]
struct Summary {
    run_id: String,
    goal: String,
    gate: String,
    model: String,
    baseline_score: Option<f64>,
    best_score: Option<f64>,
    improved: bool,
    iterations: Vec<IterSummary>,
    kept_commits: Vec<String>,
    /// The pristine upstream base the kept commits sit on, the PR's diff base.
    #[serde(skip_serializing_if = "Option::is_none")]
    base_sha: Option<String>,
    /// The single-repo fork (`owner/repo`) this run PRs against, when configured. Composite runs
    /// leave this `None` and carry per-fork identity in `components` instead. Recorded so reporting
    /// can show *which* repo a run targeted without re-deriving it from the manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    /// The draft PR(s) this run opened (single-repo: one; composite: one per touched fork). Empty
    /// when nothing was kept, no PR repo was configured, or the open failed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    prs: Vec<PrLink>,
    /// Composite per-fork base/head identity, so a multi-fork run's report can show each
    /// component's repo and the SHAs its PR spans. Empty for a single-repo run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    components: Vec<ComponentRecord>,
    cost_usd: f64,
    elapsed_secs: u64,
    /// The run's comparability key ([`crate::identity::RunIdentity::digest`]), additive field,
    /// absent from summaries written before this landed.
    #[serde(skip_serializing_if = "String::is_empty")]
    identity_digest: String,
}

#[derive(Serialize)]
struct IterSummary {
    iter: u32,
    decision: String,
    score: Option<f64>,
    note: String,
}

/// One opened draft PR, recorded so a report can deep-link to it. `name` is the component name for a
/// composite run, empty for a single-repo run. `branch` is the per-candidate head branch the PR was
/// opened from (`autoresearch/<run_id>/<candidate>`), provenance so the controller can tie a PR back
/// to its candidate when a run opens several.
#[derive(Serialize, Clone)]
pub struct PrLink {
    pub name: String,
    pub repo: String,
    pub url: String,
    pub branch: String,
}

/// Per-fork base/head identity for a composite run, mirroring [`PublishTarget`] minus the local
/// workspace path (which is meaningless off the loop pod).
#[derive(Serialize)]
struct ComponentRecord {
    name: String,
    repo: String,
    base_sha: String,
    head_sha: String,
}

/// Scores can be `INFINITY` (no measurement); `serde_json` refuses non-finite
/// floats, so drop those to `null` rather than blow up the whole record.
pub(crate) fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

fn build_summary(rec: &Record<'_>, prs: &[PrLink]) -> Summary {
    Summary {
        run_id: rec.run_id.to_string(),
        goal: rec.goal.to_string(),
        gate: rec.gate.clone(),
        model: rec.model.to_string(),
        baseline_score: finite(rec.baseline_score),
        best_score: finite(rec.best_score),
        improved: rec.improved,
        iterations: rec
            .rows
            .iter()
            .map(|row| IterSummary {
                iter: row.iter,
                decision: row.decision.clone(),
                score: row.score.and_then(finite),
                note: row.note.clone(),
            })
            .collect(),
        kept_commits: rec.kept_shas.to_vec(),
        base_sha: rec.base_sha.map(str::to_string),
        repo: (!rec.args.pr_repo.is_empty()).then(|| rec.args.pr_repo.clone()),
        prs: prs.to_vec(),
        components: rec
            .components
            .iter()
            .map(|c| ComponentRecord {
                name: c.name.clone(),
                repo: c.repo.clone(),
                base_sha: c.base_sha.clone(),
                head_sha: c.head_sha.clone(),
            })
            .collect(),
        cost_usd: rec.cost_usd,
        elapsed_secs: rec.elapsed.as_secs(),
        identity_digest: rec.identity_digest.to_string(),
    }
}

/// A compact, per-run discovery record. Written to a flat `index/` prefix (and a
/// per-goal `latest.json`) so reporting tools list small objects instead of walking
/// run dirs or fetching 25 KB session logs. Deliberately omits the full goal text.
#[derive(Serialize)]
struct IndexEntry {
    run_id: String,
    goal_slug: String,
    goal_line: String,
    gate: String,
    model: String,
    baseline_score: Option<f64>,
    best_score: Option<f64>,
    improved: bool,
    iterations: usize,
    kept: usize,
    cost_usd: f64,
    elapsed_secs: u64,
    /// The fork this run targeted (`owner/repo`), for a leaderboard repo column without fetching the
    /// full summary. `None` for a run with no PR repo (and composite runs, whose forks live per-component
    /// in the summary).
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    /// `s3://…` URI of the full run record (session.jsonl, summary.json, diffs).
    record: String,
}

/// First non-empty goal line, de-hashed and length-capped, for leaderboard display.
pub(crate) fn goal_line(goal: &str) -> String {
    let line = goal
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_start_matches('#')
        .trim();
    line.chars().take(120).collect()
}

fn build_index_entry(rec: &Record<'_>, goal_slug: &str, record_uri: &str) -> IndexEntry {
    IndexEntry {
        run_id: rec.run_id.to_string(),
        goal_slug: goal_slug.to_string(),
        goal_line: goal_line(rec.goal),
        gate: rec.gate.clone(),
        model: rec.model.to_string(),
        baseline_score: finite(rec.baseline_score),
        best_score: finite(rec.best_score),
        improved: rec.improved,
        iterations: rec.rows.iter().filter(|r| r.iter > 0).count(),
        kept: rec.rows.iter().filter(|r| r.decision == "keep").count(),
        cost_usd: rec.cost_usd,
        elapsed_secs: rec.elapsed.as_secs(),
        repo: (!rec.args.pr_repo.is_empty()).then(|| rec.args.pr_repo.clone()),
        record: record_uri.to_string(),
    }
}

/// The S3 keys for one run. Layout (under the bucket's configured base prefix):
///   runs/<goal-slug>/<run_id>/{session.jsonl, RESULTS.md, summary.json, diffs/…}
///   runs/<goal-slug>/latest.json  , overwritten each run (current state for a goal)
///   index/<run_id>.json           , flat, time-sortable discovery shard
struct Keys {
    run_prefix: String,
    index_key: String,
    latest_key: String,
}

fn keys(base: &str, goal_slug: &str, run_id: &str) -> Keys {
    let root = if base.is_empty() {
        String::new()
    } else {
        format!("{base}/")
    };
    Keys {
        run_prefix: format!("{root}runs/{goal_slug}/{run_id}"),
        index_key: format!("{root}index/{run_id}.json"),
        latest_key: format!("{root}runs/{goal_slug}/latest.json"),
    }
}

/// A publish destination: S3 (`s3://bucket[/prefix]`) or a mounted filesystem
/// (`file:///abs/path`, e.g. an artifacts PVC on a cluster with no S3 reach). Both write the
/// exact same key layout, so reporting tools walk either.
enum Backend {
    // The S3 half re-parses the URI where it's used (the async block owns bucket/base), so the
    // variant carries nothing.
    S3,
    File { root: std::path::PathBuf },
}

fn backend(uri: &str) -> Result<Backend, PublishError> {
    match uri.strip_prefix("file://") {
        Some(path) if path.starts_with('/') => Ok(Backend::File {
            root: std::path::PathBuf::from(path),
        }),
        Some(_) => Err(PublishError::FileRootRelative {
            uri: uri.to_owned(),
        }),
        None => parse_s3_uri(uri).map(|_| Backend::S3),
    }
}

/// Write the run record to whichever backend the results URI names.
fn record(rec: &Record<'_>, prs: &[PrLink]) -> Result<String> {
    match backend(&rec.args.results_bucket)? {
        Backend::S3 => s3_record(rec, prs),
        Backend::File { root } => file_record(rec, prs, &root),
    }
}

/// Newest match per declared artifact name (`embed` and `upload` both publish; `embed` also
/// inlines in the PR body). Returned as (workspace-relative key, absolute path), deduped so an
/// artifact naming the same file twice uploads once.
fn artifact_matches(rec: &Record<'_>) -> Vec<(String, std::path::PathBuf)> {
    let mut out: Vec<(String, std::path::PathBuf)> = Vec::new();
    for a in &rec.args.artifacts {
        for name in [&a.embed, &a.upload].into_iter().flatten() {
            let Some(p) = newest_named_file(&rec.paths.workspace.join(&a.path), name) else {
                continue;
            };
            let rel = p
                .strip_prefix(&rec.paths.workspace)
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_else(|_| name.clone());
            if !out.iter().any(|(have, _)| *have == rel) {
                out.push((rel, p));
            }
        }
    }
    out
}

/// A published artifact's content type, from its extension. Text-ish renders in a browser;
/// everything else (a .pptx) downloads.
fn artifact_content_type(rel: &str) -> &'static str {
    match rel.rsplit('.').next() {
        Some("txt" | "md" | "log") => "text/plain",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

/// The filesystem twin of [`s3_record`]: same layout under `root` (a mounted PVC). Everything the
/// S3 path treats as best-effort reads stays best-effort here; directory creation and the record
/// writes themselves are real errors (a read-only or missing mount must be loud).
fn file_record(rec: &Record<'_>, prs: &[PrLink], root: &Path) -> Result<String> {
    let goal_slug = slug(rec.goal);
    let k = keys("", &goal_slug, rec.run_id);
    let record_uri = format!("file://{}/{}", root.display(), k.run_prefix);

    let summary =
        serde_json::to_vec_pretty(&build_summary(rec, prs)).context("serialize summary.json")?;
    let index = serde_json::to_vec_pretty(&build_index_entry(rec, &goal_slug, &record_uri))
        .context("serialize index entry")?;

    let write = |key: &str, bytes: &[u8]| -> Result<()> {
        let dest = root.join(key);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir -p {}", parent.display()))?;
        }
        std::fs::write(&dest, bytes).with_context(|| format!("writing {}", dest.display()))
    };

    if let Ok(bytes) = std::fs::read(&rec.paths.session_log) {
        write(&format!("{}/session.jsonl", k.run_prefix), &bytes)?;
    }
    if let Ok(bytes) = std::fs::read(rec.paths.workspace.join("RESULTS.md")) {
        write(&format!("{}/RESULTS.md", k.run_prefix), &bytes)?;
    }
    for (rel, path) in artifact_matches(rec) {
        if let Ok(bytes) = std::fs::read(&path) {
            write(&format!("{}/artifacts/{rel}", k.run_prefix), &bytes)?;
        }
    }
    write(&format!("{}/summary.json", k.run_prefix), &summary)?;
    for row in rec.rows.iter().filter(|r| !r.diff.is_empty()) {
        write(
            &format!("{}/diffs/iter-{}.patch", k.run_prefix, row.iter),
            row.diff.as_bytes(),
        )?;
    }
    write(&k.index_key, &index)?;
    write(&k.latest_key, &index)?;
    Ok(record_uri)
}

fn s3_record(rec: &Record<'_>, prs: &[PrLink]) -> Result<String> {
    let (bucket, base) = parse_s3_uri(&rec.args.results_bucket)?;
    let goal_slug = slug(rec.goal);
    let k = keys(&base, &goal_slug, rec.run_id);
    let record_uri = format!("s3://{bucket}/{}", k.run_prefix);

    let summary =
        serde_json::to_vec_pretty(&build_summary(rec, prs)).context("serialize summary.json")?;
    let index = serde_json::to_vec_pretty(&build_index_entry(rec, &goal_slug, &record_uri))
        .context("serialize index entry")?;

    crate::engine::handle()?.block_on(async {
        let conf = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = aws_sdk_s3::Client::new(&conf);

        // session.jsonl + RESULTS.md are best-effort reads: a crashed-early run may
        // not have written them yet, and a missing file shouldn't sink the record.
        if let Ok(bytes) = std::fs::read(&rec.paths.session_log) {
            put(
                &client,
                &bucket,
                &format!("{}/session.jsonl", k.run_prefix),
                bytes,
                "application/x-ndjson",
            )
            .await?;
        }
        if let Ok(bytes) = std::fs::read(rec.paths.workspace.join("RESULTS.md")) {
            put(
                &client,
                &bucket,
                &format!("{}/RESULTS.md", k.run_prefix),
                bytes,
                "text/markdown",
            )
            .await?;
        }
        // Declared artifacts (`embed` + `upload` matches), kept with the run record so they
        // outlive the workspace. Best-effort reads like RESULTS.md.
        for (rel, path) in artifact_matches(rec) {
            if let Ok(bytes) = std::fs::read(&path) {
                put(
                    &client,
                    &bucket,
                    &format!("{}/artifacts/{rel}", k.run_prefix),
                    bytes,
                    artifact_content_type(&rel),
                )
                .await?;
            }
        }
        put(
            &client,
            &bucket,
            &format!("{}/summary.json", k.run_prefix),
            summary,
            "application/json",
        )
        .await?;

        for row in rec.rows.iter().filter(|r| !r.diff.is_empty()) {
            put(
                &client,
                &bucket,
                &format!("{}/diffs/iter-{}.patch", k.run_prefix, row.iter),
                row.diff.clone().into_bytes(),
                "text/x-patch",
            )
            .await?;
        }

        // Discovery shards: the flat per-run index and the per-goal latest pointer.
        put(
            &client,
            &bucket,
            &k.index_key,
            index.clone(),
            "application/json",
        )
        .await?;
        put(&client, &bucket, &k.latest_key, index, "application/json").await?;
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(record_uri)
}

/// Download one published object at an exact `s3://bucket/key` URI to a local file, the general
/// GetObject the controller's artifact proxy shells (`crucible fetch`), keeping every S3 client out
/// of `crucible-controller` (that crate has no aws-sdk and never learns the bucket layout). Nothing
/// is appended to the URI: the caller passes the exact key it wants. Reuses the same IRSA creds the
/// publisher uses (GetObject is in the role's policy).
pub fn fetch_object(uri: &str, dest: &std::path::Path) -> Result<()> {
    if let Backend::File { root } = backend(uri)? {
        // The file URI IS the object path; a plain copy is the whole fetch.
        std::fs::copy(&root, dest)
            .with_context(|| format!("copying {} to {}", root.display(), dest.display()))?;
        return Ok(());
    }
    let (bucket, key) = parse_s3_uri(uri)?;
    if key.is_empty() {
        return Err(PublishError::NoKey {
            uri: uri.to_owned(),
        }
        .into());
    }
    crate::engine::handle()?.block_on(async {
        let conf = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = aws_sdk_s3::Client::new(&conf);
        let out = client
            .get_object()
            .bucket(&bucket)
            .key(&key)
            .send()
            .await
            .with_context(|| format!("GetObject s3://{bucket}/{key}"))?;
        let data = out
            .body
            .collect()
            .await
            .context("read object body")?
            .into_bytes();
        std::fs::write(dest, &data)
            .with_context(|| format!("writing {} ({} bytes)", dest.display(), data.len()))?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Cross-run memory: fetch the PREVIOUS run's tried-ideas ledger for this goal from S3, so a fresh
/// run inherits what's already been tried instead of re-walking dead ends. Reads the per-goal
/// `latest.json` pointer, then that run's `RESULTS.md`, and returns its candidate rows (baseline
/// and header rows stripped). Best-effort: `None` when there's no bucket, no prior run for this
/// goal, or any S3 hiccup, the loop then starts with an empty history (the prior behavior).
pub fn fetch_prior_results(results_bucket: &str, goal: &str) -> Option<String> {
    if results_bucket.is_empty() {
        return None;
    }
    let goal_slug = slug(goal);
    if let Ok(Backend::File { root }) = backend(results_bucket) {
        let latest =
            std::fs::read_to_string(root.join(&keys("", &goal_slug, "").latest_key)).ok()?;
        let run_id = serde_json::from_str::<serde_json::Value>(&latest)
            .ok()?
            .get("run_id")?
            .as_str()?
            .to_string();
        let md = std::fs::read_to_string(
            root.join(keys("", &goal_slug, &run_id).run_prefix)
                .join("RESULTS.md"),
        )
        .ok()?;
        let rows = prior_candidate_rows(&md);
        return (!rows.is_empty()).then_some(rows);
    }
    let (bucket, base) = parse_s3_uri(results_bucket).ok()?;
    // `latest.json` is run-id-independent; it names the most recent prior run for this goal.
    let latest_key = keys(&base, &goal_slug, "").latest_key;
    let md = crate::engine::handle().ok()?.block_on(async {
        let conf = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = aws_sdk_s3::Client::new(&conf);
        let latest = get_string(&client, &bucket, &latest_key).await?;
        let run_id = serde_json::from_str::<serde_json::Value>(&latest)
            .ok()?
            .get("run_id")?
            .as_str()?
            .to_string();
        let results_key = format!("{}/RESULTS.md", keys(&base, &goal_slug, &run_id).run_prefix);
        get_string(&client, &bucket, &results_key).await
    })?;
    let rows = prior_candidate_rows(&md);
    (!rows.is_empty()).then_some(rows)
}

/// The candidate decision rows of a `RESULTS.md`, its `| … |` table lines minus the header,
/// separator, and per-run `baseline` rows (those repeat every run and carry no idea). Because each
/// run renders its own prior section as table rows too, this accumulates the full history flatly.
fn prior_candidate_rows(results_md: &str) -> String {
    results_md
        .lines()
        .filter(|l| l.starts_with("| "))
        .filter(|l| {
            !l.contains("| iter ") && !l.starts_with("| ---") && !l.contains("| baseline |")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// GET an S3 object as a UTF-8 string, or `None` on any error (missing key, non-utf8, …).
async fn get_string(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Option<String> {
    let out = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .ok()?;
    let data = out.body.collect().await.ok()?.into_bytes();
    String::from_utf8(data.to_vec()).ok()
}

async fn put(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    body: Vec<u8>,
    content_type: &str,
) -> Result<()> {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from(body))
        .content_type(content_type)
        .send()
        .await
        .with_context(|| format!("PutObject s3://{bucket}/{key}"))?;
    Ok(())
}

/// A malformed publish target. The URI forms and the `gh`/`git` shell-outs are the two ways
/// publishing refuses on its own terms; everything else is plumbing and stays `anyhow`.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PublishError {
    #[error("results bucket must be an s3:// or file:// URI, got `{uri}`")]
    NotAnS3Uri { uri: String },
    #[error("file:// results root must be absolute: `{uri}`")]
    FileRootRelative { uri: String },
    #[error("results bucket URI has no bucket: `{uri}`")]
    NoBucket { uri: String },
    #[error("fetch object URI has no key: `{uri}`")]
    NoKey { uri: String },
    #[error("gh pr edit failed: {stderr}")]
    PrEditFailed { stderr: String },
    #[error("gh pr create failed: {stderr}")]
    PrCreateFailed { stderr: String },
    #[error("gh pr create succeeded but printed no PR url")]
    PrCreateSilent,
    #[error("git push of {refspec} failed ({status})")]
    GitPushFailed {
        refspec: String,
        status: std::process::ExitStatus,
    },
}

/// `s3://bucket[/prefix]` → (bucket, prefix). Prefix is trimmed of slashes and may
/// be empty.
fn parse_s3_uri(uri: &str) -> Result<(String, String), PublishError> {
    let rest = uri
        .strip_prefix("s3://")
        .ok_or_else(|| PublishError::NotAnS3Uri {
            uri: uri.to_owned(),
        })?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    if bucket.is_empty() {
        return Err(PublishError::NoBucket {
            uri: uri.to_owned(),
        });
    }
    Ok((bucket.to_string(), prefix.trim_matches('/').to_string()))
}

// --- git PR channel --------------------------------------------------------

/// The head branch for one kept candidate: `autoresearch/<namespace>/<candidate_id>`. The
/// `<candidate_id>` discriminant makes a multi-candidate run (an ablation/portfolio) push each
/// candidate to its OWN branch instead of every candidate clobbering one shared ref.
/// `candidate_id` must be unique within the run and must not end in `-base` (that suffix names the
/// sibling base branch, see [`base_branch_name`]).
///
/// Both parts must be stable across a resume replay: the namespace is the goal slug and the
/// discriminant the kept commit's short sha whenever that sha is known (see
/// [`single_repo_candidates`]). Run-id-keyed branches minted a fresh branch + PR per replayed
/// finish — run 6 opened five PRs for one kept diff.
fn head_branch_name(namespace: &str, candidate_id: &str) -> String {
    format!("autoresearch/{namespace}/{candidate_id}")
}

/// First 12 chars of a commit sha, the branch discriminant for a kept candidate.
/// Char-indexed so a malformed non-ASCII "sha" truncates instead of panicking.
fn short_sha(sha: &str) -> &str {
    match sha.char_indices().nth(12) {
        Some((i, _)) => &sha[..i],
        None => sha,
    }
}

/// The base branch for one kept candidate: the head branch plus a `-base` suffix, pinned at the
/// pristine upstream SHA so the PR diff is exactly that candidate's commits.
fn base_branch_name(namespace: &str, candidate_id: &str) -> String {
    format!("autoresearch/{namespace}/{candidate_id}-base")
}

/// One publishable candidate: a collision-free discriminant within the run (goes in the branch name)
/// plus the local refs its PR spans. A single-repo run keeps exactly one today (the kept tip on
/// the pristine base); the multi-candidate portfolio that keeps SEVERAL candidates per run, each its own
/// branch + PR for side-by-side ablation, is the separate upstream piece.
struct KeptCandidate {
    /// Branch namespace: the goal slug when the kept tip sha is known (stable across resume
    /// replays), else the run id (the legacy scheme; no cross-replay dedupe possible).
    namespace: String,
    /// Discriminant, unique within the run, never suffixed `-base` (see [`head_branch_name`]).
    id: String,
    /// Local ref for the kept tip, `HEAD`, or a bare SHA.
    head_ref: String,
    /// Pristine base SHA, pinned as a branch so the PR diff is candidate-relative. `None` on resume.
    base_sha: Option<String>,
}

impl KeptCandidate {
    fn head_branch(&self) -> String {
        head_branch_name(&self.namespace, &self.id)
    }
    fn base_branch(&self) -> String {
        base_branch_name(&self.namespace, &self.id)
    }
}

/// The single-repo run's kept candidates. Today exactly one: the cumulative kept-commit chain
/// (its tip is the last of `kept_shas`), on the pristine base. When the kept tip sha is known
/// the branch keys on it, so republishing the same kept commit derives the same branch no
/// matter which run id the replay minted; without a sha (a resume from a log predating kept
/// snapshots) the run-id scheme still applies. Returned as a list so the publish loop already
/// opens one PR per candidate, a multi-candidate portfolio drops in here with no publish change.
fn single_repo_candidates(
    goal: &str,
    run_id: &str,
    kept_shas: &[String],
    base_sha: Option<&str>,
) -> Vec<KeptCandidate> {
    let cand = match kept_shas.last() {
        Some(sha) => KeptCandidate {
            namespace: slug(goal),
            id: short_sha(sha).to_string(),
            head_ref: sha.clone(),
            base_sha: base_sha.map(str::to_string),
        },
        None => KeptCandidate {
            namespace: run_id.to_string(),
            id: "0".to_string(),
            head_ref: "HEAD".to_string(),
            base_sha: base_sha.map(str::to_string),
        },
    };
    vec![cand]
}

/// Test-only view of the branch derivation, so the run-6 fixture test (in `loop_driver`'s
/// tests) can assert replay stability without shelling git.
#[cfg(test)]
pub(crate) fn head_branch_for_test(goal: &str, run_id: &str, kept_shas: &[String]) -> String {
    single_repo_candidates(goal, run_id, kept_shas, None)
        .remove(0)
        .head_branch()
}

/// Push each kept candidate's tip (+ its pristine base) to its own branch and open a DRAFT PR per
/// candidate, returning their links.
///
/// Two refs per candidate: the **head** branch `autoresearch/<run_id>/<candidate>` is the cumulative
/// kept-commit chain, and a **base** branch pinned at the pristine upstream SHA (segment 0's baseline,
/// before any agent commit) makes that candidate's PR diff *exactly* its kept commits. A GitHub PR is
/// branch-to-branch, not SHA-to-SHA, so git reconstructs the delta, no hand-rolled base+delta. On
/// resume the base SHA is absent (it lived in the original run), so that PR opens against the repo's
/// default branch.
fn open_single_repo_prs<R: Reporter>(r: &mut R, rec: &Record<'_>) -> Result<Vec<PrLink>> {
    let token = pr_token()?;
    // PAT in the URL (x-access-token is GitHub's conventional username for a token-as-password). The
    // URL is kept out of every error message (see push_ref) so the token never lands in a log.
    let url = push_url(&rec.args.pr_repo, &token);
    let mut links = Vec::new();
    for cand in single_repo_candidates(rec.goal, rec.run_id, rec.kept_shas, rec.base_sha) {
        let head = cand.head_branch();
        // Sha-keyed branches make a republished kept commit recognizable: a prior segment
        // already opened this branch's PR, so this replay is a no-op, not a duplicate.
        if rec.published_branches.contains(&head) {
            r.note(&format!(
                "publish: kept commit already published as {head}; skipping duplicate PR"
            ));
            continue;
        }
        push_ref(&rec.paths.workspace, &url, &cand.head_ref, &head).with_context(|| {
            format!("pushing kept commits to {} branch {head}", rec.args.pr_repo)
        })?;

        // Pin the pristine base as a branch so the PR diff is the kept commits and nothing else. The
        // base commit is an ancestor of HEAD, so it's already a local object, push it by SHA to a ref.
        let base_branch = match &cand.base_sha {
            Some(sha) => {
                let b = cand.base_branch();
                push_ref(&rec.paths.workspace, &url, sha, &b)
                    .with_context(|| format!("pushing pristine base {sha} to branch {b}"))?;
                Some(b)
            }
            None => None,
        };

        let pr = open_pr(
            &rec.args.pr_repo,
            &pr_title(rec),
            &pr_body(rec),
            &head,
            base_branch.as_deref(),
            &token,
        )?;
        links.push(PrLink {
            name: String::new(),
            repo: rec.args.pr_repo.clone(),
            url: pr,
            branch: head,
        });
    }
    Ok(links)
}

/// Build the authenticated push URL for `owner/repo`. The PAT is `x-access-token`'s password (GitHub's
/// token-as-password convention); the whole URL is kept out of error messages (see [`push_ref`]) so the
/// token never lands in a log.
fn push_url(repo: &str, token: &str) -> String {
    format!("https://x-access-token:{token}@github.com/{repo}.git")
}

/// Composite multi-fork publish: one cross-linked draft PR per touched component, against that
/// component's fork. Each component is a real git repo on the loop pod, so this reuses the exact
/// single-repo mechanism, push the kept tip + the pristine base as branches, open a branch-to-branch
/// draft PR so git reconstructs the per-component diff, no GitHub blobs/tree reconstruction, no log
/// parsing. Returns the opened PR links (for the
/// run record). A per-component push/open failure aborts (the caller logs it best-effort); the kept
/// work stays in git memory + S3, recoverable.
fn open_composite_prs<R: Reporter>(r: &mut R, rec: &Record<'_>) -> Result<Vec<PrLink>> {
    let token = pr_token()?;
    let ns = slug(rec.goal);

    // Open every component's PR, collecting (name, repo, url, branch) for the cross-link pass.
    // Each fork's branch keys on that component's kept tip sha, same replay-stability argument
    // as the single-repo path (see [`head_branch_name`]).
    let mut opened: Vec<(String, String, String, String)> = Vec::new();
    for c in rec.components {
        let head = head_branch_name(&ns, short_sha(&c.head_sha));
        if rec.published_branches.contains(&head) {
            r.note(&format!(
                "publish: {} kept tip already published as {head}; skipping duplicate PR",
                c.name
            ));
            continue;
        }
        let base_branch = base_branch_name(&ns, short_sha(&c.head_sha));
        let url = push_url(&c.repo, &token);
        push_ref(&c.workspace, &url, &c.head_sha, &head)
            .with_context(|| format!("pushing {} kept tip to {} branch {head}", c.name, c.repo))?;
        push_ref(&c.workspace, &url, &c.base_sha, &base_branch).with_context(|| {
            format!("pushing {} base to {} branch {base_branch}", c.name, c.repo)
        })?;
        let title = composite_pr_title(rec, &c.name);
        let body = composite_pr_body(rec, &c.name);
        let pr = open_pr(&c.repo, &title, &body, &head, Some(&base_branch), &token)
            .with_context(|| format!("opening draft PR for component {} on {}", c.name, c.repo))?;
        r.note(&format!("  {} → {}: {pr}", c.name, c.repo));
        opened.push((c.name.clone(), c.repo.clone(), pr, head));
    }

    // Cross-link the set: each body lists the sibling PRs (only meaningful when >1 component changed).
    if opened.len() > 1 {
        let link_set: Vec<(String, String, String)> = opened
            .iter()
            .map(|(name, repo, url, _)| (name.clone(), repo.clone(), url.clone()))
            .collect();
        for (name, repo, _url) in &link_set {
            let body = composite_pr_body_linked(rec, name, &link_set);
            if let Err(e) = edit_pr_body(repo, &pr_url_for(repo, &link_set), &body, &token) {
                r.note(&format!(
                    "  cross-link of {name} PR failed (non-fatal): {e:#}"
                ));
            }
        }
    }
    Ok(opened
        .into_iter()
        .map(|(name, repo, url, branch)| PrLink {
            name,
            repo,
            url,
            branch,
        })
        .collect())
}

/// The PR url for `repo` from the opened set (each fork's PR is keyed by its repo).
fn pr_url_for(repo: &str, opened: &[(String, String, String)]) -> String {
    opened
        .iter()
        .find(|(_, r, _)| r == repo)
        .map(|(_, _, u)| u.clone())
        .unwrap_or_default()
}

/// PATCH a PR's body via `gh pr edit <url> --body` (cross-linking pass). Best-effort: the PRs are
/// already open, this only adds the sibling references.
fn edit_pr_body(repo: &str, url: &str, body: &str, token: &str) -> Result<()> {
    let out = std::process::Command::new("gh")
        .arg("pr")
        .arg("edit")
        .arg(url)
        .arg("--repo")
        .arg(repo)
        .arg("--body")
        .arg(body)
        .env("GH_TOKEN", token)
        .output()
        .context("running `gh pr edit`")?;
    if !out.status.success() {
        return Err(PublishError::PrEditFailed {
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        }
        .into());
    }
    Ok(())
}

/// Read the PR PAT from `AUTORESEARCH_PR_TOKEN` (fallback `GITHUB_TOKEN`). Used both in the push URL
/// and as `gh`'s `GH_TOKEN`, so the whole channel shares one credential.
fn pr_token() -> Result<String> {
    std::env::var("AUTORESEARCH_PR_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .context("no PAT in AUTORESEARCH_PR_TOKEN or GITHUB_TOKEN")
}

/// Push a local ref (`HEAD`, or a bare SHA) to `url` as `refs/heads/{branch}` using the SYSTEM git
/// binary.
///
/// NOT libgit2: the runtime image's libgit2 has no TLS backend, so an HTTPS push through the git2
/// crate silently fails. System git
/// carries TLS. `<ref>:refs/heads/{branch}` pushes whatever `<ref>` resolves to, so a DETACHED HEAD
/// (the composed world is `git checkout --detach <SHA>` + the agent's commits) just works, no local
/// branch needed. `--force` makes a re-push idempotent (run_id is unique per run anyway). `url` may
/// carry the PAT, so it is deliberately excluded from the error.
fn push_ref(workspace: &Path, url: &str, local_ref: &str, branch: &str) -> Result<()> {
    let refspec = format!("{local_ref}:refs/heads/{branch}");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .arg("push")
        .arg("--force")
        .arg(url)
        .arg(&refspec)
        .status()
        .context("running `git push` (is git on PATH?)")?;
    if !status.success() {
        return Err(PublishError::GitPushFailed {
            refspec: refspec.to_owned(),
            status,
        }
        .into());
    }
    Ok(())
}

/// Open a draft PR via the `gh` CLI (already in the loop image; the approval watcher shells `gh api`
/// too). `--base` is omitted when there's no pinned base branch, so `gh` targets the repo's default
/// branch. Returns the PR url `gh` prints on success. `repo`/`title`/`body` are explicit so the
/// single-repo and per-component composite paths share it.
fn open_pr(
    repo: &str,
    title: &str,
    body: &str,
    head: &str,
    base: Option<&str>,
    token: &str,
) -> Result<String> {
    let mut cmd = std::process::Command::new("gh");
    cmd.arg("pr")
        .arg("create")
        .arg("--repo")
        .arg(repo)
        .arg("--head")
        .arg(head)
        .arg("--draft")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg(body)
        .env("GH_TOKEN", token);
    if let Some(base) = base {
        cmd.arg("--base").arg(base);
    }
    let out = cmd
        .output()
        .context("running `gh pr create` (is gh on PATH?)")?;
    if !out.status.success() {
        return Err(PublishError::PrCreateFailed {
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        }
        .into());
    }
    // gh prints the PR url on its own line; take the last url line to be safe.
    let url = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("https://"))
        .unwrap_or("")
        .to_string();
    if url.is_empty() {
        return Err(PublishError::PrCreateSilent.into());
    }
    Ok(url)
}

/// The PR title: a short, scannable line tying the goal to the win.
fn pr_title(rec: &Record<'_>) -> String {
    let line = goal_line(rec.goal);
    let line = if line.is_empty() { "candidate" } else { &line };
    match (finite(rec.baseline_score), finite(rec.best_score)) {
        (Some(b), Some(best)) => format!("[autoresearch] {line} ({b:.0} → {best:.0})"),
        _ => format!("[autoresearch] {line}"),
    }
}

/// The PR body: goal, the baseline→best delta, one section per kept iteration (the agent's full
/// CANDIDATE.md, the grade's metrics table, the candidate digest, and the declared-evidence
/// dispositions), run cost/elapsed, and a link back to the S3 run record (the PR and S3
/// cross-reference via run_id). The full writeup ships because a reviewer only sees this body:
/// DeepGEMM#5 arrived with the 120-char row note, cut mid-word, and no numbers.
fn pr_body(rec: &Record<'_>) -> String {
    let mut s = String::new();
    s.push_str(&format!("Autoresearch run `{}`.\n\n", rec.run_id));
    s.push_str(&format!("**Goal:** {}\n\n", goal_line(rec.goal)));
    s.push_str(&format!("**Gate:** {}\n", rec.gate));
    match (finite(rec.baseline_score), finite(rec.best_score)) {
        (Some(b), Some(best)) => {
            s.push_str(&format!("**Result:** baseline {b:.3} → best {best:.3}\n\n"));
        }
        _ => s.push('\n'),
    }
    if !rec.seed_hash.is_empty() {
        s.push_str(&format!(
            "**Seeded:** iteration 1 was handed a pack-declared seed diff \
             (content hash `{}`); the kept work builds on it.\n\n",
            rec.seed_hash
        ));
    }
    s.push_str(&score_chart(rec));
    let kept: Vec<&Row> = rec.rows.iter().filter(|r| r.decision == "keep").collect();
    for row in kept {
        s.push_str(&kept_section(row));
    }
    // Run-scoped epilogue results: advisory (they cannot un-keep), but a reviewer must
    // see a failed racecheck next to the candidate it ran against.
    let epilogue: Vec<&Row> = rec
        .rows
        .iter()
        .filter(|r| r.phase.as_deref() == Some("epilogue"))
        .collect();
    if !epilogue.is_empty() {
        s.push_str("## Epilogue checks (advisory)\n\n");
        for row in epilogue {
            let mark = match row.decision.as_str() {
                "epilogue" => "passed",
                "epilogue-skip" => "SKIPPED",
                _ => "**FAILED**",
            };
            s.push_str(&format!("- {mark} — {}\n", row.note));
        }
        s.push('\n');
    }
    s.push_str(&artifact_sections(rec));
    s.push_str(&format!(
        "Model `{}` · cost ${:.2} · {}s.\n",
        rec.model,
        rec.cost_usd,
        rec.elapsed.as_secs()
    ));
    if !rec.args.results_bucket.is_empty()
        && let Ok((bucket, base)) = parse_s3_uri(&rec.args.results_bucket)
    {
        let k = keys(&base, &slug(rec.goal), rec.run_id);
        s.push_str(&format!("\nRun record: `s3://{bucket}/{}`\n", k.run_prefix));
    }
    s.push_str("\n🤖 opened by crucible autoresearch (draft — review and steer).\n");
    s
}

/// Mermaid xychart of the run's score trajectory: every measured deep-loop iteration's score,
/// plus a flat series at the baseline. GitHub renders ```mermaid fences natively, so the PR
/// carries a real chart with no image hosting. Empty under two measured points (no trajectory).
fn score_chart(rec: &Record<'_>) -> String {
    let pts: Vec<(u32, f64)> = rec
        .rows
        .iter()
        .filter(|r| r.iter > 0 && r.phase.is_none())
        .filter_map(|r| r.score.and_then(finite).map(|v| (r.iter, v)))
        .collect();
    if pts.len() < 2 {
        return String::new();
    }
    let xs: Vec<String> = pts.iter().map(|(i, _)| i.to_string()).collect();
    let ys: Vec<String> = pts.iter().map(|(_, v)| trim_num(*v)).collect();
    // Mermaid quoted strings can't carry quotes; the gate label is free text from the manifest.
    let label = rec.gate.replace('"', "'");
    let mut s = format!(
        "```mermaid\nxychart-beta\n    title \"{label} by iteration\"\n    x-axis [{}]\n    y-axis \"{label}\"\n    line [{}]\n",
        xs.join(", "),
        ys.join(", "),
    );
    if let Some(b) = finite(rec.baseline_score) {
        let base = vec![trim_num(b); pts.len()];
        s.push_str(&format!("    line [{}]\n", base.join(", ")));
        s.push_str("```\n\n*Per-iteration measured score; the flat line is the baseline.*\n\n");
    } else {
        s.push_str("```\n\n*Per-iteration measured score.*\n\n");
    }
    s
}

/// A score formatted for a mermaid data list: fixed 6 decimals, trailing zeros trimmed, so
/// 8.500000 renders as 8.5 and long float dust doesn't bloat the chart source.
fn trim_num(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// PR-body cap for one embedded artifact. GitHub caps the whole body at 64 KiB and the
/// CANDIDATE.md writeups already claim a share, so an embed gets a head-truncated slice.
const EMBED_CAP: usize = 16 * 1024;

/// Declared-artifact PR sections: for each `[[workspace.artifact]]` with an `embed`, the newest
/// matching file under its path inlined in a collapsed block. The artifact itself is carried
/// (never in the diff), so this is how its content reaches a reviewer.
fn artifact_sections(rec: &Record<'_>) -> String {
    let mut s = String::new();
    for a in &rec.args.artifacts {
        let Some(embed) = &a.embed else { continue };
        let Some(path) = newest_named_file(&rec.paths.workspace.join(&a.path), embed) else {
            continue;
        };
        let Some(content) = read_capped(&path) else {
            continue;
        };
        let title = a
            .title
            .clone()
            .unwrap_or_else(|| a.path.trim_end_matches('/').to_string());
        let rel = path
            .strip_prefix(&rec.paths.workspace)
            .unwrap_or(&path)
            .display();
        s.push_str(&format!(
            "## {title}\n\n<details>\n<summary><code>{rel}</code></summary>\n\n````text\n{content}````\n\n</details>\n\n"
        ));
    }
    s
}

/// Read a text artifact, lossily and head-truncated to [`EMBED_CAP`], newline-terminated so it
/// composes into a fenced block. `None` when unreadable (the section is then simply absent).
fn read_capped(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut content = String::from_utf8_lossy(&bytes).into_owned();
    if content.len() > EMBED_CAP {
        let mut end = EMBED_CAP;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        content.push_str("\n… truncated …");
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }
    Some(content)
}

/// Newest (by mtime) file named `name` under `root`; `root` may itself be that file. Pipelines
/// write versioned dirs (`runtime/V1 … V7`) each holding a same-named summary, newest = the
/// final iteration's.
fn newest_named_file(root: &Path, name: &str) -> Option<std::path::PathBuf> {
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(md) = std::fs::metadata(&p) else {
            continue;
        };
        if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        } else if p.file_name().and_then(|n| n.to_str()) == Some(name) {
            let t = md.modified().unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
                best = Some((t, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// One kept iteration's PR section: the agent's whole CANDIDATE.md (the row note is its
/// 120-char fold, useless to a reviewer), then the grade's metrics, digest, and which
/// declared checks actually ran.
fn kept_section(row: &Row) -> String {
    let mut s = format!("## Kept: iter {}\n\n", row.iter);
    let writeup = if row.candidate_md.is_empty() {
        // Logs predating `candidate_md` only carried the fold; better than nothing.
        row.note.as_str()
    } else {
        row.candidate_md.as_str()
    };
    s.push_str(writeup.trim());
    s.push('\n');
    let table = metrics_table(&row.detail);
    if !table.is_empty() {
        s.push('\n');
        s.push_str(&table);
    }
    for digest in candidate_digests(&row.detail) {
        s.push_str(&format!("\nCandidate digest: `{digest}`\n"));
    }
    if !row.evidence.is_empty() {
        s.push_str(&format!(
            "\nDeclared checks: {}\n",
            crate::reporter::evidence_line(&row.evidence)
        ));
    }
    s.push('\n');
    s
}

/// A kept row's grade detail rendered as a markdown metrics table. The graph grade's fold is
/// `{"evidence": {<task>: {"detail": {"metrics": {...}, "threshold": ...}, ...}}}`; each
/// metric becomes a row, with the task's declared threshold alongside. Empty when the detail
/// doesn't carry that shape (a plain command judge) — the PR then simply has no table. The
/// `pass` pseudo-metric echoes the check verdict and is dropped.
fn metrics_table(detail: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(detail) else {
        return String::new();
    };
    let Some(evidence) = v.get("evidence").and_then(serde_json::Value::as_object) else {
        return String::new();
    };
    let mut rows = Vec::new();
    for (task, entry) in evidence {
        let d = entry.get("detail");
        let threshold = d
            .and_then(|d| d.get("threshold"))
            .map(ToString::to_string)
            .unwrap_or_default();
        let Some(metrics) = d
            .and_then(|d| d.get("metrics"))
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        for (name, value) in metrics {
            if name == "pass" {
                continue;
            }
            rows.push(format!("| {task} | {name} | {value} | {threshold} |"));
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    format!(
        "| check | metric | value | threshold |\n| --- | --- | --- | --- |\n{}\n",
        rows.join("\n")
    )
}

/// Distinct candidate digests from the grade detail's evidence entries, first-seen order.
/// Normally one (every rung graded the same built image); more than one means the rungs
/// measured different builds, which a reviewer should see.
fn candidate_digests(detail: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(detail) else {
        return Vec::new();
    };
    let Some(evidence) = v.get("evidence").and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    let mut digests: Vec<String> = Vec::new();
    for entry in evidence.values() {
        if let Some(d) = entry.get("digest").and_then(serde_json::Value::as_str)
            && !digests.iter().any(|have| have == d)
        {
            digests.push(d.to_string());
        }
    }
    digests
}

/// A composite component's PR title: the shared goal/delta plus which component this PR carries.
fn composite_pr_title(rec: &Record<'_>, component: &str) -> String {
    let line = goal_line(rec.goal);
    let line = if line.is_empty() { "candidate" } else { &line };
    match (finite(rec.baseline_score), finite(rec.best_score)) {
        (Some(b), Some(best)) => {
            format!("[autoresearch] {line} — {component} ({b:.0} → {best:.0})")
        }
        _ => format!("[autoresearch] {line} — {component}"),
    }
}

/// A composite component's PR body: the shared run summary ([`pr_body`]) plus a one-line note that this
/// is the `component` half of a cross-cut change. The sibling links are added by the cross-link pass.
fn composite_pr_body(rec: &Record<'_>, component: &str) -> String {
    let mut s = format!(
        "Autoresearch composite candidate — the **{component}** half of a cross-cut change.\n\n"
    );
    s.push_str(&pr_body(rec));
    s
}

/// As [`composite_pr_body`], plus a list of the sibling component PRs (the cross-link pass, run once all
/// PRs exist). `opened` is `(name, repo, url)`; the entry for `component` itself is skipped.
fn composite_pr_body_linked(
    rec: &Record<'_>,
    component: &str,
    opened: &[(String, String, String)],
) -> String {
    let mut s = composite_pr_body(rec, component);
    s.push_str(&sibling_links(component, opened));
    s
}

/// The "Linked PRs" markdown block for `component`: one line per OTHER opened PR (itself skipped).
/// Empty when there are no siblings (a single-component change opens one PR, nothing to link).
fn sibling_links(component: &str, opened: &[(String, String, String)]) -> String {
    let siblings: Vec<String> = opened
        .iter()
        .filter(|(n, _, _)| n != component)
        .map(|(n, _, url)| format!("- {n}: {url}"))
        .collect();
    if siblings.is_empty() {
        return String::new();
    }
    format!(
        "\n**Linked PRs** (this change spans multiple repos):\n{}\n",
        siblings.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PR body charts the score trajectory as a native mermaid xychart and inlines the
    /// newest match of a declared artifact's `embed`; the artifact file itself never enters
    /// the diff (it's carried), so the section is a reviewer's only view of it.
    #[test]
    fn pr_body_charts_scores_and_embeds_declared_artifacts() {
        let ws = std::env::temp_dir().join(format!("pub-artifact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join("codegen-out/runtime/V1")).unwrap();
        std::fs::create_dir_all(ws.join("codegen-out/runtime/V2")).unwrap();
        std::fs::write(
            ws.join("codegen-out/runtime/V1/summary.txt"),
            "old investigation",
        )
        .unwrap();
        std::fs::write(
            ws.join("codegen-out/runtime/V2/summary.txt"),
            "final investigation",
        )
        .unwrap();
        // Same-second writes tie on coarse filesystems; pin V1 unambiguously older.
        std::fs::File::open(ws.join("codegen-out/runtime/V1/summary.txt"))
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();

        let mut args = <crate::Cli as clap::Parser>::try_parse_from(["crucible"])
            .unwrap()
            .run;
        args.artifacts = vec![crate::manifest::Artifact {
            path: "codegen-out/runtime".into(),
            embed: Some("summary.txt".into()),
            upload: None,
            title: Some("Runtime investigation".into()),
        }];
        let paths = crate::Paths::for_manifest(
            ws.clone(),
            std::env::temp_dir(),
            &std::env::temp_dir(),
            None,
        );
        let rows = vec![
            Row {
                iter: 1,
                decision: "keep".into(),
                score: Some(8.6),
                ..Default::default()
            },
            Row {
                iter: 2,
                decision: "discard".into(),
                score: Some(8.9),
                ..Default::default()
            },
            Row {
                iter: 3,
                decision: "keep".into(),
                score: Some(8.5),
                ..Default::default()
            },
        ];
        let body = pr_body(&Record {
            args: &args,
            paths: &paths,
            run_id: "r",
            goal: "g",
            model: "m",
            gate: "tpot_ms".into(),
            rows: &rows,
            baseline_score: 9.0,
            best_score: 8.5,
            improved: true,
            kept_shas: &[],
            base_sha: None,
            components: &[],
            published_branches: &[],
            cost_usd: 0.0,
            elapsed: std::time::Duration::ZERO,
            identity_digest: "v2:0",
            seed_hash: "",
        });

        assert!(body.contains("```mermaid"), "{body}");
        assert!(body.contains("x-axis [1, 2, 3]"), "{body}");
        assert!(body.contains("line [8.6, 8.9, 8.5]"), "{body}");
        assert!(body.contains("line [9, 9, 9]"), "baseline series: {body}");
        assert!(body.contains("## Runtime investigation"), "{body}");
        assert!(
            body.contains("codegen-out/runtime/V2/summary.txt"),
            "{body}"
        );
        assert!(body.contains("final investigation"), "{body}");
        assert!(
            !body.contains("old investigation"),
            "newest match only: {body}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// One measured point is no trajectory, and an artifact whose embed never matched
    /// produces no section: the body degrades to exactly the pre-feature shape.
    #[test]
    fn no_chart_or_artifact_section_without_data() {
        let mut args = <crate::Cli as clap::Parser>::try_parse_from(["crucible"])
            .unwrap()
            .run;
        args.artifacts = vec![crate::manifest::Artifact {
            path: "does-not-exist".into(),
            embed: Some("summary.txt".into()),
            upload: None,
            title: None,
        }];
        let paths = crate::Paths::for_manifest(
            std::env::temp_dir().join("pub-artifact-none"),
            std::env::temp_dir(),
            &std::env::temp_dir(),
            None,
        );
        let rows = vec![Row {
            iter: 1,
            decision: "keep".into(),
            score: Some(8.6),
            ..Default::default()
        }];
        let body = pr_body(&Record {
            args: &args,
            paths: &paths,
            run_id: "r",
            goal: "g",
            model: "m",
            gate: "tpot_ms".into(),
            rows: &rows,
            baseline_score: 9.0,
            best_score: 8.6,
            improved: true,
            kept_shas: &[],
            base_sha: None,
            components: &[],
            published_branches: &[],
            cost_usd: 0.0,
            elapsed: std::time::Duration::ZERO,
            identity_digest: "v2:0",
            seed_hash: "",
        });
        assert!(!body.contains("```mermaid"), "{body}");
        assert!(!body.contains("<details>"), "{body}");
    }

    /// `file://` backend round trip: the record lands under the root with the same layout as
    /// S3 (summary, index, latest pointer, diffs, artifacts incl. an upload-only binary), and
    /// `fetch_prior_results` reads the cross-run memory back from it.
    #[test]
    fn file_backend_round_trips_record_and_prior_results() {
        let base = std::env::temp_dir().join(format!("pub-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ws = base.join("ws");
        let root = base.join("results");
        std::fs::create_dir_all(ws.join("codegen-out")).unwrap();
        std::fs::write(
            ws.join("RESULTS.md"),
            "| 1 | keep | 8.5 | tried X |
",
        )
        .unwrap();
        std::fs::write(ws.join("codegen-out/summary.txt"), "investigation").unwrap();
        std::fs::write(ws.join("codegen-out/summary.pptx"), b"PPTX".as_slice()).unwrap();

        let mut args = <crate::Cli as clap::Parser>::try_parse_from(["crucible"])
            .unwrap()
            .run;
        args.results_bucket = format!("file://{}", root.display());
        args.artifacts = vec![crate::manifest::Artifact {
            path: "codegen-out".into(),
            embed: Some("summary.txt".into()),
            upload: Some("summary.pptx".into()),
            title: None,
        }];
        let paths = crate::Paths::for_manifest(ws.clone(), base.join("state"), &base, None);
        let rows = vec![Row {
            iter: 1,
            decision: "keep".into(),
            score: Some(8.5),
            diff: "--- a
+++ b
"
            .into(),
            ..Default::default()
        }];
        let rec = Record {
            args: &args,
            paths: &paths,
            run_id: "20260811T000000Z-goal",
            goal: "goal",
            model: "m",
            gate: "tpot_ms".into(),
            rows: &rows,
            baseline_score: 9.0,
            best_score: 8.5,
            improved: true,
            kept_shas: &[],
            base_sha: None,
            components: &[],
            published_branches: &[],
            cost_usd: 0.0,
            elapsed: std::time::Duration::ZERO,
            identity_digest: "v2:0",
            seed_hash: "",
        };

        let uri = record(&rec, &[]).expect("file record");
        assert!(uri.starts_with("file://"), "{uri}");
        let run = root.join("runs/goal/20260811T000000Z-goal");
        for key in [
            "RESULTS.md",
            "summary.json",
            "diffs/iter-1.patch",
            "artifacts/codegen-out/summary.txt",
            "artifacts/codegen-out/summary.pptx",
        ] {
            assert!(run.join(key).exists(), "{key} written");
        }
        assert!(root.join("index/20260811T000000Z-goal.json").exists());
        assert!(root.join("runs/goal/latest.json").exists());

        let prior = fetch_prior_results(&args.results_bucket, "goal")
            .expect("prior results readable from file backend");
        assert!(prior.contains("tried X"), "{prior}");

        // fetch_object: the file URI is the object path.
        let dest = base.join("fetched.txt");
        fetch_object(
            &format!(
                "file://{}",
                run.join("artifacts/codegen-out/summary.txt").display()
            ),
            &dest,
        )
        .expect("fetch_object file");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "investigation");

        // A relative file root is refused loudly.
        drop(rec);
        let mut bad_args = args;
        bad_args.results_bucket = "file://relative/path".into();
        let bad = Record {
            args: &bad_args,
            paths: &paths,
            run_id: "r",
            goal: "goal",
            model: "m",
            gate: "tpot_ms".into(),
            rows: &rows,
            baseline_score: 9.0,
            best_score: 8.5,
            improved: true,
            kept_shas: &[],
            base_sha: None,
            components: &[],
            published_branches: &[],
            cost_usd: 0.0,
            elapsed: std::time::Duration::ZERO,
            identity_digest: "v2:0",
            seed_hash: "",
        };
        assert!(record(&bad, &[]).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Epilogue rows reach the PR body as an advisory section, with failures shouted.
    #[test]
    fn pr_body_reports_epilogue_results_loudly() {
        let args = <crate::Cli as clap::Parser>::try_parse_from(["crucible"])
            .unwrap()
            .run;
        let paths = crate::Paths::for_manifest(
            std::env::temp_dir(),
            std::env::temp_dir(),
            &std::env::temp_dir(),
            None,
        );
        let rows = vec![
            Row {
                iter: 1,
                decision: "keep".into(),
                note: "kept it".into(),
                ..Default::default()
            },
            Row {
                iter: 1,
                decision: "epilogue".into(),
                note: "perf: ok".into(),
                phase: Some("epilogue".into()),
                ..Default::default()
            },
            Row {
                iter: 1,
                decision: "epilogue-fail".into(),
                note: "racecheck: exit 3: boom".into(),
                phase: Some("epilogue".into()),
                ..Default::default()
            },
        ];
        let body = pr_body(&Record {
            args: &args,
            paths: &paths,
            run_id: "test-run",
            goal: "goal",
            model: "model",
            gate: "gate".into(),
            rows: &rows,
            baseline_score: 2.0,
            best_score: 1.0,
            improved: true,
            kept_shas: &[],
            base_sha: None,
            components: &[],
            published_branches: &[],
            cost_usd: 0.0,
            elapsed: std::time::Duration::ZERO,
            identity_digest: "v2:0",
            seed_hash: "",
        });
        assert!(body.contains("## Epilogue checks (advisory)"), "{body}");
        assert!(body.contains("- passed — perf: ok"), "{body}");
        assert!(
            body.contains("- **FAILED** — racecheck: exit 3: boom"),
            "{body}"
        );
        // An unseeded run makes no seeding claim.
        assert!(!body.contains("**Seeded:**"), "{body}");
    }

    /// A seeded run's PR discloses the seeding and the seed's content hash; DeepGEMM#5 shipped a
    /// hand-steered diff under an autoresearch banner with neither.
    #[test]
    fn pr_body_discloses_a_pack_seed() {
        let args = <crate::Cli as clap::Parser>::try_parse_from(["crucible"])
            .unwrap()
            .run;
        let paths = crate::Paths::for_manifest(
            std::env::temp_dir(),
            std::env::temp_dir(),
            &std::env::temp_dir(),
            None,
        );
        let rows = vec![Row {
            iter: 1,
            decision: "keep".into(),
            note: "kept it".into(),
            ..Default::default()
        }];
        let body = pr_body(&Record {
            args: &args,
            paths: &paths,
            run_id: "test-run",
            goal: "goal",
            model: "model",
            gate: "gate".into(),
            rows: &rows,
            baseline_score: 2.0,
            best_score: 1.0,
            improved: true,
            kept_shas: &[],
            base_sha: None,
            components: &[],
            published_branches: &[],
            cost_usd: 0.0,
            elapsed: std::time::Duration::ZERO,
            identity_digest: "v2:0",
            seed_hash: "cafe1234cafe1234",
        });
        assert!(body.contains("**Seeded:**"), "{body}");
        assert!(body.contains("`cafe1234cafe1234`"), "{body}");
    }

    /// The bug this guards: a composed workspace runs on a DETACHED HEAD, so kept commits never reached
    /// the fork. This reproduces the exact path, detached HEAD -> `git push HEAD:refs/heads/<branch>`
    /// via the SYSTEM git binary (the libgit2 path silently failed in the no-TLS runtime image), against
    /// a local bare remote, and asserts the branch lands at the kept commit. Hermetic, no network, no
    /// mocks: real git repos + the real `git` binary throughout. (A local-path remote needs no TLS/auth;
    /// the TLS-to-GitHub step is proven separately in-cluster.)
    #[test]
    fn pushes_a_detached_head_tip_to_a_remote_branch() -> Result<()> {
        let root = std::env::temp_dir().join(format!("crucible-pushtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;

        let remote_path = root.join("remote.git");
        git2::Repository::init_bare(&remote_path)?;

        let work = root.join("work");
        let repo = git2::Repository::init(&work)?;
        let sig = git2::Signature::now("autoresearch", "auto@research.local")?;
        std::fs::write(work.join("candidate.txt"), "kept change")?;
        let mut index = repo.index()?;
        index.add_path(Path::new("candidate.txt"))?;
        index.write()?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let tip = repo.commit(Some("HEAD"), &sig, &sig, "kept candidate", &tree, &[])?;

        // Detach HEAD, exactly like the composed world (`git checkout --detach <SHA>`).
        repo.set_head_detached(tip)?;
        assert!(repo.head_detached()?, "precondition: HEAD must be detached");

        // Head branch from the detached HEAD (the kept tip), and a base branch pinned by SHA, the two
        // refs open_draft_pr pushes so the PR diff is pristine-relative.
        let remote = remote_path.to_str().unwrap();
        let branch = "autoresearch/test-run";
        push_ref(&work, remote, "HEAD", branch)?;
        let base_branch = "autoresearch/test-run-base";
        push_ref(&work, remote, &tip.to_string(), base_branch)?;

        let remote_repo = git2::Repository::open_bare(&remote_path)?;
        let pushed = remote_repo
            .find_reference(&format!("refs/heads/{branch}"))
            .context("head branch should exist on the remote after push")?;
        assert_eq!(
            pushed.target(),
            Some(tip),
            "remote head branch must point at the kept tip"
        );
        // Pushing a bare SHA to a branch ref must land that exact commit (the pristine-base path).
        let base = remote_repo
            .find_reference(&format!("refs/heads/{base_branch}"))
            .context("base branch should exist on the remote after push")?;
        assert_eq!(
            base.target(),
            Some(tip),
            "base branch must point at the SHA"
        );

        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    /// The composite multi-fork push: two component repos, each pushing its kept tip + pristine base to
    /// its OWN bare remote (the per-component `git push` half of `open_composite_prs`, minus the `gh pr
    /// create` step which needs network/auth, same boundary the single-repo test draws). Asserts each
    /// fork's head/base branches land at that component's recorded shas, so the two PRs are independent.
    #[test]
    fn composite_pushes_each_component_to_its_own_fork() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("crucible-composite-push-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;

        // Build one component: a work repo with a base commit + a kept commit on top, and a bare remote.
        // Returns (workspace, remote_path, base_sha, head_sha).
        let make =
            |name: &str| -> Result<(std::path::PathBuf, std::path::PathBuf, String, String)> {
                let remote_path = root.join(format!("{name}.git"));
                git2::Repository::init_bare(&remote_path)?;
                let work = root.join(name);
                let repo = git2::Repository::init(&work)?;
                let sig = git2::Signature::now("autoresearch", "auto@research.local")?;
                std::fs::write(work.join("f.txt"), format!("{name} base"))?;
                let mut idx = repo.index()?;
                idx.add_path(Path::new("f.txt"))?;
                idx.write()?;
                let tree = repo.find_tree(idx.write_tree()?)?;
                let base = repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])?;
                // A kept commit on top (the candidate).
                std::fs::write(work.join("f.txt"), format!("{name} edited"))?;
                let mut idx = repo.index()?;
                idx.add_path(Path::new("f.txt"))?;
                idx.write()?;
                let tree = repo.find_tree(idx.write_tree()?)?;
                let parent = repo.find_commit(base)?;
                let head = repo.commit(Some("HEAD"), &sig, &sig, "candidate", &tree, &[&parent])?;
                repo.set_head_detached(head)?; // composed world runs detached
                Ok((work, remote_path, base.to_string(), head.to_string()))
            };

        let run_id = "20260628T000000Z-pd";
        let head_branch = format!("autoresearch/{run_id}");
        let base_branch = format!("autoresearch/{run_id}-base");
        for name in ["vllm", "epp"] {
            let (work, remote_path, base_sha, head_sha) = make(name)?;
            let remote = remote_path.to_str().unwrap();
            // Exactly what open_composite_prs does per component: push the recorded head + base shas.
            push_ref(&work, remote, &head_sha, &head_branch)?;
            push_ref(&work, remote, &base_sha, &base_branch)?;

            let rr = git2::Repository::open_bare(&remote_path)?;
            assert_eq!(
                rr.find_reference(&format!("refs/heads/{head_branch}"))?
                    .target()
                    .map(|o| o.to_string()),
                Some(head_sha),
                "{name} fork head branch at the kept tip"
            );
            assert_eq!(
                rr.find_reference(&format!("refs/heads/{base_branch}"))?
                    .target()
                    .map(|o| o.to_string()),
                Some(base_sha),
                "{name} fork base branch at the pristine sha"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn composite_targets_join_drops_unmapped_components() {
        use crate::crucible::PublishComponent;
        let components = vec![
            PublishComponent {
                name: "vllm".into(),
                workspace: std::path::PathBuf::from("/w/vllm"),
                base_sha: "b1".into(),
                head_sha: "h1".into(),
            },
            PublishComponent {
                name: "coordinator".into(),
                workspace: std::path::PathBuf::from("/w/coordinator"),
                base_sha: "b2".into(),
                head_sha: "h2".into(),
            },
        ];
        let repos = vec![("vllm".to_string(), "wseaton/vllm".to_string())];
        let targets = composite_targets(components, &repos);
        assert_eq!(
            targets.len(),
            1,
            "coordinator has no fork mapping -> dropped"
        );
        assert_eq!(targets[0].name, "vllm");
        assert_eq!(targets[0].repo, "wseaton/vllm");
        assert_eq!(targets[0].head_sha, "h1");
    }

    #[test]
    fn sibling_links_cross_reference_other_prs_only() {
        let opened = vec![
            (
                "vllm".to_string(),
                "wseaton/vllm".to_string(),
                "https://github.com/wseaton/vllm/pull/1".to_string(),
            ),
            (
                "epp".to_string(),
                "wseaton/llm-d-router".to_string(),
                "https://github.com/wseaton/llm-d-router/pull/2".to_string(),
            ),
        ];
        let block = sibling_links("vllm", &opened);
        assert!(
            block.contains("epp: https://github.com/wseaton/llm-d-router/pull/2"),
            "links the sibling: {block}"
        );
        assert!(
            !block.contains("vllm: https://github.com/wseaton/vllm/pull/1"),
            "does not link itself: {block}"
        );
        // A single-component change has no siblings -> no link block.
        assert!(sibling_links("vllm", &opened[..1]).is_empty());
    }

    #[test]
    fn branch_names_are_per_candidate_and_collision_free() {
        let run = "20260716T000000Z-cut-latency";
        // Distinct candidate ids -> distinct head branches, all under the run's namespace.
        let a = head_branch_name(run, "0");
        let b = head_branch_name(run, "1");
        assert_eq!(a, "autoresearch/20260716T000000Z-cut-latency/0");
        assert_ne!(a, b, "distinct candidates never share a head branch");
        assert!(a.starts_with(&format!("autoresearch/{run}/")));
        // The base branch is the head's `-base` sibling and never equals a head branch (heads carry
        // no `-base` suffix because candidate ids are plain indices).
        assert_eq!(base_branch_name(run, "0"), format!("{a}-base"));
        assert_ne!(base_branch_name(run, "0"), a);
        // Cross-candidate: candidate 0's base must not collide with candidate 1's head.
        assert_ne!(base_branch_name(run, "0"), b);
    }

    #[test]
    fn single_repo_run_keeps_one_candidate_today() {
        // The foundation as it stands: one kept candidate (the kept tip on the pristine base).
        // The multi-candidate portfolio that returns several here is the deferred piece.
        let kept = vec!["4d3406b1c9aa0f2e77aa000000000000deadbeef".to_string()];
        let cands = single_repo_candidates("# Cut latency", "run-a", &kept, Some("abc123"));
        assert_eq!(
            cands.len(),
            1,
            "single-repo run keeps exactly one candidate today"
        );
        // Sha-keyed: the branch derives from goal slug + kept tip sha, and the head ref IS the
        // kept sha (a replay's HEAD may sit elsewhere).
        assert_eq!(cands[0].namespace, "cut-latency");
        assert_eq!(cands[0].id, "4d3406b1c9aa");
        assert_eq!(cands[0].head_ref, kept[0]);
        assert_eq!(cands[0].base_sha.as_deref(), Some("abc123"));
        assert_eq!(
            cands[0].head_branch(),
            "autoresearch/cut-latency/4d3406b1c9aa"
        );
        // Resume: no pristine base recorded -> that PR opens against the default branch.
        let no_base = single_repo_candidates("# Cut latency", "run-a", &kept, None);
        assert_eq!(no_base[0].base_sha, None);
        // No kept sha (a log predating kept snapshots): the legacy run-id scheme.
        let legacy = single_repo_candidates("# Cut latency", "run-a", &[], Some("abc123"));
        assert_eq!(legacy[0].head_branch(), "autoresearch/run-a/0");
        assert_eq!(legacy[0].head_ref, "HEAD");
    }

    /// The run-6 failure mode: five resume replays, five run ids, one kept diff, five PRs.
    /// Sha-keyed branches make every replay derive the SAME branch, so the published-branch
    /// check can turn the republish into a no-op.
    #[test]
    fn same_kept_sha_derives_the_same_branch_across_run_ids() {
        let kept = vec!["4d3406b1c9aa0f2e77aa000000000000deadbeef".to_string()];
        let goal = "# GOAL Fold block-scaled FP8 into DeepGEMM";
        let a = single_repo_candidates(goal, "20260804T021753Z-goal", &kept, None);
        let b = single_repo_candidates(goal, "20260804T044928Z-goal", &kept, None);
        assert_eq!(a[0].head_branch(), b[0].head_branch());
        assert_eq!(a[0].base_branch(), b[0].base_branch());
        // A different kept commit is a different candidate: distinct branch.
        let other = vec!["ffff06b1c9aa0f2e77aa000000000000deadbeef".to_string()];
        let c = single_repo_candidates(goal, "20260804T021753Z-goal", &other, None);
        assert_ne!(a[0].head_branch(), c[0].head_branch());
    }

    #[test]
    fn short_sha_truncates_safely() {
        assert_eq!(short_sha("4d3406b1c9aa0f2e"), "4d3406b1c9aa");
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
    }

    /// The DeepGEMM#5 gap: the PR body must carry the whole CANDIDATE.md, the graded
    /// metrics with thresholds, the candidate digest, and which declared checks ran —
    /// not a 120-char note cut mid-word.
    #[test]
    fn kept_section_carries_full_writeup_metrics_digest_and_checks() {
        use crate::session::{EvidenceDisposition, EvidenceEntry};
        let detail = r#"{"evidence":{
            "calc-diff":{"detail":{"digest":"quay.io/x/cand@sha256:d2d3","metrics":{"calc_diff":0.0,"mega_diff":0.000994230754400638,"metric":0.0,"pass":1.0},"threshold":0.001},"digest":"quay.io/x/cand@sha256:d2d3","pass":true,"score":0.0},
            "refcheck":{"detail":{"digest":"quay.io/x/cand@sha256:d2d3","metrics":{"metric":4.4869505444466995e-8,"pass":1.0,"refcheck":4.4869505444466995e-8},"threshold":1e-6},"digest":"quay.io/x/cand@sha256:d2d3","pass":true,"score":0.0}}}"#;
        let row = Row {
            iter: 1,
            decision: "keep".into(),
            note: "# Candidate: Block-scaled FP8 MegaMoE on SM90 (H200)  ## What changed  **Deliverable A (graded this run):** Added a comp".into(),
            detail: detail.into(),
            candidate_md: "# Candidate: Block-scaled FP8 MegaMoE on SM90 (H200)\n\n## What changed\n\n**Deliverable A (graded this run):** Added a composed MegaMoE path.".into(),
            evidence: vec![
                EvidenceEntry {
                    task: "refcheck".into(),
                    disposition: EvidenceDisposition::Passed,
                    note: String::new(),
                },
                EvidenceEntry {
                    task: "tensor-pipe".into(),
                    disposition: EvidenceDisposition::Skipped,
                    note: "worktree setup failed".into(),
                },
            ],
            ..Default::default()
        };
        let s = kept_section(&row);
        // The full writeup, not the truncated note.
        assert!(s.contains("Added a composed MegaMoE path."), "{s}");
        // Metrics with values and thresholds; the `pass` echo dropped.
        assert!(
            s.contains("| calc-diff | mega_diff | 0.000994230754400638 | 0.001 |"),
            "{s}"
        );
        assert!(
            s.contains("| refcheck | refcheck | 4.4869505444466995e-8 | 1e-6 |"),
            "{s}"
        );
        assert!(!s.contains("| pass |"), "{s}");
        // One digest (shared across rungs), printed once.
        assert_eq!(s.matches("Candidate digest:").count(), 1, "{s}");
        assert!(s.contains("`quay.io/x/cand@sha256:d2d3`"), "{s}");
        // Declared checks with dispositions, skipped ones included.
        assert!(
            s.contains("tensor-pipe SKIPPED (worktree setup failed)"),
            "{s}"
        );

        // A row without the full writeup (old log) falls back to the note; no table when the
        // detail isn't grade-shaped.
        let old = Row {
            iter: 2,
            decision: "keep".into(),
            note: "short note".into(),
            detail: "cache_hit=0.83".into(),
            ..Default::default()
        };
        let s = kept_section(&old);
        assert!(s.contains("short note"), "{s}");
        assert!(!s.contains("| check |"), "{s}");
    }

    #[test]
    fn slug_kebabs_a_goal_heading() {
        assert_eq!(
            slug("# Cut EPP p99 latency!\n\nblah"),
            "cut-epp-p99-latency"
        );
        assert_eq!(slug("   \n  Load scorer  tuning  "), "load-scorer-tuning");
        assert_eq!(slug("!!!"), "run");
        assert_eq!(slug(""), "run");
    }

    #[test]
    fn slug_caps_length() {
        let long = "a".repeat(100);
        assert!(slug(&long).len() <= 40);
    }

    #[test]
    fn run_id_is_timestamp_then_slug() {
        let id = run_id("# Speed up routing");
        // <YYYYMMDDTHHMMSSZ>-<slug>; time-first so lexical sort is chronological.
        assert!(id.ends_with("-speed-up-routing"), "got {id}");
        let stamp = id.split('-').next().unwrap_or_default();
        assert_eq!(stamp.len(), 16, "compact UTC stamp, got {stamp}");
        assert!(stamp.ends_with('Z') && stamp.contains('T'), "got {stamp}");
        assert!(
            stamp
                .trim_end_matches('Z')
                .replace('T', "")
                .chars()
                .all(|c| c.is_ascii_digit()),
            "got {stamp}"
        );
    }

    #[test]
    fn utc_stamp_matches_known_epochs() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_782_309_018), "20260624T135018Z");
        assert_eq!(utc_stamp(1_782_309_938), "20260624T140538Z");
    }

    #[test]
    fn keys_partition_by_goal_and_index_is_flat() {
        let k = keys(
            "autoresearch",
            "cut-latency",
            "20260624T140538Z-cut-latency",
        );
        assert_eq!(
            k.run_prefix,
            "autoresearch/runs/cut-latency/20260624T140538Z-cut-latency"
        );
        assert_eq!(
            k.index_key,
            "autoresearch/index/20260624T140538Z-cut-latency.json"
        );
        assert_eq!(k.latest_key, "autoresearch/runs/cut-latency/latest.json");
        // Empty base prefix: no leading slash.
        let k2 = keys("", "g", "id");
        assert_eq!(k2.run_prefix, "runs/g/id");
        assert_eq!(k2.index_key, "index/id.json");
    }

    #[test]
    fn prior_rows_keeps_candidates_drops_baseline_and_headers() {
        let md = "\
# Autoresearch results log\n\n## Goal\ncut ttft\n\n\
| iter | decision | note | detail |\n| --- | --- | --- | --- |\n\
| 0 | baseline | TTFT 2289ms | {} |\n\
| 1 | discard | lock contention tweak | {} |\n\
| 2 | keep | skip double JSON parse | {} |\n";
        let rows = prior_candidate_rows(md);
        // baseline + header + separator dropped; the two real ideas kept, in order.
        assert!(!rows.contains("baseline"), "baseline row dropped: {rows}");
        assert!(!rows.contains("| iter |") && !rows.contains("| --- |"));
        assert!(rows.contains("lock contention tweak"));
        assert!(rows.contains("skip double JSON parse"));
        assert_eq!(rows.lines().count(), 2);
    }

    #[test]
    fn parse_s3_uri_splits_bucket_and_prefix() {
        assert_eq!(
            parse_s3_uri("s3://my-bucket/autoresearch").unwrap(),
            ("my-bucket".into(), "autoresearch".into())
        );
        assert_eq!(
            parse_s3_uri("s3://my-bucket").unwrap(),
            ("my-bucket".into(), String::new())
        );
        assert_eq!(
            parse_s3_uri("s3://my-bucket/a/b/").unwrap(),
            ("my-bucket".into(), "a/b".into())
        );
        assert!(parse_s3_uri("https://nope").is_err());
        assert!(parse_s3_uri("s3:///just-prefix").is_err());
    }

    #[test]
    fn finite_drops_non_finite() {
        assert_eq!(finite(1.5), Some(1.5));
        assert_eq!(finite(f64::INFINITY), None);
        assert_eq!(finite(f64::NAN), None);
    }
}
