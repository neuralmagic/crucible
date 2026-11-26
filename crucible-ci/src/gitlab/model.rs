//! The fields of GitLab's REST API (v4) this crate reads.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Pipeline {
    pub id: u64,
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
    /// The jobs this one needs, whether GitLab lists them as names or as objects.
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
