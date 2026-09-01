//! `[outputs]`: the pack's declared bounds on the mediated writes a run may perform.
//!
//! The vocabulary of kinds lives in [`crucible_contract::outputs`]; this module owns the TOML
//! surface, its validation, and the resolution that folds a declaration onto the engine default
//! table. Resolution is a pure function of the frozen manifest plus the engine's own run context.
//!
//! # Engine default table
//!
//! A vocabulary kind the pack does not declare resolves to the row below. Every default carries a
//! count and no default carries an open target. A default whose target does not resolve refuses
//! every write of that kind.
//!
//! | kind               | count | default target                                     |
//! |--------------------|-------|----------------------------------------------------|
//! | `draft-pr`         | 2     | `[publish].pr_repo`                                 |
//! | `tracker-comment`  | 2     | the run's parameterizing item (`CRUCIBLE_ITEM`)     |
//! | `chat-message`     | 8     | the engine's operator channel                       |
//! | `image-push`       | 100   | `$FORGE_REGISTRY`                                   |
//! | `deploy`           | 100   | `$FORGE_DEPLOY_NAMESPACE/$FORGE_DEPLOY_NAME`        |
//! | `workflow-dispatch`| 20    | the single `[build.*.github].repo`, when unambiguous|
//! | `gpu-capture`      | 100   | addresses nothing                                   |

use anyhow::Result;
use crucible_contract::outputs::{
    BoundSource, DefaultTargets, OUTPUTS_WIRE_VERSION, OpenScope, OutputKind, ResolvedOutput,
    ResolvedOutputs, ResolvedTarget, default_count, scope_is_unbounded,
};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The `[outputs]` table: a declaration per kind. An unknown kind is a parse error (the map is
/// keyed by the closed vocabulary), and so is a malformed bound.
pub type OutputsCfg = BTreeMap<OutputKind, OutputDecl>;

/// One kind's declared bounds.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputDecl {
    /// Writes of this kind allowed per run. Required: a declaration without one is a manifest error.
    pub count: u32,
    /// Required for a kind that addresses a target, rejected for one that does not.
    #[serde(default)]
    pub target: Option<TargetDecl>,
}

/// `target = { fixed = "…" }` or `target = { open = { scope = "…", param = "…" } }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetDecl {
    #[serde(default)]
    pub fixed: Option<String>,
    #[serde(default)]
    pub open: Option<OpenDecl>,
}

/// An explicitly-open target: the scope the agent may address within, optionally pinned to a
/// named workflow parameter's run value.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenDecl {
    pub scope: String,
    #[serde(default)]
    pub param: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum OutputsError {
    #[error("[outputs.{kind}] needs exactly one of `target.fixed` / `target.open`")]
    AmbiguousTarget { kind: OutputKind },
    #[error("[outputs.{kind}] addresses a target, so it must declare one")]
    TargetRequired { kind: OutputKind },
    #[error("[outputs.{kind}] addresses no target, so it must not declare one")]
    TargetRejected { kind: OutputKind },
    #[error(
        "[outputs.{kind}] target {scope:?} admits any target; name a target or scope narrower \
         than the kind's whole address space"
    )]
    UnboundedScope { kind: OutputKind, scope: String },
    #[error("[outputs.{kind}].target.fixed must not be empty")]
    EmptyFixedTarget { kind: OutputKind },
    #[error("[outputs.{kind}].target.open.param must not be empty")]
    EmptyParam { kind: OutputKind },
}

/// Cross-field checks the type system cannot express.
pub fn validate_outputs(cfg: &OutputsCfg) -> Result<()> {
    for (kind, decl) in cfg {
        let kind = *kind;
        match (&decl.target, kind.addresses_target()) {
            (None, true) => return Err(OutputsError::TargetRequired { kind }.into()),
            (Some(_), false) => return Err(OutputsError::TargetRejected { kind }.into()),
            (None, false) => continue,
            (Some(target), true) => validate_target(kind, target)?,
        }
    }
    Ok(())
}

