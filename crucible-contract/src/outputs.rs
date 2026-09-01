//! The declared-output-kind vocabulary and the resolved bounds both sides read.
//!
//! The engine resolves a frozen manifest's `[outputs]` into [`ResolvedOutputs`] and projects it
//! into the broker's environment as `BROKER_OUTPUTS`; the broker deserializes the same type and
//! enforces it at the mediation point.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Env var carrying the run's [`ResolvedOutputs`] as JSON.
pub const ENV_OUTPUTS: &str = "BROKER_OUTPUTS";
/// Env var carrying the run's bound workflow parameter values as a JSON object, for resolving an
/// open target's `param` binding.
pub const ENV_OUTPUT_PARAMS: &str = "BROKER_OUTPUT_PARAMS";
/// Env var naming the session log a refusal row is appended to.
pub const ENV_SESSION_LOG: &str = "BROKER_SESSION_LOG";

/// The closed, engine-defined vocabulary of mediated writes. A kind name, once retired, must not
/// be reused with a different meaning.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash, Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum OutputKind {
    /// A draft pull request opened on a forge.
    #[default]
    DraftPr,
    /// A comment posted on an issue tracker item.
    TrackerComment,
    /// A message posted to a chat destination.
    ChatMessage,
    /// A container image pushed to a registry.
    ImagePush,
    /// A rollout of a built candidate onto a live deployment.
    Deploy,
    /// A `workflow_dispatch` fired at a forge-hosted build workflow.
    WorkflowDispatch,
    /// A GPU job submitted to capture a measurement, evaluation, or trace.
    GpuCapture,
}

impl OutputKind {
    /// Every kind in the vocabulary, in declaration order.
    pub const ALL: &'static [OutputKind] = &[
        OutputKind::DraftPr,
        OutputKind::TrackerComment,
        OutputKind::ChatMessage,
        OutputKind::ImagePush,
        OutputKind::Deploy,
        OutputKind::WorkflowDispatch,
        OutputKind::GpuCapture,
    ];

    /// The stable wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            OutputKind::DraftPr => "draft-pr",
            OutputKind::TrackerComment => "tracker-comment",
            OutputKind::ChatMessage => "chat-message",
            OutputKind::ImagePush => "image-push",
            OutputKind::Deploy => "deploy",
            OutputKind::WorkflowDispatch => "workflow-dispatch",
            OutputKind::GpuCapture => "gpu-capture",
        }
    }

    /// Whether the kind addresses a target (a repository, a tracker item, a chat destination, an
    /// image registry, a deployment). A capture addresses nothing outside the run.
    pub fn addresses_target(self) -> bool {
        !matches!(self, OutputKind::GpuCapture)
    }

    /// Parse a wire token.
    pub fn parse(s: &str) -> Option<Self> {
        OutputKind::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

impl std::fmt::Display for OutputKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A resolved target: either a single address the declaration fixed, or an open scope the agent
/// may address within. An engine default never carries `Open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResolvedTarget {
    Fixed { fixed: String },
    Open { open: OpenScope },
}

/// An open target's reach: a scope narrower than the kind's whole address space, optionally bound
/// to a named workflow parameter whose run value the target must equal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenScope {
    pub scope: String,
    /// The workflow parameter the scope binds to, or `None` for an unbound scope.
    #[serde(default)]
    pub param: Option<String>,
}

/// One kind's resolved bounds for a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedOutput {
    pub kind: OutputKind,
    /// Writes of this kind allowed per run.
    pub count: u32,
    /// `null` for a kind that addresses nothing, or for an addressing kind whose target the engine
    /// could not resolve — the latter refuses every write of that kind. Always present on the
    /// wire, so a reader never has to distinguish "absent" from "no target".
    #[serde(default)]
    pub target: Option<ResolvedTarget>,
    /// Where the bound came from: a manifest declaration or the engine default table.
    pub source: BoundSource,
}

/// Whether a bound was declared by the pack or supplied by the engine default table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BoundSource {
    Manifest,
    EngineDefault,
}

