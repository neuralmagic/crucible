//! Proposing a tree of files to a repository as a pull request, written through the Git Data API
//! with the caller's token, and reading that pull request back.
//!
//! ```text
//! GET   git/ref/heads/{base}          ─▶ base commit
//! GET   git/commits/{base}            ─▶ base tree;  git/trees/{tree}?recursive=1 ─▶ files under path
//! POST  git/blobs (each file)  ─▶  git/trees {base_tree, add + delete}  ─▶  git/commits {parent base}
//! PATCH git/refs/heads/{branch} {force}  (or POST git/refs)
//! GET   pulls?head=owner:branch&state=open  ─▶ reuse, else POST pulls
//! ```

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;

use crate::github::{
    API, Client, GithubError, JSON_ACCEPT, Repo, Since, Token, encode, etag_of, if_none_match,
};

const BLOB_MODE: &str = "100644";

/// A tree of files to write under one path of a repository, as one commit on a branch.
pub struct Proposal<'a> {
    pub base: &'a str,
    pub branch: &'a str,
    pub path: &'a str,
    pub files: &'a BTreeMap<String, Vec<u8>>,
    pub message: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

/// A repository as `GET /repos/{owner}/{repo}` answers to the caller.
#[derive(Clone, Debug, Deserialize)]
pub struct Repository {
    pub default_branch: String,
    #[serde(default)]
    pub permissions: Option<Permissions>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub push: bool,
}

/// A pull request, as the fields a promotion follows.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub html_url: String,
    pub state: String,
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
}

#[derive(Deserialize)]
struct Sha {
    sha: String,
}

#[derive(Deserialize)]
struct RefObject {
    object: Sha,
}

#[derive(Deserialize)]
struct CommitTree {
    tree: Sha,
}

#[derive(Deserialize)]
struct Tree {
    tree: Vec<TreeEntry>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
}

fn under(path: &str, file: &str) -> String {
    match path.trim_matches('/') {
        "" => file.to_owned(),
        dir => format!("{dir}/{file}"),
    }
}

fn removed<'a>(existing: &'a [TreeEntry], path: &str, kept: &BTreeSet<String>) -> Vec<&'a str> {
    let dir = path.trim_matches('/');
    existing
        .iter()
        .filter(|e| e.kind == "blob")
        .filter(|e| dir.is_empty() || e.path.starts_with(&format!("{dir}/")))
        .filter(|e| !kept.contains(&e.path))
        .map(|e| e.path.as_str())
        .collect()
}

impl Client {
    pub async fn repository(&self, token: &Token, repo: &Repo) -> Result<Repository, GithubError> {
        let doing = "reading the repository";
        let resp = self
            .send(
                self.http.get(format!("{API}/repos/{repo}")),
                token,
                JSON_ACCEPT,
                doing,
            )
            .await?;
        resp.json().await.map_err(GithubError::http(doing))
    }

    pub async fn propose(
        &self,
        token: &Token,
        repo: &Repo,
        proposal: &Proposal<'_>,
    ) -> Result<PullRequest, GithubError> {
        let base: RefObject = self
            .get_json(
                token,
                &format!("{API}/repos/{repo}/git/ref/heads/{}", encode(proposal.base)),
                "reading the base branch",
            )
            .await?;
        let base_sha = base.object.sha;
        let commit: CommitTree = self
            .get_json(
                token,
                &format!("{API}/repos/{repo}/git/commits/{base_sha}"),
                "reading the base commit",
            )
            .await?;
        let tree_sha = commit.tree.sha;
        let existing: Tree = self
            .get_json(
                token,
                &format!("{API}/repos/{repo}/git/trees/{tree_sha}?recursive=1"),
                "listing the base tree",
            )
            .await?;
        if existing.truncated {
            return Err(GithubError::response(
                "listing the base tree",
                "GitHub truncated the tree; the repository is too large to promote into",
            ));
        }

        let mut entries = Vec::with_capacity(proposal.files.len());
        let mut kept = BTreeSet::new();
        for (file, bytes) in proposal.files {
            let path = under(proposal.path, file);
            let blob: Sha = self
                .post_json(
                    token,
                    &format!("{API}/repos/{repo}/git/blobs"),
                    &json!({
                        "content": base64::engine::general_purpose::STANDARD.encode(bytes),
                        "encoding": "base64",
                    }),
                    "writing a file",
                )
                .await?;
            entries.push(json!({"path": path, "mode": BLOB_MODE, "type": "blob", "sha": blob.sha}));
            kept.insert(path);
        }
        for gone in removed(&existing.tree, proposal.path, &kept) {
            entries.push(json!({"path": gone, "mode": BLOB_MODE, "type": "blob", "sha": null}));
        }
        let tree: Sha = self
            .post_json(
                token,
                &format!("{API}/repos/{repo}/git/trees"),
                &json!({"base_tree": tree_sha, "tree": entries}),
                "writing the tree",
            )
            .await?;
        let commit: Sha = self
            .post_json(
                token,
                &format!("{API}/repos/{repo}/git/commits"),
                &json!({"message": proposal.message, "tree": tree.sha, "parents": [base_sha]}),
                "writing the commit",
            )
            .await?;
        self.point_branch(token, repo, proposal.branch, &commit.sha)
            .await?;

        let open: Vec<PullRequest> = self
            .get_json(
                token,
                &format!(
                    "{API}/repos/{repo}/pulls?state=open&head={}:{}",
                    encode(repo.owner()),
                    encode(proposal.branch)
                ),
                "finding the open pull request",
            )
            .await?;
        if let Some(pull) = open.into_iter().next() {
            return Ok(pull);
        }
        self.post_json(
            token,
            &format!("{API}/repos/{repo}/pulls"),
            &json!({
                "title": proposal.title,
                "head": proposal.branch,
                "base": proposal.base,
                "body": proposal.body,
            }),
            "opening the pull request",
        )
        .await
    }

