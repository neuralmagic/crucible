//! Approval gates: the wire shapes an `approve(...)` plan task, its resolution, and the
//! artifacts around a parked run share between the engine and the controller.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::fmt;

/// A non-empty approval actor name bounded to the admission-key size.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Approver(String);

/// Why an approval actor name cannot cross the wire.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApproverError {
    #[error("approver {value:?} is empty")]
    Empty { value: String },
    #[error("approver {value:?} is {bytes} bytes, maximum is {max} bytes")]
    TooLong {
        value: String,
        bytes: usize,
        max: usize,
    },
}

impl Approver {
    /// Return the canonical actor name without its validation wrapper.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Approver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for Approver {
    type Error = ApproverError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let value = value.trim();
        if value.is_empty() {
            return Err(ApproverError::Empty {
                value: value.to_string(),
            });
        }
        if value.len() > crate::admission::MAX_KEY_LEN {
            return Err(ApproverError::TooLong {
                value: value.to_string(),
                bytes: value.len(),
                max: crate::admission::MAX_KEY_LEN,
            });
        }
        Ok(Approver(value.to_string()))
    }
}

impl TryFrom<&str> for Approver {
    type Error = ApproverError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_from(value.to_string())
    }
}

impl std::str::FromStr for Approver {
    type Err = ApproverError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(value)
    }
}

impl From<Approver> for String {
    fn from(value: Approver) -> Self {
        value.0
    }
}

impl Serialize for Approver {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Approver {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value).map_err(de::Error::custom)
    }
}

/// Where a gate's resolution comes from, as the engine resolved it when the gate was reached.
/// Rides `ApprovalWait.source` on the session log and the `approval-waits` artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateSource {
    /// A person resolves it through the controller (SPA, MCP) or the control bridge.
    Native,
    /// A GitHub pull request: an authorized review approval, an `/approve` comment, or a merge.
    GithubPr { url: String, until: PrUntil },
    /// A Jira issue reaching a status or carrying a label.
    Jira { key: String, until: JiraUntil },
}

impl GateSource {
    /// The source's wire kind, the token a resolution names in `ApprovalResolved.source`.
    pub fn kind(&self) -> &'static str {
        match self {
            GateSource::Native => "native",
            GateSource::GithubPr { .. } => "github_pr",
            GateSource::Jira { .. } => "jira",
        }
    }

    /// The human-facing handle of the thing being waited on.
    pub fn handle(&self) -> Option<&str> {
        match self {
            GateSource::Native => None,
            GateSource::GithubPr { url, .. } => Some(url),
            GateSource::Jira { key, .. } => Some(key),
        }
    }
}

/// What a pull request must reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrUntil {
    Approved,
    Merged,
}

impl PrUntil {
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "approved" => Some(PrUntil::Approved),
            "merged" => Some(PrUntil::Merged),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PrUntil::Approved => "approved",
            PrUntil::Merged => "merged",
        }
    }
}

/// What a Jira issue must reach: a status by name, or a label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JiraUntil {
    Status(String),
    Label(String),
}

/// How a gate was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    Granted,
    Denied,
}

impl GateDecision {
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "granted" | "approve" | "approved" => Some(GateDecision::Granted),
            "denied" | "deny" => Some(GateDecision::Denied),
            _ => None,
        }
    }

    /// The `ApprovalResolved.outcome` token.
    pub fn as_str(self) -> &'static str {
        match self {
            GateDecision::Granted => "granted",
            GateDecision::Denied => "denied",
        }
    }
}

/// A gate's resolution, from whichever source produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResolution {
    pub decision: GateDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<Approver>,
    /// The source kind that resolved it (`native`, `github_pr`, `jira`, `timeout`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl GateResolution {
    pub fn granted(by: Option<Approver>, source: &str) -> Self {
        GateResolution {
            decision: GateDecision::Granted,
            reason: None,
            by,
            source: Some(source.to_string()),
        }
    }

    pub fn denied(reason: impl Into<String>, by: Option<Approver>, source: &str) -> Self {
        GateResolution {
            decision: GateDecision::Denied,
            reason: Some(reason.into()),
            by,
            source: Some(source.to_string()),
        }
    }

    /// A park that ran out its ceiling.
    pub fn timeout() -> Self {
        GateResolution::denied("park timed out waiting for approval", None, "timeout")
    }

    /// The `ApprovalResolved.reason` text.
    pub fn reason_text(&self) -> String {
        self.reason.clone().unwrap_or_default()
    }
}

/// Why an `--approval` argument could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadApprovalArg(pub String);

impl fmt::Display for BadApprovalArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "--approval {:?} is not <trace_id>=granted|denied[:reason][@by]",
            self.0
        )
    }
}

impl std::error::Error for BadApprovalArg {}

/// Parse one `--approval <trace_id>=granted|denied[:reason][@by]` argument.
pub fn parse_approval_arg(raw: &str) -> Result<(String, GateResolution), BadApprovalArg> {
    let bad = || BadApprovalArg(raw.to_string());
    let (trace, rest) = raw.split_once('=').ok_or_else(bad)?;
    let trace = trace.trim();
    if trace.is_empty() {
        return Err(bad());
    }
    let (rest, by) = match rest.rsplit_once('@') {
        Some((head, by)) if !by.trim().is_empty() => {
            let by = Approver::try_from(by).map_err(|_| bad())?;
            (head, Some(by))
        }
        _ => (rest, None),
    };
    let (decision, reason) = match rest.split_once(':') {
        Some((d, reason)) => (d, Some(reason.trim().to_string()).filter(|r| !r.is_empty())),
        None => (rest, None),
    };
    let decision = GateDecision::parse(decision.trim()).ok_or_else(bad)?;
    Ok((
        trace.to_string(),
        GateResolution {
            decision,
            reason,
            by,
            source: Some("native".to_string()),
        },
    ))
}