/// Every vocabulary kind's resolved bounds, plus the wire version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedOutputs {
    pub version: u8,
    pub outputs: Vec<ResolvedOutput>,
}

/// The `version` every [`ResolvedOutputs`] carries.
pub const OUTPUTS_WIRE_VERSION: u8 = 1;

impl ResolvedOutputs {
    pub fn get(&self, kind: OutputKind) -> Option<&ResolvedOutput> {
        self.outputs.iter().find(|o| o.kind == kind)
    }
}

/// The engine default count for a kind the pack leaves undeclared. Always bounded.
pub fn default_count(kind: OutputKind) -> u32 {
    match kind {
        OutputKind::DraftPr => 2,
        OutputKind::TrackerComment => 2,
        OutputKind::ChatMessage => 8,
        OutputKind::ImagePush => 100,
        OutputKind::Deploy => 100,
        OutputKind::WorkflowDispatch => 20,
        OutputKind::GpuCapture => 100,
    }
}

/// The default target for each addressing kind, resolved from the run's own environment plus what
/// the caller already knows. A `None` field means no default target resolved, which refuses every
/// write of that kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DefaultTargets {
    pub draft_pr: Option<String>,
    pub tracker_item: Option<String>,
    pub chat: Option<String>,
    pub registry: Option<String>,
    pub deployment: Option<String>,
    pub dispatch_repo: Option<String>,
}

/// The `chat-message` default target: the engine's own operator channel.
pub const OPERATOR_CHANNEL: &str = "operator-channel";

/// A non-empty, trimmed environment value.
fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl DefaultTargets {
    /// The targets readable from the process environment alone. `draft_pr` and `dispatch_repo`
    /// need manifest knowledge, so they stay unset here and the engine fills them in.
    pub fn from_env() -> Self {
        let deployment = match (
            env_value("FORGE_DEPLOY_NAMESPACE"),
            env_value("FORGE_DEPLOY_NAME"),
        ) {
            (Some(ns), Some(name)) => Some(format!("{ns}/{name}")),
            (None, Some(name)) => Some(name),
            _ => None,
        };
        Self {
            draft_pr: env_value("AUTORESEARCH_PR_REPO"),
            tracker_item: env_value("CRUCIBLE_ITEM"),
            chat: Some(OPERATOR_CHANNEL.to_string()),
            registry: env_value("FORGE_REGISTRY"),
            deployment,
            dispatch_repo: None,
        }
    }

    /// The default target for one kind.
    pub fn for_kind(&self, kind: OutputKind) -> Option<String> {
        match kind {
            OutputKind::DraftPr => self.draft_pr.clone(),
            OutputKind::TrackerComment => self.tracker_item.clone(),
            OutputKind::ChatMessage => self.chat.clone(),
            OutputKind::ImagePush => self.registry.clone(),
            OutputKind::Deploy => self.deployment.clone(),
            OutputKind::WorkflowDispatch => self.dispatch_repo.clone(),
            OutputKind::GpuCapture => None,
        }
    }

    /// One resolved bound per vocabulary kind, entirely from the default table.
    pub fn engine_defaults(&self) -> ResolvedOutputs {
        ResolvedOutputs {
            version: OUTPUTS_WIRE_VERSION,
            outputs: OutputKind::ALL
                .iter()
                .map(|kind| ResolvedOutput {
                    kind: *kind,
                    count: default_count(*kind),
                    target: self
                        .for_kind(*kind)
                        .map(|fixed| ResolvedTarget::Fixed { fixed }),
                    source: BoundSource::EngineDefault,
                })
                .collect(),
        }
    }
}

/// Why a mediated write was refused, naming the violated bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundViolation {
    /// The per-run count for this kind is spent.
    CountExhausted { kind: OutputKind, count: u32 },
    /// The kind addresses a target but none resolved, so nothing may be written.
    NoTarget { kind: OutputKind },
    /// The requested target lies outside the resolved declaration.
    TargetOutOfScope {
        kind: OutputKind,
        requested: String,
        scope: String,
    },
    /// The declaration binds a parameter the run did not supply.
    UnboundParam { kind: OutputKind, param: String },
}