    pub async fn pull_since(
        &self,
        token: &Token,
        repo: &Repo,
        number: u64,
        etag: Option<&str>,
    ) -> Result<Since<PullRequest>, GithubError> {
        let doing = "reading the pull request";
        let resp = self
            .send(
                if_none_match(
                    self.http.get(format!("{API}/repos/{repo}/pulls/{number}")),
                    etag,
                ),
                token,
                JSON_ACCEPT,
                doing,
            )
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Since::Unchanged);
        }
        let etag = etag_of(&resp);
        let value = resp.json().await.map_err(GithubError::http(doing))?;
        Ok(Since::Changed { value, etag })
    }

    async fn point_branch(
        &self,
        token: &Token,
        repo: &Repo,
        branch: &str,
        sha: &str,
    ) -> Result<(), GithubError> {
        let moved = self
            .send(
                self.http
                    .patch(format!(
                        "{API}/repos/{repo}/git/refs/heads/{}",
                        encode(branch)
                    ))
                    .json(&json!({"sha": sha, "force": true})),
                token,
                JSON_ACCEPT,
                "moving the branch",
            )
            .await;
        match moved {
            Ok(_) => Ok(()),
            Err(GithubError::Api {
                status: 404 | 422, ..
            }) => {
                self.send(
                    self.http
                        .post(format!("{API}/repos/{repo}/git/refs"))
                        .json(&json!({"ref": format!("refs/heads/{branch}"), "sha": sha})),
                    token,
                    JSON_ACCEPT,
                    "creating the branch",
                )
                .await?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        token: &Token,
        url: &str,
        doing: &str,
    ) -> Result<T, GithubError> {
        let resp = self
            .send(self.http.get(url), token, JSON_ACCEPT, doing)
            .await?;
        resp.json().await.map_err(GithubError::http(doing))
    }

    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        token: &Token,
        url: &str,
        body: &serde_json::Value,
        doing: &str,
    ) -> Result<T, GithubError> {
        let resp = self
            .send(self.http.post(url).json(body), token, JSON_ACCEPT, doing)
            .await?;
        resp.json().await.map_err(GithubError::http(doing))
    }
}

#[cfg(test)]
mod tests {
    use crate::github::pulls::*;

    fn entry(path: &str, kind: &str) -> TreeEntry {
        TreeEntry {
            path: path.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn files_land_under_the_pack_path() {
        assert_eq!(under("", "crucible.toml"), "crucible.toml");
        assert_eq!(under("packs/triage/", "a/b.star"), "packs/triage/a/b.star");
        assert_eq!(under("/packs", "x"), "packs/x");
    }

    #[test]
    fn only_files_under_the_path_that_the_pack_dropped_are_removed() {
        let existing = [
            entry("README.md", "blob"),
            entry("packs/triage/crucible.toml", "blob"),
            entry("packs/triage/old.star", "blob"),
            entry("packs/triage/prompts", "tree"),
            entry("packs/triage-two/x.star", "blob"),
            entry("packs/other/old.star", "blob"),
        ];
        let kept = BTreeSet::from(["packs/triage/crucible.toml".to_owned()]);
        assert_eq!(
            removed(&existing, "packs/triage", &kept),
            ["packs/triage/old.star"]
        );
        let root_kept = BTreeSet::from(["crucible.toml".to_owned()]);
        assert_eq!(
            removed(
                &[entry("README.md", "blob"), entry("crucible.toml", "blob")],
                "",
                &root_kept
            ),
            ["README.md"]
        );
    }

    #[test]
    fn a_pull_request_reads_its_merge() {
        let pull: PullRequest = serde_json::from_value(serde_json::json!({
            "number": 7,
            "html_url": "https://github.com/acme/packs/pull/7",
            "state": "closed",
            "merged": true,
            "merge_commit_sha": "abc",
            "user": {"login": "wren"},
        }))
        .unwrap();
        assert!(pull.merged);
        assert_eq!(pull.merge_commit_sha.as_deref(), Some("abc"));
    }
}