/// The trace id an `approve` task waits under: one per task instance per run, and the
/// admission-ledger key a bridge `approve` records against.
pub fn gate_trace_id(run_id: &str, task: &str) -> String {
    format!("approve:{run_id}:{task}")
}

/// Whether a parked run idles in the pod or leaves a snapshot and exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkMode {
    Park,
    Suspend,
}

impl ParkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ParkMode::Park => "park",
            ParkMode::Suspend => "suspend",
        }
    }
}

/// One open gate, as the `approval-waits` artifact lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateWait {
    pub trace_id: String,
    pub handle: String,
    pub task: String,
    pub source: GateSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub mode: ParkMode,
    /// Unix seconds when the gate was reached.
    pub requested_at: f64,
}

/// The `approval-waits` artifact: the gates a parked run is waiting on, replaced whole on every
/// change so the controller's copy is always the current set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalWaits {
    pub v: u8,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_port: Option<u16>,
    pub waits: Vec<GateWait>,
}

impl ApprovalWaits {
    pub const VERSION: u8 = 1;
}

/// One resolution the controller holds for a parked pod, as `GET /api/pods/{pod}/approvals`
/// lists them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodApproval {
    pub trace_id: String,
    pub resolution: GateResolution,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_round_trip_under_their_kind_tag() {
        for (source, kind) in [
            (GateSource::Native, "native"),
            (
                GateSource::GithubPr {
                    url: "https://github.com/o/r/pull/7".into(),
                    until: PrUntil::Merged,
                },
                "github_pr",
            ),
            (
                GateSource::Jira {
                    key: "PROJ-1".into(),
                    until: JiraUntil::Status("Ready".into()),
                },
                "jira",
            ),
        ] {
            let json = serde_json::to_value(&source).expect("encode");
            assert_eq!(json["kind"], kind);
            assert_eq!(source.kind(), kind);
            let back: GateSource = serde_json::from_value(json).expect("decode");
            assert_eq!(back, source);
        }
        let jira = serde_json::to_value(GateSource::Jira {
            key: "PROJ-1".into(),
            until: JiraUntil::Label("approved".into()),
        })
        .expect("encode");
        assert_eq!(jira["until"]["label"], "approved");
    }

    #[test]
    fn approval_args_parse_every_shape_and_refuse_the_rest() {
        let (trace, r) = parse_approval_arg("approve:run-1:gate=granted").expect("bare grant");
        assert_eq!(trace, "approve:run-1:gate");
        assert_eq!(r.decision, GateDecision::Granted);
        assert_eq!(r.reason, None);
        assert_eq!(r.by, None);
        let (_, r) = parse_approval_arg("t=denied:changes requested@alice").expect("full deny");
        assert_eq!(r.decision, GateDecision::Denied);
        assert_eq!(r.reason.as_deref(), Some("changes requested"));
        assert_eq!(r.by.as_ref().map(Approver::as_str), Some("alice"));
        let (_, r) = parse_approval_arg("t=granted@bob").expect("grant with by");
        assert_eq!(r.by.as_ref().map(Approver::as_str), Some("bob"));
        assert_eq!(r.reason, None);
        let (_, r) = parse_approval_arg("t=approve").expect("approve alias");
        assert_eq!(r.decision, GateDecision::Granted);
        for bad in ["", "t", "=granted", "t=maybe", "t=", "t=:why"] {
            assert!(parse_approval_arg(bad).is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn approvers_reject_empty_and_overlong_values_without_truncating() {
        assert!(Approver::try_from("   ").is_err());
        let value = "é".repeat(crate::admission::MAX_KEY_LEN);
        assert!(Approver::try_from(value).is_err());
        let value = "a".repeat(crate::admission::MAX_KEY_LEN);
        assert!(Approver::try_from(value).is_ok());
        let value = "a".repeat(crate::admission::MAX_KEY_LEN + 1);
        assert!(Approver::try_from(value).is_err());
        assert_eq!(Approver::try_from(" alice ").unwrap().as_str(), "alice");
    }

    #[test]
    fn trace_ids_are_one_per_task_per_run() {
        assert_eq!(gate_trace_id("run-1", "review"), "approve:run-1:review");
        assert_eq!(
            gate_trace_id("run-1", "review[item-a]"),
            "approve:run-1:review[item-a]"
        );
    }

    #[test]
    fn the_waits_artifact_round_trips() {
        let waits = ApprovalWaits {
            v: ApprovalWaits::VERSION,
            run_id: "run-1".into(),
            control_port: Some(7777),
            waits: vec![GateWait {
                trace_id: "approve:run-1:gate".into(),
                handle: "gate".into(),
                task: "gate".into(),
                source: GateSource::Native,
                summary: Some("ship it?".into()),
                mode: ParkMode::Park,
                requested_at: 1.5,
            }],
        };
        let json = serde_json::to_string(&waits).expect("encode");
        let back: ApprovalWaits = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, waits);
        assert_eq!(GateResolution::timeout().source.as_deref(), Some("timeout"));
    }
}