impl BoundViolation {
    /// The bound this violation names, as it appears in the refusal and the session row.
    pub fn bound(&self) -> String {
        match self {
            BoundViolation::CountExhausted { kind, count } => {
                format!("[outputs.{kind}].count = {count}")
            }
            BoundViolation::NoTarget { kind } => format!("[outputs.{kind}].target"),
            BoundViolation::TargetOutOfScope { kind, scope, .. } => {
                format!("[outputs.{kind}].target = {scope}")
            }
            BoundViolation::UnboundParam { kind, param } => {
                format!("[outputs.{kind}].target.open.param = {param}")
            }
        }
    }

    pub fn kind(&self) -> OutputKind {
        match self {
            BoundViolation::CountExhausted { kind, .. }
            | BoundViolation::NoTarget { kind }
            | BoundViolation::TargetOutOfScope { kind, .. }
            | BoundViolation::UnboundParam { kind, .. } => *kind,
        }
    }
}

impl std::fmt::Display for BoundViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoundViolation::CountExhausted { kind, count } => write!(
                f,
                "refused: this run's {kind} budget is spent ({count} allowed by [outputs.{kind}].count)"
            ),
            BoundViolation::NoTarget { kind } => write!(
                f,
                "refused: {kind} addresses a target and this run resolved none \
                 (declare [outputs.{kind}].target)"
            ),
            BoundViolation::TargetOutOfScope {
                kind,
                requested,
                scope,
            } => write!(
                f,
                "refused: {kind} target {requested:?} is outside [outputs.{kind}].target = {scope:?}"
            ),
            BoundViolation::UnboundParam { kind, param } => write!(
                f,
                "refused: [outputs.{kind}].target binds parameter {param:?}, which this run did \
                 not supply"
            ),
        }
    }
}

impl std::error::Error for BoundViolation {}

/// Whether `scope` admits `value`.
///
/// A scope containing `*` is a glob (`*` matches any run of characters). A literal scope admits an
/// exact match, or a value extending it at an address boundary (`/`, `:`, `@`, `#`), which is what
/// makes a registry or repository prefix bound every reference pushed under it while
/// `PROJ-123` does not admit `PROJ-1234`.
pub fn scope_admits(scope: &str, value: &str) -> bool {
    if scope.is_empty() {
        return false;
    }
    if scope.contains('*') {
        return glob_match(scope, value);
    }
    if value == scope {
        return true;
    }
    value
        .strip_prefix(scope)
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| matches!(c, '/' | ':' | '@' | '#'))
}