fn validate_target(kind: OutputKind, target: &TargetDecl) -> Result<()> {
    match (&target.fixed, &target.open) {
        (Some(fixed), None) => {
            if fixed.trim().is_empty() {
                return Err(OutputsError::EmptyFixedTarget { kind }.into());
            }
            // A fixed target containing `*` is admitted as a glob at the mediation point, so an
            // unbounded pattern here is an open target in disguise.
            if scope_is_unbounded(fixed) {
                return Err(OutputsError::UnboundedScope {
                    kind,
                    scope: fixed.clone(),
                }
                .into());
            }
            Ok(())
        }
        (None, Some(open)) => {
            if scope_is_unbounded(&open.scope) {
                return Err(OutputsError::UnboundedScope {
                    kind,
                    scope: open.scope.clone(),
                }
                .into());
            }
            if open.param.as_ref().is_some_and(|p| p.trim().is_empty()) {
                return Err(OutputsError::EmptyParam { kind }.into());
            }
            Ok(())
        }
        _ => Err(OutputsError::AmbiguousTarget { kind }.into()),
    }
}

/// The engine's default targets: the shared environment-resolved table plus the two entries that
/// need manifest knowledge.
pub fn default_targets(
    pr_repo: Option<&str>,
    builds: &BTreeMap<String, forge::spec::BuildSpec>,
) -> DefaultTargets {
    // An ambiguous dispatch target is no target: several declared build repos means the pack has
    // to name one itself.
    let mut repos: Vec<&str> = builds
        .values()
        .filter_map(|b| b.github.as_ref().map(|g| g.repo.as_str()))
        .collect();
    repos.sort_unstable();
    repos.dedup();
    let from_env = DefaultTargets::from_env();
    DefaultTargets {
        draft_pr: pr_repo
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or(from_env.draft_pr),
        dispatch_repo: match repos.as_slice() {
            [only] => Some((*only).to_string()),
            _ => None,
        },
        ..from_env
    }
}

/// Fold the pack's declarations onto the engine default table, producing one resolved bound per
/// vocabulary kind.
pub fn resolve(cfg: &OutputsCfg, defaults: &DefaultTargets) -> ResolvedOutputs {
    let outputs = OutputKind::ALL
        .iter()
        .map(|kind| match cfg.get(kind) {
            Some(decl) => ResolvedOutput {
                kind: *kind,
                count: decl.count,
                target: decl.target.as_ref().and_then(declared_target),
                source: BoundSource::Manifest,
            },
            None => ResolvedOutput {
                kind: *kind,
                count: default_count(*kind),
                target: defaults
                    .for_kind(*kind)
                    .map(|fixed| ResolvedTarget::Fixed { fixed }),
                source: BoundSource::EngineDefault,
            },
        })
        .collect();
    ResolvedOutputs {
        version: OUTPUTS_WIRE_VERSION,
        outputs,
    }
}

