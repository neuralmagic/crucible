//! Engine-side enforcement of the run's declared output bounds (RFC-0001:C-OUTPUTS).
//!
//! The broker mediates the writes its own tools perform. Two kinds are performed by the ENGINE
//! process instead — the draft PRs publish-on-keep opens and the `workflow_dispatch` a
//! `github-actions` build fires — so they are mediated here, against the same
//! [`crucible_contract::outputs::ResolvedOutputs`] the broker is projected.
//!
//! A refusal fails the requesting write naming the violated bound and appends a session row; it
//! never terminates the run.

use crucible_contract::outputs::{BoundViolation, OutputKind, ResolvedOutputs, resolve_target};
use crucible_contract::session::{SessionEvent, encode};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

/// The run's resolved bounds plus the parameter values an open target binds against. Cloned onto
/// [`crate::Args`] so every engine-side mediation point reads the value the broker was handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunBounds {
    resolved: ResolvedOutputs,
    params: BTreeMap<String, String>,
}

impl RunBounds {
    pub fn new(resolved: ResolvedOutputs, params: BTreeMap<String, String>) -> Self {
        Self { resolved, params }
    }

    /// The engine default table computed from this process's environment: the conservative posture
    /// for a path that reached a mediation point with no frozen manifest projected onto it.
    pub fn engine_defaults() -> Self {
        Self::new(
            crucible_contract::outputs::DefaultTargets::from_env().engine_defaults(),
            BTreeMap::new(),
        )
    }

    pub fn resolved(&self) -> &ResolvedOutputs {
        &self.resolved
    }

    pub fn params(&self) -> &BTreeMap<String, String> {
        &self.params
    }
}

/// One run's spend against [`RunBounds`], owned by the run and threaded through the engine's
/// mediation points.
pub struct OutputTally {
    bounds: RunBounds,
    /// Where a refusal row is appended. `None` = no session log to record on.
    session_log: Option<PathBuf>,
    spent: BTreeMap<OutputKind, u32>,
}

impl OutputTally {
    pub fn new(bounds: RunBounds, session_log: Option<PathBuf>) -> Self {
        Self {
            bounds,
            session_log,
            spent: BTreeMap::new(),
        }
    }

    /// Admit one engine-side write, returning the target it may address. `Err` names the violated
    /// bound; the session row is already on disk by then. A refused write spends no budget.
    pub fn admit(
        &mut self,
        tool: &str,
        kind: OutputKind,
        requested: Option<&str>,
    ) -> Result<Option<String>, BoundViolation> {
        match self.check(kind, requested) {
            Ok(target) => Ok(target),
            Err(violation) => {
                self.log_refusal(tool, &violation);
                Err(violation)
            }
        }
    }

    fn check(
        &mut self,
        kind: OutputKind,
        requested: Option<&str>,
    ) -> Result<Option<String>, BoundViolation> {
        let bound = self
            .bounds
            .resolved
            .get(kind)
            .ok_or(BoundViolation::NoTarget { kind })?;
        let target = resolve_target(bound, requested, &self.bounds.params)?;
        let count = bound.count;
        let used = self.spent.entry(kind).or_insert(0);
        if *used >= count {
            return Err(BoundViolation::CountExhausted { kind, count });
        }
        *used += 1;
        Ok(target)
    }

    /// Append the refusal to the run's session log. Best-effort: a log that cannot be written must
    /// not turn a bounded refusal into a failed run.
    fn log_refusal(&self, tool: &str, violation: &BoundViolation) {
        let Some(path) = &self.session_log else {
            return;
        };
        let line = encode(&SessionEvent::OutputRefused {
            output_kind: violation.kind().to_string(),
            bound: violation.bound(),
            tool: tool.to_string(),
            detail: violation.to_string(),
        });
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(error) = appended {
            tracing::warn!(%error, path = %path.display(), "appending the output-refusal session row failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_contract::outputs::{
        BoundSource, OUTPUTS_WIRE_VERSION, ResolvedOutput, ResolvedTarget,
    };

    pub(crate) fn fixed_bounds(kind: OutputKind, count: u32, target: &str) -> RunBounds {
        RunBounds::new(
            ResolvedOutputs {
                version: OUTPUTS_WIRE_VERSION,
                outputs: vec![ResolvedOutput {
                    kind,
                    count,
                    target: Some(ResolvedTarget::Fixed {
                        fixed: target.into(),
                    }),
                    source: BoundSource::Manifest,
                }],
            },
            BTreeMap::new(),
        )
    }

    #[test]
    fn the_count_is_spent_per_kind_and_exhaustion_names_the_bound() {
        let mut t = OutputTally::new(fixed_bounds(OutputKind::DraftPr, 2, "owner/repo"), None);
        for _ in 0..2 {
            assert_eq!(
                t.admit("publish", OutputKind::DraftPr, Some("owner/repo")),
                Ok(Some("owner/repo".to_string()))
            );
        }
        let err = t
            .admit("publish", OutputKind::DraftPr, Some("owner/repo"))
            .expect_err("the third PR is over the count");
        assert_eq!(
            err,
            BoundViolation::CountExhausted {
                kind: OutputKind::DraftPr,
                count: 2
            }
        );
        assert_eq!(err.bound(), "[outputs.draft-pr].count = 2");
    }

    #[test]
    fn a_target_outside_the_bound_is_refused_and_spends_nothing() {
        let mut t = OutputTally::new(
            fixed_bounds(OutputKind::WorkflowDispatch, 1, "owner/repo"),
            None,
        );
        let err = t
            .admit("build", OutputKind::WorkflowDispatch, Some("evil/repo"))
            .expect_err("a repo outside the resolved target");
        assert_eq!(
            err,
            BoundViolation::TargetOutOfScope {
                kind: OutputKind::WorkflowDispatch,
                requested: "evil/repo".into(),
                scope: "owner/repo".into(),
            }
        );
        assert_eq!(
            t.admit("build", OutputKind::WorkflowDispatch, Some("owner/repo")),
            Ok(Some("owner/repo".to_string())),
            "the refused dispatch did not consume the single allowed one"
        );
    }

    #[test]
    fn a_kind_missing_from_the_projection_is_refused() {
        let empty = RunBounds::new(
            ResolvedOutputs {
                version: OUTPUTS_WIRE_VERSION,
                outputs: Vec::new(),
            },
            BTreeMap::new(),
        );
        let mut t = OutputTally::new(empty, None);
        assert!(t.admit("publish", OutputKind::DraftPr, None).is_err());
    }

    #[test]
    fn a_refusal_writes_a_decodable_session_row() {
        let dir = std::env::temp_dir().join(format!("crucible-tally-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("session.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut t = OutputTally::new(
            fixed_bounds(OutputKind::DraftPr, 0, "owner/repo"),
            Some(path.clone()),
        );
        assert!(
            t.admit("publish", OutputKind::DraftPr, Some("owner/repo"))
                .is_err()
        );
        let text = std::fs::read_to_string(&path).expect("the row landed");
        match crucible_contract::session::decode(text.trim()).expect("decodes") {
            SessionEvent::OutputRefused {
                output_kind,
                bound,
                tool,
                ..
            } => {
                assert_eq!(output_kind, "draft-pr");
                assert_eq!(tool, "publish");
                assert_eq!(bound, "[outputs.draft-pr].count = 0");
            }
            other => panic!("wrong event: {other:?}"),
        }
        std::fs::remove_file(&path).expect("cleanup");
    }
}