/// A scope that admits any target at all: nothing to constrain, or only wildcards and separators.
pub fn scope_is_unbounded(scope: &str) -> bool {
    let trimmed = scope.trim();
    trimmed.is_empty()
        || !trimmed
            .chars()
            .any(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `*`-only glob, anchored at both ends.
fn glob_match(pattern: &str, value: &str) -> bool {
    let mut segments = pattern.split('*');
    let Some(first) = segments.next() else {
        return false;
    };
    let Some(mut rest) = value.strip_prefix(first) else {
        return false;
    };
    let tail: Vec<&str> = segments.collect();
    let Some((last, middle)) = tail.split_last() else {
        return rest.is_empty();
    };
    for seg in middle {
        if seg.is_empty() {
            continue;
        }
        match rest.find(seg) {
            Some(at) => rest = &rest[at + seg.len()..],
            None => return false,
        }
    }
    last.is_empty() || rest.ends_with(last)
}

/// Resolve the target a call may address: `requested` when the declaration admits it, the fixed
/// target when the call omitted one.
///
/// `params` supplies run values for an open target's parameter binding.
pub fn resolve_target(
    bound: &ResolvedOutput,
    requested: Option<&str>,
    params: &BTreeMap<String, String>,
) -> Result<Option<String>, BoundViolation> {
    let kind = bound.kind;
    let Some(target) = &bound.target else {
        return if kind.addresses_target() {
            Err(BoundViolation::NoTarget { kind })
        } else {
            Ok(None)
        };
    };
    match target {
        ResolvedTarget::Fixed { fixed } => match requested {
            None => Ok(Some(fixed.clone())),
            Some(req) if scope_admits(fixed, req) => Ok(Some(req.to_string())),
            Some(req) => Err(BoundViolation::TargetOutOfScope {
                kind,
                requested: req.to_string(),
                scope: fixed.clone(),
            }),
        },
        ResolvedTarget::Open { open } => {
            let bound_value =
                match &open.param {
                    Some(name) => Some(params.get(name).cloned().ok_or_else(|| {
                        BoundViolation::UnboundParam {
                            kind,
                            param: name.clone(),
                        }
                    })?),
                    None => None,
                };
            match (requested, bound_value) {
                (None, Some(v)) => Ok(Some(v)),
                (Some(req), Some(v)) if req == v => Ok(Some(v)),
                (Some(req), Some(v)) => Err(BoundViolation::TargetOutOfScope {
                    kind,
                    requested: req.to_string(),
                    scope: v,
                }),
                (None, None) => Err(BoundViolation::NoTarget { kind }),
                (Some(req), None) if scope_admits(&open.scope, req) => Ok(Some(req.to_string())),
                (Some(req), None) => Err(BoundViolation::TargetOutOfScope {
                    kind,
                    requested: req.to_string(),
                    scope: open.scope.clone(),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_tokens_round_trip_through_serde_and_parse() {
        for kind in OutputKind::ALL {
            let json = serde_json::to_string(kind).expect("serialize");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(OutputKind::parse(kind.as_str()), Some(*kind));
            let back: OutputKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, *kind);
        }
        assert_eq!(OutputKind::parse("teleport"), None);
        assert!(serde_json::from_str::<OutputKind>("\"teleport\"").is_err());
    }

    #[test]
    fn only_gpu_capture_addresses_nothing() {
        for kind in OutputKind::ALL {
            assert_eq!(
                kind.addresses_target(),
                *kind != OutputKind::GpuCapture,
                "{kind}"
            );
        }
    }

    #[test]
    fn scope_admits_at_address_boundaries_only() {
        assert!(scope_admits("PROJ-123", "PROJ-123"));
        assert!(!scope_admits("PROJ-123", "PROJ-1234"));
        assert!(scope_admits("quay.io/aipcc", "quay.io/aipcc/epp:abc"));
        assert!(scope_admits(
            "quay.io/aipcc/epp",
            "quay.io/aipcc/epp@sha256:deadbeef"
        ));
        assert!(!scope_admits("quay.io/aipcc", "quay.io/aipcc-evil/epp:abc"));
        assert!(!scope_admits("", "anything"));
    }

    #[test]
    fn scope_admits_globs() {
        assert!(scope_admits("PROJ-*", "PROJ-1234"));
        assert!(!scope_admits("PROJ-*", "OTHER-1"));
        assert!(scope_admits("*.example.com", "api.example.com"));
        assert!(!scope_admits("*.example.com", "example.org"));
        assert!(scope_admits("a*b*c", "axxbyyc"));
        assert!(!scope_admits("a*b*c", "axxbyy"));
    }

    #[test]
    fn unbounded_scopes_are_recognized() {
        for bad in ["", "  ", "*", "**", "*/*", "*:*"] {
            assert!(scope_is_unbounded(bad), "{bad:?} admits anything");
        }
        for good in ["PROJ-*", "quay.io/aipcc", "*.example.com"] {
            assert!(!scope_is_unbounded(good), "{good:?} is narrow enough");
        }
    }

    fn fixed(kind: OutputKind, target: &str) -> ResolvedOutput {
        ResolvedOutput {
            kind,
            count: 1,
            target: Some(ResolvedTarget::Fixed {
                fixed: target.into(),
            }),
            source: BoundSource::Manifest,
        }
    }

    #[test]
    fn fixed_target_supplies_an_omitted_value_and_bounds_a_supplied_one() {
        let bound = fixed(OutputKind::ImagePush, "quay.io/aipcc");
        let none = BTreeMap::new();
        assert_eq!(
            resolve_target(&bound, None, &none).unwrap(),
            Some("quay.io/aipcc".to_string())
        );
        assert_eq!(
            resolve_target(&bound, Some("quay.io/aipcc/epp:1"), &none).unwrap(),
            Some("quay.io/aipcc/epp:1".to_string())
        );
        let err = resolve_target(&bound, Some("docker.io/evil:1"), &none).unwrap_err();
        assert!(matches!(err, BoundViolation::TargetOutOfScope { .. }));
        assert!(err.bound().contains("quay.io/aipcc"));
    }

    #[test]
    fn open_target_bound_to_a_param_pins_the_run_value() {
        let bound = ResolvedOutput {
            kind: OutputKind::TrackerComment,
            count: 3,
            target: Some(ResolvedTarget::Open {
                open: OpenScope {
                    scope: "PROJ-*".into(),
                    param: Some("issue_key".into()),
                },
            }),
            source: BoundSource::Manifest,
        };
        let params = BTreeMap::from([("issue_key".to_string(), "PROJ-42".to_string())]);
        assert_eq!(
            resolve_target(&bound, None, &params).unwrap(),
            Some("PROJ-42".to_string()),
            "an omitted target defaults to the parameterizing item"
        );
        assert_eq!(
            resolve_target(&bound, Some("PROJ-42"), &params).unwrap(),
            Some("PROJ-42".to_string())
        );
        let err = resolve_target(&bound, Some("PROJ-43"), &params).unwrap_err();
        assert!(
            matches!(err, BoundViolation::TargetOutOfScope { .. }),
            "a sibling item in the same scope is still not this run's item"
        );
        let err = resolve_target(&bound, Some("PROJ-43"), &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, BoundViolation::UnboundParam { .. }));
    }

    #[test]
    fn unbound_open_scope_admits_within_the_scope_only() {
        let bound = ResolvedOutput {
            kind: OutputKind::TrackerComment,
            count: 3,
            target: Some(ResolvedTarget::Open {
                open: OpenScope {
                    scope: "PROJ-*".into(),
                    param: None,
                },
            }),
            source: BoundSource::Manifest,
        };
        let none = BTreeMap::new();
        assert_eq!(
            resolve_target(&bound, Some("PROJ-9"), &none).unwrap(),
            Some("PROJ-9".to_string())
        );
        assert!(resolve_target(&bound, Some("OTHER-9"), &none).is_err());
        assert!(
            resolve_target(&bound, None, &none).is_err(),
            "an unbound open scope cannot invent the target"
        );
    }

    #[test]
    fn a_targetless_addressing_kind_refuses_every_write() {
        let bound = ResolvedOutput {
            kind: OutputKind::DraftPr,
            count: 1,
            target: None,
            source: BoundSource::EngineDefault,
        };
        let err = resolve_target(&bound, None, &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, BoundViolation::NoTarget { .. }));
        let capture = ResolvedOutput {
            kind: OutputKind::GpuCapture,
            count: 4,
            target: None,
            source: BoundSource::EngineDefault,
        };
        assert_eq!(
            resolve_target(&capture, None, &BTreeMap::new()).unwrap(),
            None
        );
    }

    #[test]
    fn resolved_outputs_round_trip_as_the_wire_shape() {
        let doc = ResolvedOutputs {
            version: OUTPUTS_WIRE_VERSION,
            outputs: vec![
                fixed(OutputKind::DraftPr, "owner/repo"),
                ResolvedOutput {
                    kind: OutputKind::TrackerComment,
                    count: 2,
                    target: Some(ResolvedTarget::Open {
                        open: OpenScope {
                            scope: "PROJ-*".into(),
                            param: Some("issue".into()),
                        },
                    }),
                    source: BoundSource::Manifest,
                },
            ],
        };
        let json = serde_json::to_value(&doc).expect("serialize");
        assert_eq!(json["outputs"][0]["kind"], "draft-pr");
        assert_eq!(json["outputs"][0]["target"]["fixed"], "owner/repo");
        assert_eq!(json["outputs"][1]["target"]["open"]["scope"], "PROJ-*");
        assert_eq!(json["outputs"][1]["target"]["open"]["param"], "issue");
        let back: ResolvedOutputs = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, doc);
    }
}