/// A validated declaration carries exactly one of the two forms.
fn declared_target(target: &TargetDecl) -> Option<ResolvedTarget> {
    match (&target.fixed, &target.open) {
        (Some(fixed), None) => Some(ResolvedTarget::Fixed {
            fixed: fixed.clone(),
        }),
        (None, Some(open)) => Some(ResolvedTarget::Open {
            open: OpenScope {
                scope: open.scope.clone(),
                param: open.param.clone(),
            },
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    const BASE: &str = r#"
        [repo]
        path = "."
        [agent]
        backend = "openshell"
        goal = "g"
        [judge]
        measure_cmd = "m"
        direction = "higher"
    "#;

    fn load(extra: &str) -> Result<Manifest> {
        let text = format!("{BASE}{extra}");
        let m: Manifest = toml::from_str(&text)?;
        m.validate()?;
        Ok(m)
    }

    fn parse(extra: &str) -> Manifest {
        match load(extra) {
            Ok(m) => m,
            Err(e) => panic!("expected {extra:?} to parse: {e:#}"),
        }
    }

    fn refusal(extra: &str) -> String {
        match load(extra) {
            Ok(_) => panic!("expected {extra:?} to be refused"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn a_manifest_with_no_outputs_section_resolves_every_kind_from_the_default_table() {
        let m = parse("");
        assert!(m.outputs.is_empty());
        let resolved = resolve(&m.outputs, &DefaultTargets::default());
        assert_eq!(resolved.outputs.len(), OutputKind::ALL.len());
        for out in &resolved.outputs {
            assert_eq!(out.source, BoundSource::EngineDefault);
            assert!(out.count > 0, "{} default must be count-bounded", out.kind);
            assert!(
                !matches!(out.target, Some(ResolvedTarget::Open { .. })),
                "{} default must not carry an open target",
                out.kind
            );
        }
    }

    #[test]
    fn declared_bounds_win_over_the_default_table() {
        let m = parse(
            r#"
            [outputs.tracker-comment]
            count = 3
            target = { open = { scope = "PROJ-*", param = "issue_key" } }
            [outputs.image-push]
            count = 5
            target = { fixed = "quay.io/aipcc" }
        "#,
        );
        let resolved = resolve(&m.outputs, &DefaultTargets::default());
        let tracker = resolved.get(OutputKind::TrackerComment).expect("present");
        assert_eq!(tracker.count, 3);
        assert_eq!(tracker.source, BoundSource::Manifest);
        assert_eq!(
            tracker.target,
            Some(ResolvedTarget::Open {
                open: OpenScope {
                    scope: "PROJ-*".into(),
                    param: Some("issue_key".into()),
                }
            })
        );
        let push = resolved.get(OutputKind::ImagePush).expect("present");
        assert_eq!(push.count, 5);
        assert_eq!(
            push.target,
            Some(ResolvedTarget::Fixed {
                fixed: "quay.io/aipcc".into()
            })
        );
        assert_eq!(
            resolved.get(OutputKind::DraftPr).expect("present").source,
            BoundSource::EngineDefault
        );
    }

    #[test]
    fn an_unknown_kind_is_a_manifest_error() {
        let err = refusal("[outputs.teleport]\ncount = 1\n");
        assert!(err.contains("teleport"), "{err}");
    }

    #[test]
    fn a_declaration_without_a_count_is_a_manifest_error() {
        let err = refusal("[outputs.gpu-capture]\n");
        assert!(err.contains("count"), "{err}");
    }

    #[test]
    fn an_any_target_scope_is_a_validation_error() {
        for scope in ["*", "**", "*/*", ""] {
            let err = refusal(&format!(
                "[outputs.tracker-comment]\ncount = 1\ntarget = {{ open = {{ scope = \"{scope}\" }} }}\n"
            ));
            assert!(err.contains("admits any target"), "{err}");
        }
    }

    #[test]
    fn a_target_needs_exactly_one_form() {
        let err = refusal(
            "[outputs.draft-pr]\ncount = 1\ntarget = { fixed = \"o/r\", open = { scope = \"o/*\" } }\n",
        );
        assert!(err.contains("exactly one"), "{err}");
        let err = refusal("[outputs.draft-pr]\ncount = 1\ntarget = {}\n");
        assert!(err.contains("exactly one"), "{err}");
    }

    #[test]
    fn an_addressing_kind_must_declare_a_target_and_a_capture_must_not() {
        let err = refusal("[outputs.draft-pr]\ncount = 1\n");
        assert!(err.contains("must declare one"), "{err}");
        let err = refusal("[outputs.gpu-capture]\ncount = 1\ntarget = { fixed = \"x\" }\n");
        assert!(err.contains("must not declare one"), "{err}");
    }

    #[test]
    fn default_targets_read_the_engine_environment_not_pack_content() {
        let two: BTreeMap<String, forge::spec::BuildSpec> = toml::from_str(
            r#"
            [a]
            backend = "github-actions"
            image = "ghcr.io/o/a"
            [a.github]
            repo = "o/a"
            workflow = "build.yml"
            [b]
            backend = "github-actions"
            image = "ghcr.io/o/b"
            [b.github]
            repo = "o/b"
            workflow = "build.yml"
        "#,
        )
        .expect("builds parse");
        assert_eq!(default_targets(None, &two).dispatch_repo, None);
        let one: BTreeMap<String, forge::spec::BuildSpec> = toml::from_str(
            r#"
            [a]
            backend = "github-actions"
            image = "ghcr.io/o/a"
            [a.github]
            repo = "o/a"
            workflow = "build.yml"
        "#,
        )
        .expect("builds parse");
        assert_eq!(
            default_targets(None, &one).dispatch_repo,
            Some("o/a".to_string())
        );
        assert_eq!(
            default_targets(Some("fork/repo"), &one).draft_pr,
            Some("fork/repo".to_string())
        );
    }

    #[test]
    fn an_unbounded_fixed_target_is_refused_and_a_narrow_glob_is_not() {
        let msg = refusal("[outputs.tracker-comment]\ncount = 1\ntarget = { fixed = \"*\" }\n");
        assert!(msg.contains("admits any target"), "{msg}");
        let msg = refusal("[outputs.image-push]\ncount = 1\ntarget = { fixed = \"**\" }\n");
        assert!(msg.contains("admits any target"), "{msg}");
        parse("[outputs.image-push]\ncount = 1\ntarget = { fixed = \"quay.io/org/*\" }\n");
    }
}
