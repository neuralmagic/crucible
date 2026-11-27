//! The fields of GitLab's REST API (v4) this crate reads.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The account a token belongs to, as `GET /user` answers.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct User {
    pub id: u64,
    pub username: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Commit {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Member {
    pub access_level: u32,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub membership_state: Option<String>,
}

impl Member {
    pub fn is_active(&self) -> bool {
        [&self.state, &self.membership_state]
            .into_iter()
            .all(|s| s.as_deref().is_none_or(|s| s == "active"))
    }
}

/// Who may push, merge or create on a protected ref: an access level, one user, a group, or a
/// deploy key.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct RefAccess {
    #[serde(default)]
    pub access_level: Option<u32>,
    #[serde(default)]
    pub user_id: Option<u64>,
    #[serde(default)]
    pub group_id: Option<u64>,
    #[serde(default)]
    pub deploy_key_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProtectedBranch {
    pub name: String,
    #[serde(default)]
    pub push_access_levels: Vec<RefAccess>,
    #[serde(default)]
    pub merge_access_levels: Vec<RefAccess>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProtectedTag {
    pub name: String,
    #[serde(default)]
    pub create_access_levels: Vec<RefAccess>,
}

/// `GET /oauth/token/info`: the OAuth application a token was issued to.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TokenInfo {
    #[serde(default)]
    pub application: Option<TokenApplication>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TokenApplication {
    pub uid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Pipeline {
    pub id: u64,
    #[serde(default)]
    pub iid: Option<u64>,
    pub sha: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub status: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub stage: String,
    pub status: String,
    #[serde(default)]
    pub allow_failure: bool,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
}

/// A trigger job, which starts a downstream pipeline.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Bridge {
    pub id: u64,
    pub name: String,
    pub stage: String,
    pub status: String,
    #[serde(default)]
    pub allow_failure: bool,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub downstream_pipeline: Option<Downstream>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Downstream {
    pub id: u64,
    #[serde(default)]
    pub project_id: Option<u64>,
    #[serde(default)]
    pub web_url: Option<String>,
}

/// The CI lint answer for a ref, without creating a pipeline.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Lint {
    pub valid: bool,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub jobs: Vec<LintJob>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct LintJob {
    pub name: String,
    pub stage: String,
    #[serde(default)]
    pub needs: Vec<Value>,
}

impl LintJob {
    pub fn needed(&self) -> Vec<String> {
        self.needs
            .iter()
            .filter_map(|n| match n {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("name").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect()
    }
}
