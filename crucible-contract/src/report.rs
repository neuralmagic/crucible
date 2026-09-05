//! Engine-authored, broker-consumed workflow report. No task-controlled text crosses this wire.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::session::TaskBlocked;

pub const REPORT_FILE: &str = "report.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskReport {
    pub name: String,
    pub status: String,
    pub cost_usd: f64,
    /// Present exactly when `status` is `blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<TaskBlocked>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunReport {
    pub run: String,
    pub run_url: Option<String>,
    pub tasks: Vec<TaskReport>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub results: BTreeMap<String, ReportResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReportResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
}
