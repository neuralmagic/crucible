//! Engine-authored, broker-consumed workflow report. No task-controlled text crosses this wire.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::session::{TaskBlocked, TransportCause};

pub const REPORT_FILE: &str = "report.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskReport {
    pub name: String,
    pub status: String,
    pub cost_usd: f64,
    /// Present exactly when `status` is `blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<TaskBlocked>,
    /// Present exactly when `status` is `transport`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportCause>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunReport {
    pub run: String,
    pub run_url: Option<String>,
    pub tasks: Vec<TaskReport>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub results: BTreeMap<String, ReportResult>,
    #[serde(default)]
    pub verdict: RunVerdict,
}

/// The engine's run verdict: `pass` when every required main-graph task held and dispatch
/// ended on its own, `fail` otherwise, `pending` until the main graph settles.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunVerdict {
    #[default]
    Pending,
    Pass,
    Fail,
}

impl RunVerdict {
    pub fn of(valid: bool) -> Self {
        if valid { Self::Pass } else { Self::Fail }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Pass => "pass",
            Self::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReportResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
}
