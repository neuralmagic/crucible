//! Engine-authored, broker-consumed workflow report. No task-controlled text crosses this wire.

use serde::{Deserialize, Serialize};

pub const REPORT_FILE: &str = "report.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskReport {
    pub name: String,
    pub status: String,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunReport {
    pub run: String,
    pub run_url: Option<String>,
    pub tasks: Vec<TaskReport>,
}
