//! The draft-PR approval backend (headless default).
//!
//! A judge-changing `request_trace` opens a **draft PR** on the (fork) repo describing the
//! capture/re-scope ask; a maintainer approves with a `/approve-capture` slash-command comment
//! (or by merging). The agent never holds the forge token; this runs server-side. It shells
//! `gh api`, which authenticates from the keyring locally and from `GH_TOKEN` (the mounted
//! repo-scoped PAT) in the pod, so the same code path serves both.
//!
//! The marker branch is an **empty commit** (same tree as base, new message) so the PR is valid
//! without touching any files; the request lives in the PR title/body, and the
//! `agentic/<...>` branch is the durable state record on the fork.

use crate::approval::{ApprovalBackend, ApprovalRequest, ApprovalState};
use anyhow::{Context, Result};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
#[error("gh {subcommand:?} failed: {stderr}")]
struct GhFailed {
    subcommand: String,
    stderr: String,
}

/// Approval via a draft PR on `repo` (e.g. `wseaton/llm-d-router`), branched off `base`.
pub struct DraftPrApproval {
    pub(crate) repo: String,
    pub(crate) base: String,
}

impl DraftPrApproval {
    /// `agentic/capture-<sanitized-trace-id>`: self-labelling, deterministic (so a re-request
    /// reuses the branch/PR rather than spawning duplicates).
    fn branch_for(&self, req: &ApprovalRequest) -> String {
        let safe: String = req
            .trace_id
            .0
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        format!("agentic/capture-{safe}")
    }
}

impl ApprovalBackend for DraftPrApproval {
    fn open(&self, req: &ApprovalRequest) -> Result<String> {
        let branch = self.branch_for(req);

        // An existing PR for this head means the ask is already open; return it (idempotent).
        if let Some(url) = find_pr(&self.repo, &branch)? {
            return Ok(url);
        }

        // 1. base commit + its tree.
        let base_sha = gh_json(&[
            "api",
            &format!("repos/{}/git/ref/heads/{}", self.repo, self.base),
            "--jq",
            ".object.sha",
        ])?;
        let base_sha = base_sha.trim().trim_matches('"').to_string();
        let tree_sha = gh_json(&[
            "api",
            &format!("repos/{}/git/commits/{base_sha}", self.repo),
            "--jq",
            ".tree.sha",
        ])?;
        let tree_sha = tree_sha.trim().trim_matches('"').to_string();

        // 2. empty marker commit (same tree, new message) so the branch differs from base.
        let commit_sha = gh_json(&[
            "api",
            "-X",
            "POST",
            &format!("repos/{}/git/commits", self.repo),
            "-f",
            &format!("message=agentic: capture request {}", req.trace_id.0),
            "-f",
            &format!("tree={tree_sha}"),
            "-f",
            &format!("parents[]={base_sha}"),
            "--jq",
            ".sha",
        ])?;
        let commit_sha = commit_sha.trim().trim_matches('"').to_string();

        // 3. the agentic/<...> branch (ignore "already exists").
        let _ = gh_raw(&[
            "api",
            "-X",
            "POST",
            &format!("repos/{}/git/refs", self.repo),
            "-f",
            &format!("ref=refs/heads/{branch}"),
            "-f",
            &format!("sha={commit_sha}"),
        ]);

        // 4. the draft PR (head + base both in `repo` => an internal PR, never upstream).
        let body = format!(
            "**Automated capture/re-scope request from the autoresearch loop.**\n\n\
             {summary}\n\n- trace_id: `{tid}`\n- estimated GPUs: {gpus}\n\n\
             Approve by commenting `/approve-capture {tid}` or marking this PR ready. \
             The loop is holding for this signal; it never provisions GPU directly.",
            summary = req.summary,
            tid = req.trace_id.0,
            gpus = req.est_gpus,
        );
        let url = gh_json(&[
            "api",
            "-X",
            "POST",
            &format!("repos/{}/pulls", self.repo),
            "-f",
            &format!("title=agentic: capture request ({})", req.trace_id.0),
            "-f",
            &format!("head={branch}"),
            "-f",
            &format!("base={}", self.base),
            "-f",
            &format!("body={body}"),
            "-F",
            "draft=true",
            "--jq",
            ".html_url",
        ])
        .context("opening the draft PR")?;
        Ok(url.trim().trim_matches('"').to_string())
    }

    fn draft_pr_repo(&self) -> Option<&str> {
        Some(&self.repo)
    }

    fn poll(&self, handle: &str) -> Result<ApprovalState> {
        let number = pr_number(handle).context("parsing PR number from handle")?;

        // A merged/closed PR is a terminal human decision.
        let state = gh_json(&[
            "api",
            &format!("repos/{}/pulls/{number}", self.repo),
            "--jq",
            ".merged_at // .state",
        ])?;
        let state = state.trim().trim_matches('"');
        if state != "open" && !state.is_empty() && state != "null" {
            // merged_at is a timestamp (approved-by-merge); "closed" without merge is a deny.
            return Ok(if state == "closed" {
                ApprovalState::Denied
            } else {
                ApprovalState::Approved
            });
        }

        // Otherwise look for the slash-command in the comments.
        let comments = gh_json(&[
            "api",
            &format!("repos/{}/issues/{number}/comments", self.repo),
            "--jq",
            ".[].body",
        ])?;
        for line in comments.lines() {
            let c = line.trim();
            if c.contains("/approve-capture") {
                return Ok(ApprovalState::Approved);
            }
            if c.contains("/deny") {
                return Ok(ApprovalState::Denied);
            }
        }
        Ok(ApprovalState::Pending)
    }
}

/// The PR number from an `html_url` like `https://github.com/o/r/pull/42`.
fn pr_number(url: &str) -> Option<u64> {
    url.rsplit('/').next().and_then(|s| s.parse().ok())
}

/// The existing PR's `html_url` for `head` branch, if one is already open.
fn find_pr(repo: &str, branch: &str) -> Result<Option<String>> {
    let out = gh_json(&[
        "api",
        &format!("repos/{repo}/pulls"),
        "--jq",
        &format!(".[] | select(.head.ref==\"{branch}\") | .html_url"),
    ])?;
    let url = out.trim().trim_matches('"');
    Ok((!url.is_empty()).then(|| url.to_string()))
}

/// Run `gh` and return stdout on success, else an error carrying stderr.
fn gh_json(args: &[&str]) -> Result<String> {
    let out = gh_raw(args)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn gh_raw(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .context("exec `gh` (is it installed + authed / GH_TOKEN set?)")?;
    if !out.status.success() {
        return Err(GhFailed {
            subcommand: args.first().unwrap_or(&"").to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        }
        .into());
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TraceId;

    fn req() -> ApprovalRequest {
        ApprovalRequest {
            trace_id: TraceId("model=m;c=48;p=8;mt=8;lt=256;lf=0.2000".into()),
            summary: "re-scope to concurrency=48".into(),
            est_gpus: 1,
        }
    }

    #[test]
    fn branch_is_self_labelling_and_ref_safe() {
        let b = DraftPrApproval {
            repo: "wseaton/llm-d-router".into(),
            base: "main".into(),
        };
        let branch = b.branch_for(&req());
        assert!(branch.starts_with("agentic/capture-"));
        assert!(
            branch
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-')),
            "git-ref-safe: {branch}"
        );
    }

    #[test]
    fn pr_number_parses_from_html_url() {
        assert_eq!(
            pr_number("https://github.com/wseaton/llm-d-router/pull/42"),
            Some(42)
        );
        assert_eq!(pr_number("not-a-url"), None);
    }
}
