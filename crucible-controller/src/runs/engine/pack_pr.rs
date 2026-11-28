//! Pushing a pack as a branch pair and opening the draft PR whose diff is exactly that pack, for
//! the autoresearch lane.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackBranches {
    base: String,
    head: String,
}

fn pack_branches(issue_key: &str) -> PackBranches {
    let safe = crate::model::sanitize_key(issue_key);
    PackBranches {
        base: format!("crucible-pack/{safe}-base"),
        head: format!("crucible-pack/{safe}"),
    }
}

pub(crate) fn open_draft_pr(
    repo: &str,
    branch_key: &str,
    pack_out: &Path,
    title: &str,
    body: &str,
    token: Option<&str>,
) -> Result<String> {
    let branches = pack_branches(branch_key);

    if let Some(url) = find_pack_pr(repo, &branches.head, token)? {
        return Ok(url);
    }

    let push_url = pack_push_url(repo, token);
    push_pack_branches(pack_out, &push_url, &branches)
        .with_context(|| format!("pushing pack branches for {branch_key} to {repo}"))?;

    gh_open_draft_pr(repo, title, body, &branches, token)
        .with_context(|| format!("opening the draft PR on {repo}"))
}

fn pack_push_url(repo: &str, token: Option<&str>) -> String {
    match token {
        Some(t) => format!("https://x-access-token:{t}@github.com/{repo}.git"),
        None => format!("https://github.com/{repo}.git"),
    }
}

fn push_pack_branches(pack_out: &Path, push_url: &str, branches: &PackBranches) -> Result<()> {
    let git = |args: &[&str]| crucible_vcs::cli::git(pack_out, args).run();

    git(&["init", "-q"])?;
    git(&["config", "user.email", "autoresearch@crucible.local"])?;
    git(&["config", "user.name", "crucible autoresearch"])?;
    git(&["checkout", "-q", "-B", "crucible-pack-base"])?;
    git(&["commit", "-q", "--allow-empty", "-m", "pristine base"])?;
    let base_sha = crucible_vcs::cli::git(pack_out, ["rev-parse", "HEAD"]).read()?;
    git(&["checkout", "-q", "-B", "crucible-pack-head"])?;
    git(&["add", "-A"])?;
    git(&["commit", "-q", "--allow-empty", "-m", "scope pack"])?;

    push_ref(pack_out, push_url, "HEAD", &branches.head)?;
    push_ref(pack_out, push_url, base_sha.trim(), &branches.base)?;
    Ok(())
}

fn push_ref(workspace: &Path, url: &str, local_ref: &str, branch: &str) -> Result<()> {
    let refspec = format!("{local_ref}:refs/heads/{branch}");
    crucible_vcs::cli::git(workspace, ["push", "--force", url, &refspec])
        .run()
        .with_context(|| format!("git push of {refspec}"))
}

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

#[cfg(test)]
mod tests {
    use std::process::Command;

    use anyhow::Result;

    use crate::runs::engine::pack_pr::*;

    #[test]
    fn pack_branches_are_ref_safe_and_deterministic() {
        let b = pack_branches("owner/repo#7");
        assert_eq!(b.head, "crucible-pack/owner_repo_7");
        assert_eq!(b.base, "crucible-pack/owner_repo_7-base");
        assert!(
            !b.head
                .trim_start_matches("crucible-pack/")
                .contains(['#', ':'])
        );
        assert_eq!(pack_branches("owner/repo#7"), b);
    }

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

    fn run_git(args: &[&str]) -> Result<()> {
        let status = Command::new("git").args(args).status()?;
        anyhow::ensure!(status.success(), "git {args:?} failed");
        Ok(())
    }
}
