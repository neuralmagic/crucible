//! Proposing a tree of files to a project as a merge request, written as one commit through the
//! commits API with the caller's token, and reading that merge request back.
//!
//! ```text
//! GET  repository/tree?path=…&recursive=true&ref={base}  ─▶ files under path
//! POST repository/commits {branch, start_branch: base, force, actions}
//! GET  merge_requests?source_branch=…&state=opened  ─▶ reuse, else POST merge_requests
//! ```

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;

use crate::gitlab::{Gitlab, GitlabError, ProjectPath, Since, etag_of, if_none_match};
use crate::token::BearerToken;

/// A tree of files to write under one path of a project, as one commit on a branch.
pub struct Proposal<'a> {
    pub base: &'a str,
    pub branch: &'a str,
    pub path: &'a str,
    pub files: &'a BTreeMap<String, Vec<u8>>,
    pub message: &'a str,
    pub title: &'a str,
    pub description: &'a str,
}

/// A project as `GET /projects/:id` answers.
#[derive(Clone, Debug, Deserialize)]
pub struct Project {
    pub id: u64,
    #[serde(default)]
    pub default_branch: Option<String>,
}

/// A merge request, as the fields a promotion follows.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct MergeRequest {
    pub iid: u64,
    pub web_url: String,
    pub state: String,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    #[serde(default)]
    pub squash_commit_sha: Option<String>,
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

fn actions(
    path: &str,
    files: &BTreeMap<String, Vec<u8>>,
    existing: &BTreeSet<String>,
) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(files.len());
    let mut kept = BTreeSet::new();
    for (file, bytes) in files {
        let full = under(path, file);
        let action = match existing.contains(&full) {
            true => "update",
            false => "create",
        };
        out.push(json!({
            "action": action,
            "file_path": full,
            "content": base64::engine::general_purpose::STANDARD.encode(bytes),
            "encoding": "base64",
        }));
        kept.insert(full);
    }
    for gone in existing.difference(&kept) {
        out.push(json!({"action": "delete", "file_path": gone}));
    }
    out
}

impl Gitlab {
    pub async fn project(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
    ) -> Result<Project, GitlabError> {
        let doing = format!("reading project {}", project.as_str());
        let url = self.url(project, &[], &[]);
        self.get_json(token, &doing, url).await
    }

    pub async fn propose(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        proposal: &Proposal<'_>,
    ) -> Result<MergeRequest, GitlabError> {
        let dir = proposal.path.trim_matches('/');
        let mut query = vec![("recursive", "true"), ("ref", proposal.base)];
        if !dir.is_empty() {
            query.push(("path", dir));
        }
        let doing = format!("listing {} at {}", project.as_str(), proposal.base);
        let url = self.url(project, &["repository", "tree"], &query);
        let existing: BTreeSet<String> = match self.pages::<TreeEntry>(token, &doing, url).await {
            Ok(entries) => entries
                .into_iter()
                .filter(|e| e.kind == "blob")
                .map(|e| e.path)
                .collect(),
            Err(GitlabError::Status { status: 404, .. }) => BTreeSet::new(),
            Err(e) => return Err(e),
        };

        let doing = format!("committing to {} on {}", project.as_str(), proposal.branch);
        let url = self.url(project, &["repository", "commits"], &[]);
        let body = json!({
            "branch": proposal.branch,
            "start_branch": proposal.base,
            "force": true,
            "commit_message": proposal.message,
            "actions": actions(proposal.path, proposal.files, &existing),
        });
        self.response(token, &doing, self.http.post(url).json(&body))
            .await?;

        let doing = format!("finding the open merge request of {}", proposal.branch);
        let url = self.url(
            project,
            &["merge_requests"],
            &[
                ("state", "opened"),
                ("source_branch", proposal.branch),
                ("target_branch", proposal.base),
            ],
        );
        let open: Vec<MergeRequest> = self.get_json(token, &doing, url).await?;
        if let Some(mr) = open.into_iter().next() {
            return Ok(mr);
        }
        let doing = format!("opening a merge request from {}", proposal.branch);
        let url = self.url(project, &["merge_requests"], &[]);
        let body = json!({
            "source_branch": proposal.branch,
            "target_branch": proposal.base,
            "title": proposal.title,
            "description": proposal.description,
            "remove_source_branch": true,
        });
        let r = self
            .response(token, &doing, self.http.post(url).json(&body))
            .await?;
        r.json()
            .await
            .map_err(|source| GitlabError::Http { doing, source })
    }

    pub async fn merge_request_since(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        iid: u64,
        etag: Option<&str>,
    ) -> Result<Since<MergeRequest>, GitlabError> {
        let doing = format!("reading merge request !{iid} of {}", project.as_str());
        let url = self.url(project, &["merge_requests", &iid.to_string()], &[]);
        let r = self
            .response(token, &doing, if_none_match(self.http.get(url), etag))
            .await?;
        if r.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Since::Unchanged);
        }
        let etag = etag_of(&r);
        let value = r
            .json()
            .await
            .map_err(|source| GitlabError::Http { doing, source })?;
        Ok(Since::Changed { value, etag })
    }
}

#[cfg(test)]
mod tests {
    use crate::gitlab::merge::*;

    #[test]
    fn a_proposal_creates_updates_and_deletes_under_its_path() {
        let files = BTreeMap::from([
            ("crucible.toml".to_owned(), b"a".to_vec()),
            ("workflow.star".to_owned(), b"b".to_vec()),
        ]);
        let existing = BTreeSet::from([
            "packs/triage/crucible.toml".to_owned(),
            "packs/triage/old.star".to_owned(),
        ]);
        let got: Vec<(String, String)> = actions("packs/triage", &files, &existing)
            .iter()
            .map(|a| {
                (
                    a["action"].as_str().unwrap().to_owned(),
                    a["file_path"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("update".to_owned(), "packs/triage/crucible.toml".to_owned()),
                ("create".to_owned(), "packs/triage/workflow.star".to_owned()),
                ("delete".to_owned(), "packs/triage/old.star".to_owned()),
            ]
        );
        assert_eq!(under("", "x"), "x");
    }

    #[test]
    fn a_merge_request_reads_its_merge() {
        let mr: MergeRequest = serde_json::from_value(serde_json::json!({
            "iid": 3,
            "web_url": "https://gitlab.example.com/g/p/-/merge_requests/3",
            "state": "merged",
            "merge_commit_sha": "abc",
            "squash_commit_sha": null,
        }))
        .unwrap();
        assert_eq!(mr.state, "merged");
        assert_eq!(mr.merge_commit_sha.as_deref(), Some("abc"));
    }
}
