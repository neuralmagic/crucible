//! The controller-to-engine boundary. The controller ships as its own `crucible-controller`
//! binary, separate from the `crucible` engine, and neither links the other as a library — this
//! crate cannot call `scope::run` / `deploy` as library functions. The reconcile path therefore
//! drives the engine as a **subprocess** (the contract's sanctioned "subprocess where a library
//! call doesn't exist"), which is also what the per-candidate ingest already needs: it re-parses
//! the session log the run wrote, never a shared process handle.
//!
//! The engine is a sibling binary staged on `PATH` in both images, so [`resolve_bin`] defaults to
//! the bare `crucible` name; `CRUCIBLE_BIN` overrides it (tests point it at a scripted stand-in,
//! the `scope.rs` command-backend pattern one level up).

#![allow(clippy::disallowed_macros)]

use crate::model::ParkReason;
use anyhow::{Context, Result, bail};
use crucible_contract::{Disposition, Tier};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One stage of the scope pipeline, mirrored from `crucible::scope::StageResult` on the wire (the
/// second consumer of that shape — a plain serde mirror, decoupled from the
/// engine's own type).
#[derive(Debug, Clone, Deserialize)]
pub struct ScopeStage {
    name: String,
    pub(crate) passed: bool,
    detail: String,
}

/// `crucible scope --json` output (`crucible::scope::ScopeReport`), decoded here.
#[derive(Debug, Clone, Deserialize)]
pub struct ScopeReport {
    pub(crate) stages: Vec<ScopeStage>,
    pub(crate) digest: Option<String>,
    cost: Option<f64>,
    /// The exact JSON the engine printed, kept verbatim for the `scope_reports` store — the refine
    /// rounds ride only here (this mirror deliberately decodes just the fields the reconcile
    /// gates on). Set by the parser, never on the wire.
    #[serde(skip)]
    pub(crate) raw: String,
    /// The turn's preserved agent transcript (gzipped session NDJSON), when the turn delivered
    /// one: the local executor reads it off `--transcript-out`, the pod executor scrapes the
    /// `CRUCIBLE_SCOPE_TRANSCRIPT:` marker line. Never on the report's own wire.
    #[serde(skip)]
    pub(crate) transcript_gz: Option<Vec<u8>>,
    /// The surviving pack itself (gzip'd tar of the pack dir), scraped off the pod executor's
    /// `CRUCIBLE_SCOPE_PACK:` marker line. The local executor never sets it — its pack is tarred
    /// off the scratch tree it wrote. Never on the report's own wire.
    #[serde(skip)]
    pub(crate) pack_tgz: Option<Vec<u8>>,
    /// Why the pack marker was present but unusable (the engine's `{"error":…}` payload for an
    /// oversize pack, or a garbled base64) — evidence for the loud handoff failure. `None` when
    /// the blob landed or no marker existed at all.
    #[serde(skip)]
    pub(crate) pack_error: Option<String>,
}

impl ScopeReport {
    /// A pack survives when the pipeline froze an identity (a digest) and no stage said no. A
    /// failing check or selftest leaves `digest` empty and a `passed = false` stage behind.
    pub(crate) fn survived(&self) -> bool {
        self.digest.is_some() && self.stages.iter().all(|s| s.passed)
    }

    /// The turn's cost, defaulting to 0 when the report omitted it (a non-propose pipeline).
    pub(crate) fn cost_usd(&self) -> f64 {
        self.cost.unwrap_or(0.0)
    }

    /// Why the pipeline stopped: the first failing stage's name+detail, or the no-pack catch-all.
    /// The reason a dead proposal parks with.
    pub(crate) fn failure_reason(&self) -> ParkReason {
        self.stages
            .iter()
            .find(|s| !s.passed)
            .map(|s| ParkReason::ScopeFailed {
                stage: s.name.clone(),
                detail: s.detail.clone(),
            })
            .unwrap_or(ParkReason::ScopeProducedNoPack)
    }
}

/// The item a run is parameterized by, read by `crucible_contract::outputs::DefaultTargets` as the
/// engine default target for `tracker-comment`. Unset, the engine refuses every write of that kind.
pub(crate) const ITEM_ENV: &str = "CRUCIBLE_ITEM";

/// The `crucible` engine binary: `CRUCIBLE_BIN` if set, else the bare name on `PATH` (both images
/// stage the engine alongside this binary).
pub fn resolve_bin() -> PathBuf {
    match std::env::var_os("CRUCIBLE_BIN") {
        Some(bin) => PathBuf::from(bin),
        None => PathBuf::from("crucible"),
    }
}

/// Download one object by shelling `crucible fetch --uri … --out …` — S3 access stays behind the
/// engine subprocess boundary; local paths never come here. A nonzero exit carries the engine's
/// stderr verbatim: the invoking shell may lack the pod's S3 credentials, and that stderr is the
/// only diagnostic.
pub async fn fetch_object(uri: &str, out: &Path) -> Result<()> {
    let bin = resolve_bin();
    crate::runs::workpod::admit_contract(
        crate::runs::contract::RequestKind::Fetch,
        &[crate::runs::contract::DispatchTarget::Binary(bin.clone())],
    )
    .await?;
    let output = tokio::process::Command::new(&bin)
        .arg("fetch")
        .arg("--uri")
        .arg(uri)
        .arg("--out")
        .arg(out)
        .output()
        .await
        .with_context(|| format!("spawning `{} fetch`", bin.display()))?;
    if !output.status.success() {
        bail!(
            "`crucible fetch {uri}` exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The clone URL for a watched `owner/repo` (scope's `--repo`, the code under test). A value that
/// already looks like a URL or local path is passed through untouched.
pub(crate) fn repo_clone_url(repo: &str) -> String {
    if repo.contains("://") || repo.contains('@') || repo.starts_with('/') || repo.ends_with(".git")
    {
        repo.to_string()
    } else {
        format!("https://github.com/{repo}.git")
    }
}

/// A code-grounded ranking verdict, parsed off `crucible rank-grounded --json`'s stdout: the
/// engine ran one sandboxed turn over the checkout and printed this. The escalation tier's answer —
/// it overrides the text-only ranker's tier, or says the issue is `stale`.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundedVerdict {
    pub(crate) disposition: Disposition,
    pub(crate) rationale: String,
    pub(crate) confidence: Option<String>,
    /// The grounded turn's own cost (USD), ledgered as `rank-grounded`.
    pub(crate) cost_usd: f64,
}

/// The per-repo checkout the controller maintains for grounded ranking, under
/// `<scratch_dir>/checkouts/<owner>-<repo>`. One shared checkout per repo, reset per use;
/// a cold scratch dir just means a re-clone.
pub(crate) fn checkout_dir(scratch_dir: &Path, repo: &str) -> PathBuf {
    scratch_dir.join("checkouts").join(repo.replace('/', "-"))
}

/// `git -C <dir?> <args>` returning `Ok` only on success.
fn run_git(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .status()
        .context("running `git` (is it on PATH?)")?;
    if !status.success() {
        bail!("git {:?} failed ({status})", args.first().unwrap_or(&""));
    }
    Ok(())
}

/// Ensure the per-repo checkout at `dir` exists and is current: clone `repo_url` if absent, else
/// fetch + hard-reset it to the remote's tracked branch. A refresh failure on an existing checkout
/// is best-effort (a transient fetch hiccup falls back to the checkout as-is rather than failing the
/// whole reconcile); a first clone that fails is a hard error (there's nothing to fall back to).
pub(crate) fn ensure_checkout(repo_url: &str, dir: &Path) -> Result<()> {
    if dir.join(".git").is_dir() {
        if let Err(e) = refresh_checkout(dir) {
            tracing::warn!(
                checkout = %dir.display(),
                error = format!("{e:#}"),
                "rank-grounded: refreshing checkout failed, using it as-is"
            );
        }
        return Ok(());
    }
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating checkouts dir {}", parent.display()))?;
    }
    run_git(&["clone", repo_url, &dir.to_string_lossy()])
        .with_context(|| format!("cloning {repo_url} into {}", dir.display()))
}

/// Fetch + hard-reset an existing checkout to its tracked upstream (`@{u}`), then drop any stray
/// files. Keeps the shared checkout deterministic between grounded turns.
fn refresh_checkout(dir: &Path) -> Result<()> {
    let d = dir.to_string_lossy().to_string();
    run_git(&["-C", &d, "fetch", "--prune", "origin"])?;
    run_git(&["-C", &d, "reset", "--hard", "@{u}"])?;
    run_git(&["-C", &d, "clean", "-fdx"])?;
    Ok(())
}

/// Run `crucible rank-grounded --json` for one issue against `workspace`, parsing the verdict off
/// stdout. Blocking — the caller runs it under `spawn_blocking`, having first [`ensure_checkout`]ed
/// the workspace. The verdict is parsed regardless of exit code (an over-budget turn exits nonzero
/// but still prints its verdict, the `scope_propose` tolerance); only a missing verdict is an error.
///
/// This is the `local`-executor arm only (dev machines, where a `claude` CLI exists): the engine
/// runs the turn with its default backend. In-cluster the loop image has no `claude` binary, so the
/// `pod` executor dispatches a WorkPod instead ([`crate::runs::workpod`]) — there is no silent env gate
/// picking a backend here anymore.
pub(crate) fn rank_grounded(
    bin: &Path,
    issue_key: &str,
    workspace: &Path,
    max_cost: f64,
    agent_cmd: Option<&str>,
) -> Result<GroundedVerdict> {
    let workspace = workspace.to_string_lossy().to_string();
    let max_cost = format!("{max_cost}");
    let mut cmd = Command::new(bin);
    cmd.env(ITEM_ENV, issue_key);
    cmd.args([
        "rank-grounded",
        "--json",
        "--issue",
        issue_key,
        "--workspace",
        &workspace,
        "--max-cost",
        &max_cost,
    ]);
    // The comparison harness's deterministic test seam, threaded down to the engine's own turn.
    if let Some(cmd_str) = agent_cmd {
        cmd.args(["--agent-cmd", cmd_str]);
    }
    let output = cmd
        .output()
        .with_context(|| format!("spawning `{} rank-grounded`", bin.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_grounded_verdict(&stdout).with_context(|| {
        format!(
            "parsing `crucible rank-grounded --json` output (exit {:?}): stderr={:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

/// Parse the last JSON line rank-grounded prints: a verdict object, or an `{"error":…}` object when
/// the turn produced no parseable verdict (which is an error to the caller, who keeps the text tier).
fn parse_grounded_verdict(stdout: &str) -> Result<GroundedVerdict> {
    let line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .with_context(|| format!("rank-grounded printed no output: {stdout:?}"))?;
    verdict_from_json_line(line.trim())
}

/// Decode one verdict JSON object (the shape both `rank-grounded --json` and the WorkPod
/// `CRUCIBLE_VERDICT:` marker carry) into a [`GroundedVerdict`]. Decoded through the engine's own
/// [`crucible_contract::GroundedVerdict`], so the fields it already models — the failure's kind,
/// the tail the agent printed on its way down, the turn's cost — arrive as data instead of being
/// re-read off a `Value` here. A turn that produced no verdict surfaces as `Err` naming both (the
/// caller keeps the text tier). Shared by the `local` subprocess arm and [`crate::runs::workpod`]'s
/// pod-log scraper.
pub(crate) fn verdict_from_json_line(line: &str) -> Result<GroundedVerdict> {
    let wire: crucible_contract::GroundedVerdict = crucible_contract::json::from_str(line)
        .with_context(|| format!("rank-grounded output is not a verdict: {line:?}"))?;
    match wire {
        crucible_contract::GroundedVerdict::Failed {
            error,
            error_kind,
            output_tail,
            cost_usd,
            ..
        } => {
            // The kind separates a turn that never ran from one that ran and said nothing, and the
            // tail is the agent's own last words. Both were on the wire and read by nobody.
            let tail = output_tail
                .filter(|t| !t.trim().is_empty())
                .map(|t| format!("; the agent printed: {}", t.trim()))
                .unwrap_or_default();
            bail!(
                "grounded ranker produced no verdict ({}, ${cost_usd:.4} spent): {error}{tail}",
                error_kind.as_str()
            )
        }
        crucible_contract::GroundedVerdict::Ruled {
            tier,
            rationale,
            confidence,
            cost_usd,
            ..
        } => Ok(GroundedVerdict {
            disposition: tier,
            rationale,
            confidence,
            cost_usd,
        }),
    }
}

/// Run `crucible scope --propose --json` for one issue: draft a pack into `out` from the issue's
/// goal, validate it (`crucible check` + selftest), and freeze a `SCOPE.md`. Blocking — the caller
/// runs it under `spawn_blocking`. The report is parsed from stdout even on a non-zero exit (a
/// failed pipeline still prints its JSON, then exits 1), so a `passed = false` stage is data, not
/// an error; only a missing/garbled report is a hard error.
///
/// `tier` is the issue's confirmed tier, forwarded as `--tier t0|t1` so the propose
/// prompt drafts the right gate shape. Only `T0`/`T1` have an engine-side `--tier` spelling; any
/// other tier (shouldn't reach a scope turn at all — `tier_gate` excludes it — but this function
/// doesn't re-litigate that) is passed through with no `--tier` flag, which the engine defaults to
/// `T0` behavior. `gaming_refine_rounds` is the effective gaming-review refine bound, forwarded as
/// `--gaming-refine-rounds`. `skip_gaming_review` is the operator escape hatch (demo/bring-up
/// postures where the review's fail-closed loop blocks the first e2e run through a new rig): when
/// true, forwarded as `--skip-gaming-review` and `gaming_refine_rounds` is omitted entirely.
/// `authoritative` marks the goal an authoritative brief, forwarded as `--authoritative` so the
/// propose/refine prompts preserve its prescriptions instead of de-prescribing them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scope_propose(
    bin: &Path,
    issue_key: &str,
    repo: &str,
    out: &Path,
    max_cost: f64,
    tier: Option<Tier>,
    gaming_refine_rounds: u32,
    skip_gaming_review: bool,
    goal_text: Option<&str>,
    authoritative: bool,
) -> Result<ScopeReport> {
    let out = out.to_string_lossy().to_string();
    let repo_url = repo_clone_url(repo);
    let max_cost = format!("{max_cost}");
    // The turn's transcript pickup: the engine writes the gzipped session NDJSON here; the scratch
    // dir (and the file) is deleted once the bytes are read back below.
    let transcript_dir = tempfile::tempdir().context("scope transcript scratch dir")?;
    let transcript_path = transcript_dir.path().join("transcript.jsonl.gz");
    let mut args = vec![
        "scope".to_string(),
        "--propose".to_string(),
        "--json".to_string(),
        "--force".to_string(),
    ];
    // A non-upstream issue (e.g. an adopted scenario) has no GitHub item to fetch: its goal is the
    // free text ledgered at adoption, written to a file in the turn's own scratch dir and passed via
    // `--goal-file` (which `--issue` conflicts with at the CLI) — the engine's existing local-file
    // `Ingest` arm, no network fetch.
    if let Some(text) = goal_text {
        let goal_path = transcript_dir.path().join("goal.md");
        std::fs::write(&goal_path, text)
            .with_context(|| format!("writing scope goal file {}", goal_path.display()))?;
        args.push("--goal-file".to_string());
        args.push(goal_path.to_string_lossy().to_string());
    } else {
        args.push("--issue".to_string());
        args.push(issue_key.to_string());
    }
    args.extend([
        "--repo".to_string(),
        repo_url,
        "--out".to_string(),
        out,
        "--max-cost".to_string(),
        max_cost,
        "--transcript-out".to_string(),
        transcript_path.to_string_lossy().to_string(),
    ]);
    if skip_gaming_review {
        args.push("--skip-gaming-review".to_string());
    } else {
        args.push("--gaming-refine-rounds".to_string());
        args.push(format!("{gaming_refine_rounds}"));
    }
    if let Some(t) = tier.and_then(|t| match t {
        Tier::T0 => Some("t0"),
        Tier::T1 => Some("t1"),
        Tier::T2 | Tier::T3 | Tier::N => None,
    }) {
        args.push("--tier".to_string());
        args.push(t.to_string());
    }
    if authoritative {
        args.push("--authoritative".to_string());
    }
    let output = Command::new(bin)
        .args(&args)
        .env(ITEM_ENV, issue_key)
        .output()
        .with_context(|| format!("spawning `{} scope --propose`", bin.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut report: ScopeReport = serde_json::from_str(stdout.trim()).with_context(|| {
        format!(
            "parsing `crucible scope --json` output (exit {:?}): stdout={:?} stderr={:?}",
            output.status.code(),
            stdout,
            String::from_utf8_lossy(&output.stderr),
        )
    })?;
    report.raw = stdout.trim().to_string();
    // Absent/empty file = the turn streamed no transcript. Never fail the report over it.
    report.transcript_gz = std::fs::read(&transcript_path)
        .ok()
        .filter(|b| !b.is_empty());
    Ok(report)
}

/// The pushed branch pair for a pack: the pristine (empty) base and the pack head, so the draft PR's
/// diff is *exactly* the pack files (the `publish.rs` pinned-base trick). Deterministic per issue, so
/// a re-open reuses the branches rather than spawning duplicates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackBranches {
    base: String,
    head: String,
}

/// The branch names for an issue's pack PR: `crucible-pack/<key>[-base]`, the key sanitized to a
/// git-ref-safe token (`owner/repo#7` → `owner_repo_7`).
fn pack_branches(issue_key: &str) -> PackBranches {
    let safe = crate::model::sanitize_key(issue_key);
    PackBranches {
        base: format!("crucible-pack/{safe}-base"),
        head: format!("crucible-pack/{safe}"),
    }
}

/// The repo (`owner/repo`) the approval PRs open against — `CONTROLLER_PACK_REPO`. Unset means the
/// approval isn't configured, so [`open_pack_pr`] returns `Ok(None)` and reconcile leaves the row at
/// `scoped` (the harmless pre-lane-E no-op, retried on the next re-enqueue).
pub(crate) fn pack_pr_repo() -> Option<String> {
    std::env::var("CONTROLLER_PACK_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// The forge PAT for pushing the pack + opening the PR — `AUTORESEARCH_PR_TOKEN` (fallback
/// `GITHUB_TOKEN`/`GH_TOKEN`), the same credential the publisher and the broker share. The
/// fallback arm of [`resolve_pack_pr_token`]; deploys with a GitHub App configured never get here.
fn pack_pr_token() -> Option<String> {
    std::env::var("AUTORESEARCH_PR_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|s| !s.is_empty())
}

/// The pack-PR credential: an App installation token when the deploy configured one
/// (`cfg.github_app`, from the `CONTROLLER_GITHUB_APP_*` env vars — the durable path, since the
/// org's token policy forbids long-lived PATs), else the [`pack_pr_token`] env chain. A configured
/// App that fails to mint is an error, never a silent PAT fallback — the retry re-drives it.
/// Installation tokens are documented to work both as the `x-access-token` git password and as
/// `GH_TOKEN` for `gh`, so one credential serves both halves of [`open_pack_pr`].
pub(crate) async fn resolve_pack_pr_token(
    cfg: &crate::config::ControllerCfg,
) -> Result<Option<String>> {
    resolve_pack_pr_token_for(cfg.github_app.as_ref()).await
}

/// [`resolve_pack_pr_token`] against the App source alone, for callers that hold the source rather
/// than the whole parsed config.
pub(crate) async fn resolve_pack_pr_token_for(
    app: Option<&crate::secrets::github_app::GithubAppTokenSource>,
) -> Result<Option<String>> {
    if let Some(app) = app {
        let token = app
            .token()
            .await
            .context("minting the GitHub App installation token for the pack PR")?;
        return Ok(Some(token));
    }
    Ok(pack_pr_token())
}

/// The approval-PR title + body: the reviewable diff is the pack itself; the body
/// carries the validation evidence (the frozen `SCOPE.md`, which records the identity digest and the
/// check/self-test outcome) so a human approves the harness before any budget is spent.
fn pack_pr_body(issue_key: &str, scope_md: Option<&str>) -> String {
    let mut s = format!(
        "**Scope-pack approval gate.** The autoresearch controller proposed a domain \
         pack for `{issue_key}` and it passed `crucible check` + the gate self-test. The diff in \
         this PR *is* the pack (manifest, gate, negative controls, `SCOPE.md`).\n\n\
         Approve (a PR review approval, or a `/approve` comment) to let the loop spend budget on \
         this issue; close this PR to bounce it back to re-scope. The loop never auto-merges and \
         never auto-approves — this approval cannot be turned off.\n\n"
    );
    match scope_md {
        Some(md) if !md.trim().is_empty() => {
            s.push_str("**Validation evidence (`SCOPE.md`):**\n\n```\n");
            // Cap the embedded evidence so a huge SCOPE.md can't balloon the PR body.
            let capped: String = md.chars().take(4000).collect();
            s.push_str(&capped);
            s.push_str("\n```\n");
        }
        _ => s.push_str("_(no `SCOPE.md` found in the pack — the freeze evidence is missing.)_\n"),
    }
    s
}

/// Open the approval-gate draft PR for a frozen pack. Pushes the pack as a branch
/// pair (a pristine empty base + the pack head) to `CONTROLLER_PACK_REPO` and opens a DRAFT PR whose
/// diff is exactly the pack, reusing the `publish.rs` git+gh plumbing shape. Returns the PR url, or
/// `Ok(None)` when no pack repo is configured (the approval is off — reconcile leaves the row `scoped`).
/// Idempotent: an already-open PR for the head branch is returned rather than duplicated.
/// `token` comes from [`resolve_pack_pr_token`] (App installation token or PAT — both work in
/// the push URL and as `GH_TOKEN`); resolution stays with the async caller because the App mint
/// is an HTTP exchange, while this function is the blocking git/gh half.
pub(crate) fn open_pack_pr(
    issue_key: &str,
    pack_out: &Path,
    token: Option<String>,
) -> Result<Option<String>> {
    let Some(repo) = pack_pr_repo() else {
        return Ok(None);
    };
    let scope_md = std::fs::read_to_string(pack_out.join("SCOPE.md")).ok();
    let title = format!("[scope] approve pack for {issue_key}");
    let body = pack_pr_body(issue_key, scope_md.as_deref());
    open_draft_pr(&repo, issue_key, pack_out, &title, &body, token.as_deref()).map(Some)
}

/// Push a tree as a branch pair to `repo` and open the draft PR whose diff is exactly that tree.
/// `branch_key` names the pair through [`pack_branches`], so re-opening the same key reuses the
/// branches rather than spawning duplicates. Blocking; `token` is resolved by the async caller.
pub(crate) fn open_draft_pr(
    repo: &str,
    branch_key: &str,
    pack_out: &Path,
    title: &str,
    body: &str,
    token: Option<&str>,
) -> Result<String> {
    let branches = pack_branches(branch_key);

    // Idempotent: if the PR already exists for this head branch, return it without re-pushing.
    if let Some(url) = find_pack_pr(repo, &branches.head, token)? {
        return Ok(url);
    }

    let push_url = pack_push_url(repo, token);
    push_pack_branches(pack_out, &push_url, &branches)
        .with_context(|| format!("pushing pack branches for {branch_key} to {repo}"))?;

    gh_open_draft_pr(repo, title, body, &branches, token)
        .with_context(|| format!("opening the draft PR on {repo}"))
}

/// The authenticated push URL (`x-access-token` is GitHub's token-as-password convention, and it
/// accepts App installation tokens (`ghs_…`) exactly like PATs — documented GitHub behavior, so
/// the App path needs no separate plumbing here). Kept out
/// of every error message (see [`push_pack_branches`]) so the token never lands in a log. No token →
/// a plain URL, relying on the ambient git credential helper.
fn pack_push_url(repo: &str, token: Option<&str>) -> String {
    match token {
        Some(t) => format!("https://x-access-token:{t}@github.com/{repo}.git"),
        None => format!("https://github.com/{repo}.git"),
    }
}

/// Turn the pack dir into a one-commit git repo on top of an empty base and push both refs. The
/// empty base pinned as a branch makes the PR diff exactly the pack files, pristine-relative (the
/// `publish.rs` base-branch trick). Uses the SYSTEM git binary (the runtime image's libgit2 has no
/// TLS backend — the same reason `publish::push_ref` shells git). `--force` makes a re-push idempotent.
fn push_pack_branches(pack_out: &Path, push_url: &str, branches: &PackBranches) -> Result<()> {
    let git = |args: &[&str]| -> Result<()> {
        let status = Command::new("git")
            .arg("-C")
            .arg(pack_out)
            .args(args)
            .status()
            .context("running `git` (is it on PATH?)")?;
        if !status.success() {
            // `args` never carries the token (that's only in the push URL, handled separately).
            bail!("git {:?} failed ({status})", args.first().unwrap_or(&""));
        }
        Ok(())
    };

    // Fresh repo (idempotent on re-run), identity set locally so no global config is required.
    git(&["init", "-q"])?;
    git(&["config", "user.email", "autoresearch@crucible.local"])?;
    git(&["config", "user.name", "crucible autoresearch"])?;
    // The pristine empty base commit, then the pack head on top of it.
    git(&["checkout", "-q", "-B", "crucible-pack-base"])?;
    git(&["commit", "-q", "--allow-empty", "-m", "pristine base"])?;
    let base_sha = git_stdout(pack_out, &["rev-parse", "HEAD"])?;
    git(&["checkout", "-q", "-B", "crucible-pack-head"])?;
    git(&["add", "-A"])?;
    git(&["commit", "-q", "--allow-empty", "-m", "scope pack"])?;

    // Push the head (kept commit) and the pinned base. `git push` keeps the token-bearing URL out of
    // its own error output only if we do — so wrap failures with a token-free message.
    push_ref(pack_out, push_url, "HEAD", &branches.head)?;
    push_ref(pack_out, push_url, base_sha.trim(), &branches.base)?;
    Ok(())
}

/// `git -C <dir> <args>` capturing stdout (trimmed by the caller).
fn git_stdout(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running `git`")?;
    if !out.status.success() {
        bail!(
            "git {:?} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Push `local_ref` (a branch, `HEAD`, or a bare SHA) to `url` as `refs/heads/{branch}`. `--force`
/// makes a re-push idempotent. `url` may carry the PAT, so it is excluded from the error message.
fn push_ref(workspace: &Path, url: &str, local_ref: &str, branch: &str) -> Result<()> {
    let refspec = format!("{local_ref}:refs/heads/{branch}");
    let status = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .arg("push")
        .arg("--force")
        .arg(url)
        .arg(&refspec)
        .status()
        .context("running `git push`")?;
    if !status.success() {
        bail!("git push of {refspec} failed ({status})");
    }
    Ok(())
}

/// The existing draft PR's url for `head`, if one is already open (idempotency). Shells `gh pr list`.
fn find_pack_pr(repo: &str, head: &str, token: Option<&str>) -> Result<Option<String>> {
    let mut cmd = Command::new("gh");
    cmd.args([
        "pr", "list", "--repo", repo, "--head", head, "--state", "open",
    ])
    .args(["--json", "url", "--jq", ".[0].url // \"\""]);
    if let Some(t) = token {
        cmd.env("GH_TOKEN", t);
    }
    let out = cmd.output().context("running `gh pr list`")?;
    if !out.status.success() {
        bail!(
            "gh pr list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!url.is_empty()).then_some(url))
}

/// Open the DRAFT PR via `gh pr create` (already in the loop/controller image; the broker + the
/// publisher shell `gh` too). Both head and base are branches in `repo`, so it's an internal PR.
fn gh_open_draft_pr(
    repo: &str,
    title: &str,
    body: &str,
    branches: &PackBranches,
    token: Option<&str>,
) -> Result<String> {
    let mut cmd = Command::new("gh");
    cmd.args([
        "pr",
        "create",
        "--repo",
        repo,
        "--draft",
        "--head",
        &branches.head,
        "--base",
        &branches.base,
        "--title",
        title,
        "--body",
        body,
    ]);
    if let Some(t) = token {
        cmd.env("GH_TOKEN", t);
    }
    let out = cmd.output().context("running `gh pr create`")?;
    if !out.status.success() {
        bail!(
            "gh pr create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let url = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("https://"))
        .unwrap_or("")
        .to_string();
    if url.is_empty() {
        bail!("gh pr create succeeded but printed no PR url");
    }
    Ok(url)
}

/// Encode an issue key into a Kubernetes label *value* (alphanumeric plus `-_.`, ≤63 chars, must
/// start/end alphanumeric) for [`crate::daemon::ISSUE_KEY_LABEL`]: every other char (`/`, `#`, …)
/// becomes `-`. `owner/repo#42` → `owner-repo-42`. Deliberately lossy — a human-readable hint
/// only. The key the watch reads back rides [`crate::daemon::ISSUE_KEY_ANNOTATION`] verbatim
/// (annotation values are unrestricted, so it round-trips exactly).
pub(crate) fn issue_key_label_value(key: &str) -> String {
    let mut v: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    v.truncate(63);
    let trimmed = v.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_contract::Tier;

    #[test]
    fn repo_clone_url_builds_github_https_for_a_bare_slug() {
        assert_eq!(
            repo_clone_url("neuralmagic/crucible"),
            "https://github.com/neuralmagic/crucible.git"
        );
    }

    #[test]
    fn repo_clone_url_passes_through_urls_and_paths() {
        assert_eq!(
            repo_clone_url("https://example.com/x.git"),
            "https://example.com/x.git"
        );
        assert_eq!(repo_clone_url("/abs/local/repo"), "/abs/local/repo");
        assert_eq!(
            repo_clone_url("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
    }

    #[test]
    fn scope_report_survived_needs_a_digest_and_all_passed() {
        let ok = ScopeReport {
            stages: vec![ScopeStage {
                name: "validate".into(),
                passed: true,
                detail: "ok".into(),
            }],
            digest: Some("v1:abc".into()),
            cost: Some(0.5),
            raw: String::new(),
            transcript_gz: None,
            pack_tgz: None,
            pack_error: None,
        };
        assert!(ok.survived());
        assert_eq!(ok.cost_usd(), 0.5);

        let failed = ScopeReport {
            stages: vec![
                ScopeStage {
                    name: "propose".into(),
                    passed: true,
                    detail: "drafted".into(),
                },
                ScopeStage {
                    name: "validate".into(),
                    passed: false,
                    detail: "measure_cmd missing".into(),
                },
            ],
            digest: None,
            cost: None,
            raw: String::new(),
            transcript_gz: None,
            pack_tgz: None,
            pack_error: None,
        };
        assert!(!failed.survived());
        assert_eq!(failed.cost_usd(), 0.0);
        assert_eq!(
            failed.failure_reason(),
            ParkReason::ScopeFailed {
                stage: "validate".to_string(),
                detail: "measure_cmd missing".to_string(),
            }
        );
    }

    #[test]
    fn issue_key_label_value_is_k8s_label_safe() {
        assert_eq!(issue_key_label_value("owner/repo#42"), "owner-repo-42");
        assert_eq!(issue_key_label_value("a_b.c-1"), "a_b.c-1");
        let v = issue_key_label_value("owner/repo#42");
        assert!(v.len() <= 63);
        assert!(v.chars().next().unwrap().is_ascii_alphanumeric());
        assert!(v.chars().last().unwrap().is_ascii_alphanumeric());
    }

    #[test]
    fn resolve_bin_honors_the_env_override() {
        // Serialize on the crate-wide env lock: `set_var`/`remove_var` race any other test that
        // reads the environ or spawns a subprocess (the whole crate shares one guard). A sync test
        // runs outside any tokio runtime, so `blocking_lock` is the sanctioned acquire.
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::set_var("CRUCIBLE_BIN", "/tmp/fake-crucible");
        }
        assert_eq!(resolve_bin(), PathBuf::from("/tmp/fake-crucible"));
        unsafe {
            std::env::remove_var("CRUCIBLE_BIN");
        }
    }

    #[test]
    fn pack_branches_are_ref_safe_and_deterministic() {
        let b = pack_branches("owner/repo#7");
        assert_eq!(b.head, "crucible-pack/owner_repo_7");
        assert_eq!(b.base, "crucible-pack/owner_repo_7-base");
        // Ref-safe: no `/`-in-key, `#`, or `:` leaks through (the sanitizer maps them to `_`).
        assert!(
            !b.head
                .trim_start_matches("crucible-pack/")
                .contains(['#', ':'])
        );
        // Deterministic so a re-open reuses the branch.
        assert_eq!(pack_branches("owner/repo#7"), b);
    }

    #[test]
    fn pack_pr_body_embeds_the_scope_evidence_and_the_approval_framing() {
        let body = pack_pr_body("owner/repo#7", Some("identity: v1:deadbeef\ncheck: PASS"));
        assert!(body.contains("owner/repo#7"));
        assert!(body.contains("Scope-pack approval gate"));
        assert!(
            body.contains("/approve"),
            "tells the reviewer how to approve"
        );
        assert!(body.contains("v1:deadbeef"), "embeds the SCOPE.md evidence");
        // No evidence → says so rather than lying about it.
        let none = pack_pr_body("owner/repo#7", None);
        assert!(none.contains("no `SCOPE.md`"));
    }

    /// The git half of `open_pack_pr` against a local bare remote (the `gh pr create` leg needs
    /// network/auth, the same boundary `publish.rs` draws). Hermetic: real `git` throughout, no
    /// mocks. Asserts the pack's head + pristine base branches land on the remote.
    #[test]
    fn push_pack_branches_pushes_head_and_base_to_a_remote() -> Result<()> {
        let root = std::env::temp_dir().join(format!("crucible-packpush-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;

        let remote = root.join("remote.git");
        run_git(&["init", "--bare", "-q", remote.to_str().unwrap()])?;

        let pack = root.join("pack");
        std::fs::create_dir_all(&pack)?;
        std::fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n")?;
        std::fs::write(pack.join("SCOPE.md"), "identity: v1:beef\n")?;

        let branches = pack_branches("owner/repo#7");
        push_pack_branches(&pack, remote.to_str().unwrap(), &branches)?;

        let refs = String::from_utf8(
            Command::new("git")
                .args(["ls-remote", "--heads", remote.to_str().unwrap()])
                .output()?
                .stdout,
        )?;
        assert!(
            refs.contains(&format!("refs/heads/{}", branches.head)),
            "head branch pushed: {refs}"
        );
        assert!(
            refs.contains(&format!("refs/heads/{}", branches.base)),
            "pristine base branch pushed: {refs}"
        );

        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    /// `fetch_object` shells `CRUCIBLE_BIN fetch` (here a script) and surfaces a nonzero exit's
    /// stderr — the only diagnostic when the operator's shell lacks S3 credentials.
    #[tokio::test]
    async fn fetch_object_shells_the_engine_and_surfaces_stderr() {
        let _g = crate::ENV_LOCK.lock().await;
        let root = std::env::temp_dir().join(format!("crucible-fetchobj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let bin = root.join("crucible-fake");
        crate::testing::write_exec(
            &bin,
            "#!/bin/sh\n[ \"$1\" = fetch ] || exit 1\nshift\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --uri) uri=\"$2\"; shift 2;;\n    --out) out=\"$2\"; shift 2;;\n    *) shift;;\n  esac\ndone\ncase \"$uri\" in\n  s3://b/ok*) printf 'fetched' > \"$out\";;\n  *) echo 'AccessDenied: no credentials' >&2; exit 1;;\nesac\n",
        );
        unsafe {
            std::env::set_var("CRUCIBLE_BIN", &bin);
        }

        let out = root.join("session.jsonl");
        fetch_object("s3://b/ok/session.jsonl", &out)
            .await
            .expect("fetch succeeds");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "fetched");

        let err = fetch_object("s3://b/missing", &out).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("AccessDenied: no credentials"),
            "stderr rides the error: {err:#}"
        );

        unsafe {
            std::env::remove_var("CRUCIBLE_BIN");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The pack-PR credential preference order: a configured GitHub App wins over a set PAT
    /// (minting through a wiremock exchange, never the real GitHub); no App ⇒ the env chain.
    #[tokio::test]
    async fn resolve_pack_pr_token_prefers_the_app_over_the_pat_chain() -> Result<()> {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _g = crate::ENV_LOCK.lock().await;
        let vars = ["AUTORESEARCH_PR_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"];
        let prior: Vec<_> = vars.iter().map(std::env::var_os).collect();
        unsafe {
            std::env::set_var("AUTORESEARCH_PR_TOKEN", "pat-chain-token");
        }
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
        }
        unsafe {
            std::env::remove_var("GH_TOKEN");
        }

        // No App threaded → the PAT chain answers.
        let mut cfg = crate::testing::cfg_from_args(["ctl"]);
        assert_eq!(
            resolve_pack_pr_token(&cfg).await?.as_deref(),
            Some("pat-chain-token")
        );

        // App threaded → the installation token wins even with the PAT still set.
        let server = MockServer::start().await;
        let expires = jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() + 3600)?;
        Mock::given(method("POST"))
            .and(path("/app/installations/7/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_app_token",
                "expires_at": expires.to_string(),
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir()?;
        let key = crate::secrets::github_app::testkey::write_test_key(dir.path());
        cfg.github_app = Some(crate::secrets::github_app::GithubAppTokenSource::new(
            "4210340",
            "7",
            key,
            server.uri(),
        ));
        assert_eq!(
            resolve_pack_pr_token(&cfg).await?.as_deref(),
            Some("ghs_app_token")
        );

        for (v, p) in vars.iter().zip(prior) {
            match p {
                Some(val) => unsafe { std::env::set_var(v, val) },
                None => unsafe { std::env::remove_var(v) },
            }
        }
        Ok(())
    }

    fn run_git(args: &[&str]) -> Result<()> {
        let status = Command::new("git").args(args).status()?;
        anyhow::ensure!(status.success(), "git {args:?} failed");
        Ok(())
    }

    /// The Openshell arm end-to-end at the engine boundary: `rank_grounded` shells `CRUCIBLE_BIN`'s
    /// `rank-grounded` (here a script) against a maintained checkout and parses the verdict off its
    /// stdout — the shape the reconcile escalation drives.
    #[test]
    fn rank_grounded_parses_a_scripted_verdict() {
        let _g = crate::ENV_LOCK.blocking_lock();
        let root = std::env::temp_dir().join(format!("crucible-rankg-eng-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // A maintained checkout (a git repo) at the standard per-repo path; ensure_checkout on an
        // existing checkout is a best-effort offline refresh (no origin) rather than a clone.
        let state = root.join("state");
        let dir = checkout_dir(&state, "owner/repo");
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&["-C", &dir.to_string_lossy(), "init", "-q"]).unwrap();
        ensure_checkout("unused-url", &dir).expect("existing checkout refresh is best-effort");

        let bin = root.join("crucible-fake");
        let json = r#"{"tier":"T2","rationale":"needs one live service","confidence":"low","cost_usd":0.2,"over_budget":false}"#;
        crate::testing::write_exec(
            &bin,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = rank-grounded ]; then\n printf '%s\\n' '{json}'\n exit 0\nfi\nexit 1\n"
            ),
        );
        unsafe {
            std::env::remove_var("CONTROLLER_SANDBOX_IMAGE");
        }

        let v = rank_grounded(&bin, "owner/repo#1", &dir, 5.0, None).expect("verdict parses");
        assert_eq!(v.disposition, Disposition::Tier(Tier::T2));
        assert_eq!(v.confidence.as_deref(), Some("low"));
        assert!((v.cost_usd - 0.2).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A grounded turn that produced no verdict prints an `{"error":…}` object; the engine surfaces
    /// that as an `Err` so the caller keeps the text-only verdict rather than tiering off nothing.
    #[test]
    fn rank_grounded_surfaces_a_missing_verdict_as_an_error() {
        let root = std::env::temp_dir().join(format!("crucible-rankg-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let bin = root.join("crucible-fake");
        crate::testing::write_exec(
            &bin,
            "#!/bin/sh\nprintf '%s\\n' '{\"error\":\"no verdict\",\"cost_usd\":0.1,\"over_budget\":false}'\n",
        );
        // `Some(agent_cmd)` keeps the call off the CONTROLLER_SANDBOX_IMAGE env read (no lock needed).
        let err = rank_grounded(&bin, "owner/repo#1", &root, 5.0, Some("true")).unwrap_err();
        assert!(
            format!("{err:#}").contains("no verdict"),
            "error should name the missing verdict: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `stale` verdict parses to [`Disposition::Stale`], not a [`Tier`] — the wire vocabulary's
    /// extension beyond `T0|T1|T2|T3|N` ( already-implemented asks close as stale,
    /// never a tier).
    #[test]
    fn rank_grounded_parses_a_stale_disposition() {
        let root =
            std::env::temp_dir().join(format!("crucible-rankg-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let bin = root.join("crucible-fake");
        crate::testing::write_exec(
            &bin,
            "#!/bin/sh\nprintf '%s\\n' '{\"tier\":\"stale\",\"rationale\":\"already fixed in src/foo.rs:42\",\"confidence\":\"high\",\"cost_usd\":0.1,\"over_budget\":false}'\n",
        );
        let v = rank_grounded(&bin, "owner/repo#1", &root, 5.0, Some("true")).expect("parses");
        assert_eq!(v.disposition, Disposition::Stale);
        assert!(v.rationale.contains("src/foo.rs:42"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
