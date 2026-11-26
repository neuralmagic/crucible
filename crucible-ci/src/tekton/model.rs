//! The fields of Tekton's `tekton.dev/v1` PipelineRun and TaskRun this crate reads. Anything not
//! named here is ignored, so a newer Tekton that adds fields still parses.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SUCCEEDED: &str = "Succeeded";

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub name: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub creation_timestamp: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct PipelineRun {
    pub metadata: Meta,
    #[serde(default)]
    pub status: Option<PipelineRunStatus>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineRunStatus {
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub completion_time: Option<String>,
    #[serde(default)]
    pub pipeline_spec: Option<PipelineSpec>,
    #[serde(default)]
    pub child_references: Vec<ChildReference>,
    #[serde(default)]
    pub results: Vec<RunResult>,
    #[serde(default)]
    pub skipped_tasks: Vec<SkippedTask>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct PipelineSpec {
    #[serde(default)]
    pub tasks: Vec<PipelineTask>,
    #[serde(default)]
    pub finally: Vec<PipelineTask>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineTask {
    pub name: String,
    #[serde(default)]
    pub run_after: Vec<String>,
    #[serde(default)]
    pub params: Vec<Param>,
    #[serde(default)]
    pub when: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Param {
    pub name: String,
    #[serde(default)]
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildReference {
    #[serde(default)]
    pub kind: Option<String>,
    pub name: String,
    #[serde(default)]
    pub pipeline_task_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct RunResult {
    pub name: String,
    #[serde(default)]
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct SkippedTask {
    pub name: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct TaskRun {
    pub metadata: Meta,
    #[serde(default)]
    pub status: Option<TaskRunStatus>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRunStatus {
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub pod_name: Option<String>,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub completion_time: Option<String>,
    #[serde(default)]
    pub steps: Vec<StepState>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct StepState {
    pub name: String,
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub running: Option<Running>,
    #[serde(default)]
    pub terminated: Option<Terminated>,
    #[serde(default)]
    pub waiting: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Running {
    #[serde(default)]
    pub started_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Terminated {
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
}

/// A list response's items.
#[derive(Clone, Debug, Deserialize)]
pub struct List<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
}

pub fn succeeded(conditions: &[Condition]) -> Option<&Condition> {
    conditions.iter().find(|c| c.kind == SUCCEEDED)
}
