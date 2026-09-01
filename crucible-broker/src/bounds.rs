//! The single mediation point for the run's declared output bounds (RFC-0001:C-OUTPUTS).
//!
//! Every mutating tool routes its write through [`Bounds::admit`] before performing it. The bounds
//! come from the frozen manifest, projected into this process's environment by the engine, so
//! nothing inside the sandbox can alter one. A refusal fails the requesting call naming the
//! violated bound and writes a session row; it never terminates the run.

use crucible_contract::outputs::{
    BoundViolation, DefaultTargets, ENV_OUTPUT_PARAMS, ENV_OUTPUTS, ENV_SESSION_LOG, OutputKind,
    ResolvedOutputs, resolve_target,
};
use crucible_contract::session::{SessionEvent, encode};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// The run's resolved bounds plus the per-kind tally already spent.
pub struct Bounds {
    resolved: ResolvedOutputs,
    /// Bound workflow parameter values, for an open target's `param` binding.
    params: BTreeMap<String, String>,
    /// Where a refusal row is appended. `None` = no session log was projected.
    session_log: Option<PathBuf>,
    spent: Mutex<BTreeMap<OutputKind, u32>>,
}

impl Bounds {
    /// Load from the engine's projection. Without `BROKER_OUTPUTS` (an operator-started broker, or
    /// an engine that predates the projection) the engine default table applies, computed from
    /// this process's own environment: the conservative posture, never an unbounded one.
    pub fn from_env() -> Self {
        let resolved = std::env::var(ENV_OUTPUTS)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .and_then(|json| match serde_json::from_str::<ResolvedOutputs>(&json) {
                Ok(r) => Some(r),
                Err(error) => {
                    tracing::warn!(%error, "{ENV_OUTPUTS} did not parse; falling back to the engine default table");
                    None
                }
            })
            .unwrap_or_else(|| DefaultTargets::from_env().engine_defaults());
        let params = std::env::var(ENV_OUTPUT_PARAMS)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        Self {
            resolved,
            params,
            session_log: std::env::var(ENV_SESSION_LOG)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from),
            spent: Mutex::new(BTreeMap::new()),
        }
    }

    /// Admit one mediated write, returning the target it may address.
    ///
    /// `requested` is the agent-supplied target, if the tool takes one. `Err` is the refusal text
    /// the tool hands back; the row is already on the session log by then.
    pub fn admit(
        &self,
        tool: &str,
        kind: OutputKind,
        requested: Option<&str>,
    ) -> Result<Option<String>, String> {
        match self.check(kind, requested) {
            Ok(target) => Ok(target),
            Err(violation) => {
                let detail = violation.to_string();
                self.log_refusal(tool, &violation, &detail);
                Err(detail)
            }
        }
    }

    /// Validate a target against a kind's bound without spending its count. A call that addresses
    /// one kind's target while performing another kind's write (a deploy naming the image the
    /// push already produced) checks the address here and spends its own kind's budget through
    /// [`Bounds::admit`].
    pub fn permit_target(
        &self,
        tool: &str,
        kind: OutputKind,
        requested: Option<&str>,
    ) -> Result<Option<String>, String> {
        let bound = match self.resolved.get(kind) {
            Some(b) => b,
            None => {
                let violation = BoundViolation::NoTarget { kind };
                let detail = violation.to_string();
                self.log_refusal(tool, &violation, &detail);
                return Err(detail);
            }
        };
        resolve_target(bound, requested, &self.params).map_err(|violation| {
            let detail = violation.to_string();
            self.log_refusal(tool, &violation, &detail);
            detail
        })
    }

    /// The count/target decision, and the spend on success. The lock spans both so two concurrent
    /// calls cannot each see the last unit of budget.
    fn check(
        &self,
        kind: OutputKind,
        requested: Option<&str>,
    ) -> Result<Option<String>, BoundViolation> {
        let bound = self
            .resolved
            .get(kind)
            .ok_or(BoundViolation::NoTarget { kind })?;
        let target = resolve_target(bound, requested, &self.params)?;
        let mut spent = match self.spent.lock() {
            Ok(g) => g,
            // A poisoned tally cannot be trusted to be under the count, so refuse rather than
            // let an unbounded number of writes through.
            Err(_) => {
                return Err(BoundViolation::CountExhausted {
                    kind,
                    count: bound.count,
                });
            }
        };
        let used = spent.entry(kind).or_insert(0);
        if *used >= bound.count {
            return Err(BoundViolation::CountExhausted {
                kind,
                count: bound.count,
            });
        }
        *used += 1;
        Ok(target)
    }

    /// Append the refusal to the run's session log. Best-effort: a log that cannot be written must
    /// not turn a bounded refusal into a broken tool.
    fn log_refusal(&self, tool: &str, violation: &BoundViolation, detail: &str) {
        let Some(path) = &self.session_log else {
            return;
        };
        let line = encode(&SessionEvent::OutputRefused {
            output_kind: violation.kind().to_string(),
            bound: violation.bound(),
            tool: tool.to_string(),
            detail: detail.to_string(),
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

    /// The resolved bounds, for the tool descriptions and the broker's startup line.
    pub fn resolved(&self) -> &ResolvedOutputs {
        &self.resolved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_contract::outputs::{
        BoundSource, OUTPUTS_WIRE_VERSION, OpenScope, ResolvedOutput, ResolvedTarget,
    };

    fn bounds(outputs: Vec<ResolvedOutput>, log: Option<PathBuf>) -> Bounds {
        Bounds {
            resolved: ResolvedOutputs {
                version: OUTPUTS_WIRE_VERSION,
                outputs,
            },
            params: BTreeMap::new(),
            session_log: log,
            spent: Mutex::new(BTreeMap::new()),
        }
    }

    fn fixed(kind: OutputKind, count: u32, target: &str) -> ResolvedOutput {
        ResolvedOutput {
            kind,
            count,
            target: Some(ResolvedTarget::Fixed {
                fixed: target.into(),
            }),
            source: BoundSource::Manifest,
        }
    }

    #[test]
    fn the_count_is_spent_per_kind_and_the_refusal_names_the_bound() {
        let b = bounds(vec![fixed(OutputKind::Deploy, 2, "ns/app")], None);
        for _ in 0..2 {
            assert_eq!(
                b.admit("deploy_candidate", OutputKind::Deploy, None),
                Ok(Some("ns/app".to_string()))
            );
        }
        let err = b
            .admit("deploy_candidate", OutputKind::Deploy, None)
            .expect_err("the third write is over the count");
        assert!(err.contains("budget is spent"), "{err}");
        assert!(err.contains("2"), "{err}");
    }

    #[test]
    fn a_refused_target_never_spends_budget() {
        let b = bounds(vec![fixed(OutputKind::ImagePush, 1, "quay.io/aipcc")], None);
        assert!(
            b.admit("build_epp", OutputKind::ImagePush, Some("docker.io/evil"))
                .is_err()
        );
        assert_eq!(
            b.admit(
                "build_epp",
                OutputKind::ImagePush,
                Some("quay.io/aipcc/x:1")
            ),
            Ok(Some("quay.io/aipcc/x:1".to_string())),
            "the refused call did not consume the single allowed push"
        );
    }

    #[test]
    fn a_kind_with_no_resolved_target_refuses_every_write() {
        let b = bounds(
            vec![ResolvedOutput {
                kind: OutputKind::TrackerComment,
                count: 5,
                target: None,
                source: BoundSource::EngineDefault,
            }],
            None,
        );
        let err = b
            .admit("jira_add_comment", OutputKind::TrackerComment, Some("P-1"))
            .expect_err("no target resolved");
        assert!(err.contains("addresses a target"), "{err}");
    }

    #[test]
    fn a_kind_missing_from_the_projection_is_refused_not_admitted() {
        let b = bounds(Vec::new(), None);
        assert!(b.admit("report", OutputKind::ChatMessage, None).is_err());
    }

    #[test]
    fn an_open_param_binding_defaults_the_target_to_the_runs_item() {
        let mut b = bounds(
            vec![ResolvedOutput {
                kind: OutputKind::TrackerComment,
                count: 2,
                target: Some(ResolvedTarget::Open {
                    open: OpenScope {
                        scope: "PROJ-*".into(),
                        param: Some("issue_key".into()),
                    },
                }),
                source: BoundSource::Manifest,
            }],
            None,
        );
        b.params
            .insert("issue_key".to_string(), "PROJ-77".to_string());
        assert_eq!(
            b.admit("jira_add_comment", OutputKind::TrackerComment, None),
            Ok(Some("PROJ-77".to_string()))
        );
        let err = b
            .admit(
                "jira_add_comment",
                OutputKind::TrackerComment,
                Some("PROJ-78"),
            )
            .expect_err("a sibling item is not this run's item");
        assert!(err.contains("PROJ-78"), "{err}");
    }

    #[test]
    fn a_refusal_writes_a_decodable_session_row() {
        let dir = std::env::temp_dir().join(format!("crucible-bounds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("session.jsonl");
        let _ = std::fs::remove_file(&path);
        let b = bounds(
            vec![fixed(OutputKind::Deploy, 0, "ns/app")],
            Some(path.clone()),
        );
        assert!(
            b.admit("deploy_candidate", OutputKind::Deploy, None)
                .is_err()
        );
        let text = std::fs::read_to_string(&path).expect("the row landed");
        let event = crucible_contract::session::decode(text.trim()).expect("decodes");
        match event {
            SessionEvent::OutputRefused {
                output_kind,
                bound,
                tool,
                ..
            } => {
                assert_eq!(output_kind, "deploy");
                assert_eq!(tool, "deploy_candidate");
                assert!(bound.contains("[outputs.deploy].count"), "{bound}");
            }
            other => panic!("wrong event: {other:?}"),
        }
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[test]
    fn an_absent_projection_falls_back_to_the_engine_default_table() {
        // No BROKER_OUTPUTS in this test process: every vocabulary kind still resolves, bounded.
        let b = Bounds::from_env();
        for kind in OutputKind::ALL {
            let bound = b.resolved().get(*kind).expect("every kind resolves");
            assert!(bound.count > 0, "{kind} default must be count-bounded");
            assert!(
                !matches!(bound.target, Some(ResolvedTarget::Open { .. })),
                "{kind} default must not carry an open target"
            );
        }
    }
}
