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
use crate::runs::engine::{ITEM_ENV, open_draft_pr, repo_clone_url};
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

/// The repo (`owner/repo`) the approval PRs open against — `CONTROLLER_PACK_REPO`. Unset means the
/// approval isn't configured, so [`open_pack_pr`] returns `Ok(None)` and reconcile leaves the row at
/// `scoped` (the harmless pre-lane-E no-op, retried on the next re-enqueue).
pub(crate) fn pack_pr_repo() -> Option<String> {
    std::env::var("CONTROLLER_PACK_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_contract::Tier;

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
    fn run_git(args: &[&str]) -> Result<()> {
        let status = Command::new("git").args(args).status()?;
        anyhow::ensure!(status.success(), "git {args:?} failed");
        Ok(())
    }
}
