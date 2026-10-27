//! Settings a launch sets on a run: agent settings per task, and the ref the repo is checked
//! out at.
//!
//! A launch addresses a setting by name in the same `name=value` map as declared parameters:
//! `<task>.<setting>` for one agent task, `*.<setting>` for every agent task, `repo.ref` for the
//! checkout. A declared parameter name cannot contain a dot, so the two never collide.
//!
//! ```text
//!   lowest                                                              highest
//!   [agent] / launch flags  <  *.<setting>  <  the task's own  <  <task>.<setting>
//! ```

use std::collections::BTreeMap;

use serde::Serialize;

use crate::manifest::{
    AgentOverridesCfg, GitRef, GitRefError, Harness, ReasoningEffort, RepoOverridesCfg,
};
use crate::plan::diag;
use crate::plan::ir::{Task, TaskKind, TaskName};

/// An agent setting a launch may set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Setting {
    Harness,
    Model,
    Effort,
}

impl Setting {
    pub const ALL: [Setting; 3] = [Setting::Harness, Setting::Model, Setting::Effort];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Setting::Harness => "harness",
            Setting::Model => "model",
            Setting::Effort => "effort",
        }
    }
}

impl std::fmt::Display for Setting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which agent tasks a launch value applies to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(into = "String")]
pub enum Target {
    /// `*`: every agent task that does not set the value itself.
    All,
    /// One agent task, over whatever it sets itself.
    Task(TaskName),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::All => f.write_str("*"),
            Target::Task(name) => f.write_str(&name.0),
        }
    }
}

impl From<Target> for String {
    fn from(target: Target) -> String {
        target.to_string()
    }
}

/// One target's validated settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<Harness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
}

impl AgentSettings {
    fn set(&self) -> impl Iterator<Item = Setting> {
        [
            self.harness.map(|_| Setting::Harness),
            self.model.as_ref().map(|_| Setting::Model),
            self.effort.map(|_| Setting::Effort),
        ]
        .into_iter()
        .flatten()
    }
}

/// The address of the checkout's ref.
pub const REPO_REF: &str = "repo.ref";

/// What one launch set: agent settings by target, and the ref to check out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LaunchSettings {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    agents: BTreeMap<Target, AgentSettings>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    repo_ref: Option<GitRef>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SettingsError {
    #[error(
        "launch value {address:?} names no task; address one as <task>.<setting> or *.<setting>"
    )]
    NoTarget { address: String },
    #[error(
        "launch value {address:?} names setting {got:?}; a launch sets harness, model or effort \
         on a task, or ref on repo{}",
        suggestion.map(|s| format!("; did you mean {s:?}?")).unwrap_or_default()
    )]
    UnknownSetting {
        address: String,
        got: String,
        suggestion: Option<&'static str>,
    },
    #[error("launch value {address:?} sets ref, which is set on the checkout as {REPO_REF}")]
    RefOnTask { address: String },
    #[error("launch value {REPO_REF:?}: {source}")]
    BadRef {
        #[source]
        source: GitRefError,
    },
    #[error("launch value {REPO_REF:?} names {got:?}, but [repo.overrides].refs lists no pattern")]
    NoRefPatterns { got: String },
    #[error(
        "launch value {REPO_REF:?} names {got:?}, which no [repo.overrides].refs pattern matches"
    )]
    RefNotListed { got: String },
    #[error("launch value {address:?} is {got:?}, which is not {expected}")]
    BadValue {
        address: String,
        got: String,
        expected: &'static str,
    },
    #[error("launch value {address:?} sets {setting}, which [agent.overrides].allow does not list")]
    NotAllowed { address: String, setting: Setting },
    #[error(
        "launch value {address:?} names model {got:?}, which [agent.overrides].models does not list"
    )]
    ModelNotListed { address: String, got: String },
    #[error(
        "launch value {address:?} names task {task:?}, which the workflow does not contain{}",
        suggestion.as_ref().map(|s| format!("; did you mean {s:?}?")).unwrap_or_default()
    )]
    UnknownTask {
        address: String,
        task: String,
        suggestion: Option<String>,
    },
    #[error("launch value {address:?} names task {task:?}, which runs no agent")]
    NotAgentTask { address: String, task: String },
    #[error(
        "launch value {address:?} names task {task:?}, which shares session {session:?}; a \
         session runs under one agent config, so a launch cannot change one of its tasks"
    )]
    SessionTask {
        address: String,
        task: String,
        session: String,
    },
}

/// Whether a supplied name addresses a setting rather than a declared parameter.
pub fn is_address(name: &str) -> bool {
    name.contains('.')
}

/// Supplied values as (declared parameters, setting addresses).
pub fn split(
    supplied: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    supplied
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .partition(|(name, _)| !is_address(name))
}

/// A harness name, read the way a turn reads it.
pub fn parse_harness(raw: &str) -> Option<Harness> {
    <Harness as clap::ValueEnum>::from_str(raw, true).ok()
}

/// A reasoning effort, read the way a turn reads it.
pub fn parse_effort(raw: &str) -> Option<ReasoningEffort> {
    <ReasoningEffort as clap::ValueEnum>::from_str(raw, true).ok()
}

pub const HARNESS_NAMES: &str = "claude, hermes, codex, opencode or pi";
pub const EFFORT_NAMES: &str = "low, medium, high, xhigh or max";
const MODEL_SHAPE: &str = "a model name: letters, digits and . _ - : / @ [ ], not starting with -";

/// A model name that reaches a harness as exactly one argument.
pub fn is_model_name(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 200
        && !raw.starts_with('-')
        && raw.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@' | '[' | ']')
        })
}

impl LaunchSettings {
    /// Read setting addresses and values. Whether the pack allows them, and whether the tasks
    /// they name exist, is [`LaunchSettings::apply`]'s to check.
    pub fn parse(addressed: &BTreeMap<String, String>) -> Result<Self, SettingsError> {
        let mut settings: BTreeMap<Target, AgentSettings> = BTreeMap::new();
        let mut repo_ref = None;
        for (address, raw) in addressed {
            if address == REPO_REF {
                repo_ref =
                    Some(GitRef::parse(raw).map_err(|source| SettingsError::BadRef { source })?);
                continue;
            }
            let (target, name) =
                address
                    .rsplit_once('.')
                    .ok_or_else(|| SettingsError::NoTarget {
                        address: address.clone(),
                    })?;
            if target.is_empty() {
                return Err(SettingsError::NoTarget {
                    address: address.clone(),
                });
            }
            if name == "ref" {
                return Err(SettingsError::RefOnTask {
                    address: address.clone(),
                });
            }
            let setting = Setting::parse(name).ok_or_else(|| SettingsError::UnknownSetting {
                address: address.clone(),
                got: name.to_owned(),
                suggestion: diag::suggest(
                    name,
                    Setting::ALL.map(Setting::as_str).into_iter().chain(["ref"]),
                ),
            })?;
            let bad = |expected| SettingsError::BadValue {
                address: address.clone(),
                got: raw.clone(),
                expected,
            };
            let target = match target {
                "*" => Target::All,
                task => Target::Task(TaskName(task.to_owned())),
            };
            let entry = settings.entry(target).or_default();
            match setting {
                Setting::Harness => {
                    entry.harness = Some(parse_harness(raw).ok_or_else(|| bad(HARNESS_NAMES))?);
                }
                Setting::Effort => {
                    entry.effort = Some(parse_effort(raw).ok_or_else(|| bad(EFFORT_NAMES))?);
                }
                Setting::Model => {
                    if !is_model_name(raw) {
                        return Err(bad(MODEL_SHAPE));
                    }
                    entry.model = Some(raw.clone());
                }
            }
        }
        Ok(Self {
            agents: settings,
            repo_ref,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty() && self.repo_ref.is_none()
    }

    /// The `*.<setting>` values.
    pub fn all(&self) -> Option<&AgentSettings> {
        self.agents.get(&Target::All)
    }

    /// The `repo.ref` value.
    pub fn repo_ref(&self) -> Option<&GitRef> {
        self.repo_ref.as_ref()
    }

    /// Refuse a `repo.ref` the pack's patterns do not admit. A commit is admitted here and
    /// checked once it is cloned, against the branches and tags that contain it.
    pub fn check_ref(&self, gate: &RepoOverridesCfg) -> Result<(), SettingsError> {
        let Some(r) = &self.repo_ref else {
            return Ok(());
        };
        if gate.is_empty() {
            return Err(SettingsError::NoRefPatterns { got: r.to_string() });
        }
        if !r.is_commit() && !gate.allows(r.as_str()) {
            return Err(SettingsError::RefNotListed { got: r.to_string() });
        }
        Ok(())
    }

    /// Refuse what `gate` does not allow and every address `tasks` cannot take, then write each
    /// value onto the agent tasks it applies to.
    pub fn apply(&self, tasks: &mut [Task], gate: &AgentOverridesCfg) -> Result<(), SettingsError> {
        for (target, settings) in &self.agents {
            for setting in settings.set() {
                if !gate.allow.contains(&setting) {
                    return Err(SettingsError::NotAllowed {
                        address: format!("{target}.{setting}"),
                        setting,
                    });
                }
            }
            if let Some(model) = &settings.model
                && !gate.models.is_empty()
                && !gate.models.contains(model)
            {
                return Err(SettingsError::ModelNotListed {
                    address: format!("{target}.model"),
                    got: model.clone(),
                });
            }
            if let Target::Task(name) = target {
                check_target(tasks, name, settings)?;
            }
        }
        let all = self.agents.get(&Target::All);
        for task in tasks.iter_mut() {
            let own = self.agents.get(&Target::Task(task.name.clone()));
            if let TaskKind::Agent {
                harness,
                model,
                effort,
                ..
            } = &mut task.task
            {
                let pick = |field: fn(&AgentSettings) -> Option<String>,
                            current: &mut Option<String>| {
                    if let Some(value) = own.and_then(field) {
                        *current = Some(value);
                    } else if current.is_none() {
                        *current = all.and_then(field);
                    }
                };
                pick(|s| s.harness.map(|h| h.as_str().to_owned()), harness);
                pick(|s| s.model.clone(), model);
                pick(|s| s.effort.map(|e| e.as_flag().to_owned()), effort);
            }
        }
        Ok(())
    }
}

/// `name` must be an agent task outside any session.
fn check_target(
    tasks: &[Task],
    name: &TaskName,
    settings: &AgentSettings,
) -> Result<(), SettingsError> {
    let address = || {
        let first = settings.set().next().map_or("", Setting::as_str);
        format!("{name}.{first}", name = name.0)
    };
    let Some(task) = tasks.iter().find(|t| &t.name == name) else {
        let agents = tasks
            .iter()
            .filter(|t| matches!(t.task, TaskKind::Agent { .. }))
            .map(|t| t.name.0.as_str());
        return Err(SettingsError::UnknownTask {
            address: address(),
            task: name.0.clone(),
            suggestion: diag::suggest(&name.0, agents).map(str::to_owned),
        });
    };
    if !matches!(task.task, TaskKind::Agent { .. }) {
        return Err(SettingsError::NotAgentTask {
            address: address(),
            task: name.0.clone(),
        });
    }
    if let Some(session) = &task.session {
        return Err(SettingsError::SessionTask {
            address: address(),
            task: name.0.clone(),
            session: session.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::manifest::{AgentOverridesCfg, GitRef, Harness, ReasoningEffort, RepoOverridesCfg};
    use crate::plan::ir::{Task, TaskKind};
    use crate::plan::settings::{LaunchSettings, Setting, SettingsError, split};
    use crate::plan::workflow::WorkflowCfg;

    fn supplied(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn parse(pairs: &[(&str, &str)]) -> Result<LaunchSettings, SettingsError> {
        LaunchSettings::parse(&supplied(pairs))
    }

    fn allowing(allow: &[Setting], models: &[&str]) -> AgentOverridesCfg {
        AgentOverridesCfg {
            allow: allow.iter().copied().collect(),
            models: models.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn all_allowed() -> AgentOverridesCfg {
        allowing(&Setting::ALL, &[])
    }

    /// plan: pins nothing. review: pins model and effort. write: pins model. build: a command.
    /// pair-a and pair-b: share a session that pins harness.
    fn tasks() -> Vec<Task> {
        toml::from_str::<WorkflowCfg>(
            "[[task]]\nname = \"plan\"\nkind = \"agent\"\nprompt = \"p\"\n\
             [[task]]\nname = \"review\"\nkind = \"agent\"\nprompt = \"p\"\nmodel = \"authored-review\"\neffort = \"low\"\n\
             [[task]]\nname = \"write\"\nkind = \"agent\"\nprompt = \"p\"\nmodel = \"authored-write\"\n\
             [[task]]\nname = \"build\"\nkind = \"command\"\ncommand = \"true\"\n\
             [[task]]\nname = \"pair-a\"\nkind = \"agent\"\nprompt = \"p\"\nsession = \"pair\"\nharness = \"codex\"\n\
             [[task]]\nname = \"pair-b\"\nkind = \"agent\"\nprompt = \"p\"\nsession = \"pair\"\nharness = \"codex\"\ndepends_on = [\"pair-a\"]\n",
        )
        .expect("workflow parses")
        .tasks
    }

    fn knobs(tasks: &[Task], name: &str) -> (Option<String>, Option<String>, Option<String>) {
        let task = tasks
            .iter()
            .find(|t| t.name.0 == name)
            .expect("task exists");
        match &task.task {
            TaskKind::Agent {
                harness,
                model,
                effort,
                ..
            } => (harness.clone(), model.clone(), effort.clone()),
            other => panic!("{name} is not an agent task: {other:?}"),
        }
    }

    fn s(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    #[test]
    fn a_dotted_name_is_a_setting_and_anything_else_a_declared_param() {
        let (declared, addressed) = split(&supplied(&[
            ("topic", "sinks"),
            ("review.model", "m"),
            ("*.effort", "high"),
            ("depth", "3"),
        ]));
        assert_eq!(declared, supplied(&[("depth", "3"), ("topic", "sinks")]));
        assert_eq!(
            addressed,
            supplied(&[("*.effort", "high"), ("review.model", "m")])
        );
    }

    #[test]
    fn values_are_read_typed_and_addressed_by_their_last_dot() {
        let launch = parse(&[
            ("*.harness", "Codex"),
            ("*.effort", "HIGH"),
            ("stage.one.model", "anthropic/claude-opus-4-6[1m]"),
        ])
        .expect("parses");
        let all = launch.all().expect("a value for every task");
        assert_eq!(all.harness, Some(Harness::Codex));
        assert_eq!(all.effort, Some(ReasoningEffort::High));
        assert_eq!(
            serde_json::to_value(&launch).expect("json"),
            serde_json::json!({"agents": {
                "*": {"harness": "codex", "effort": "high"},
                "stage.one": {"model": "anthropic/claude-opus-4-6[1m]"},
            }}),
            "a task name may itself contain dots; the setting is after the last one"
        );
        assert!(parse(&[]).expect("nothing supplied").is_empty());
    }

    #[test]
    fn a_bad_address_setting_or_value_is_refused_by_name() {
        assert_eq!(
            parse(&[(".model", "m")]),
            Err(SettingsError::NoTarget {
                address: ".model".into()
            })
        );
        assert_eq!(
            parse(&[("review.modle", "m")]),
            Err(SettingsError::UnknownSetting {
                address: "review.modle".into(),
                got: "modle".into(),
                suggestion: Some("model"),
            })
        );
        assert!(matches!(
            parse(&[("review.sandbox", "gpu")]),
            Err(SettingsError::UnknownSetting {
                suggestion: None,
                ..
            })
        ));
        for (address, value) in [
            ("*.harness", "gpt"),
            ("*.effort", "extreme"),
            ("*.effort", ""),
            ("review.model", ""),
            ("review.model", "--dangerously-skip-permissions"),
            ("review.model", "opus; rm -rf /"),
            ("review.model", "opus 4"),
        ] {
            assert!(
                matches!(
                    parse(&[(address, value)]),
                    Err(SettingsError::BadValue { ref got, .. }) if got == value
                ),
                "{address}={value:?} was read"
            );
        }
        assert!(
            parse(&[("review.model", &"m".repeat(201))]).is_err(),
            "an unbounded model name is refused"
        );
    }

    #[test]
    fn the_more_specific_value_wins_and_a_launch_wins_a_tie() {
        let mut tasks = tasks();
        let launch = parse(&[
            ("*.model", "launched-all"),
            ("*.effort", "high"),
            ("review.model", "launched-review"),
        ])
        .expect("parses");
        launch.apply(&mut tasks, &all_allowed()).expect("applies");

        assert_eq!(
            knobs(&tasks, "plan"),
            (None, s("launched-all"), s("high")),
            "a task that sets nothing takes *.<setting>"
        );
        assert_eq!(
            knobs(&tasks, "review"),
            (None, s("launched-review"), s("low")),
            "<task>.<setting> replaces the task's own value; *.<setting> does not"
        );
        assert_eq!(
            knobs(&tasks, "write"),
            (None, s("authored-write"), s("high")),
            "the task's own value beats *.<setting>"
        );
        assert_eq!(
            knobs(&tasks, "pair-a"),
            (s("codex"), s("launched-all"), s("high")),
            "a session's tasks take *.<setting> alike"
        );
        assert_eq!(knobs(&tasks, "pair-a"), knobs(&tasks, "pair-b"));
        let build = tasks.iter().find(|t| t.name.0 == "build").expect("build");
        assert!(
            matches!(&build.task, TaskKind::Command { command } if command == "true"),
            "a command task is untouched"
        );
    }

    #[test]
    fn nothing_supplied_changes_nothing() {
        let mut tasks = tasks();
        LaunchSettings::default()
            .apply(&mut tasks, &AgentOverridesCfg::default())
            .expect("nothing to refuse");
        assert_eq!(
            serde_json::to_value(&tasks).expect("json"),
            serde_json::to_value(self::tasks()).expect("json")
        );
    }

    #[test]
    fn the_pack_decides_which_settings_and_models_a_launch_may_set() {
        let refused = |pairs: &[(&str, &str)], gate: &AgentOverridesCfg| {
            parse(pairs)
                .expect("parses")
                .apply(&mut tasks(), gate)
                .expect_err("refused")
        };
        assert_eq!(
            refused(&[("*.model", "m")], &AgentOverridesCfg::default()),
            SettingsError::NotAllowed {
                address: "*.model".into(),
                setting: Setting::Model,
            },
            "a pack that declares no [agent.overrides] takes no setting"
        );
        assert_eq!(
            refused(
                &[("review.harness", "pi")],
                &allowing(&[Setting::Model, Setting::Effort], &[])
            ),
            SettingsError::NotAllowed {
                address: "review.harness".into(),
                setting: Setting::Harness,
            }
        );
        let listed = allowing(&[Setting::Model], &["claude-opus-5-5", "claude-sonnet-5-5"]);
        assert_eq!(
            refused(&[("plan.model", "gpt-5.6-sol")], &listed),
            SettingsError::ModelNotListed {
                address: "plan.model".into(),
                got: "gpt-5.6-sol".into(),
            }
        );
        let mut tasks = tasks();
        parse(&[("plan.model", "claude-sonnet-5-5")])
            .expect("parses")
            .apply(&mut tasks, &listed)
            .expect("a listed model");
        assert_eq!(knobs(&tasks, "plan").1, s("claude-sonnet-5-5"));
    }

    #[test]
    fn an_address_must_name_an_agent_task_outside_a_session() {
        let refused = |pairs: &[(&str, &str)]| {
            parse(pairs)
                .expect("parses")
                .apply(&mut tasks(), &all_allowed())
                .expect_err("refused")
        };
        assert_eq!(
            refused(&[("reveiw.model", "m")]),
            SettingsError::UnknownTask {
                address: "reveiw.model".into(),
                task: "reveiw".into(),
                suggestion: Some("review".into()),
            }
        );
        assert_eq!(
            refused(&[("build.effort", "high")]),
            SettingsError::NotAgentTask {
                address: "build.effort".into(),
                task: "build".into(),
            }
        );
        assert_eq!(
            refused(&[("pair-b.model", "m")]),
            SettingsError::SessionTask {
                address: "pair-b.model".into(),
                task: "pair-b".into(),
                session: "pair".into(),
            }
        );
    }

    fn ref_gate(refs: &[&str]) -> RepoOverridesCfg {
        toml::from_str(&format!("refs = {refs:?}")).expect("parses")
    }

    #[test]
    fn repo_ref_is_read_as_a_git_ref_and_only_on_repo() {
        let launch =
            parse(&[("repo.ref", "refs/heads/feature/x"), ("*.effort", "low")]).expect("parses");
        assert_eq!(
            launch.repo_ref(),
            Some(&GitRef::parse("refs/heads/feature/x").expect("ref"))
        );
        assert_eq!(
            serde_json::to_value(&launch).expect("json"),
            serde_json::json!({"agents": {"*": {"effort": "low"}}, "ref": "refs/heads/feature/x"})
        );
        assert!(!parse(&[("repo.ref", "main")]).expect("parses").is_empty());

        for address in ["review.ref", "*.ref"] {
            assert_eq!(
                parse(&[(address, "main")]),
                Err(SettingsError::RefOnTask {
                    address: address.into()
                })
            );
        }
        for bad in ["--upload-pack=x", "main..dev", "HEAD~1", ""] {
            assert!(
                matches!(
                    parse(&[("repo.ref", bad)]),
                    Err(SettingsError::BadRef { .. })
                ),
                "{bad:?} was read"
            );
        }
        assert!(matches!(
            parse(&[("review.reff", "main")]),
            Err(SettingsError::UnknownSetting {
                suggestion: Some("ref"),
                ..
            })
        ));
        let mut tasks = toml::from_str::<WorkflowCfg>(
            "[[task]]\nname = \"repo\"\nkind = \"agent\"\nprompt = \"p\"\n",
        )
        .expect("parses")
        .tasks;
        parse(&[("repo.model", "m")])
            .expect("a task named repo takes agent settings")
            .apply(&mut tasks, &all_allowed())
            .expect("applies");
        assert_eq!(knobs(&tasks, "repo").1, s("m"));
    }

    #[test]
    fn repo_ref_must_match_a_pattern_and_a_commit_waits_for_the_clone() {
        let gate = ref_gate(&["main", "feature/*"]);
        for allowed in ["main", "refs/heads/feature/x", "feature/y"] {
            parse(&[("repo.ref", allowed)])
                .expect("parses")
                .check_ref(&gate)
                .expect(allowed);
        }
        assert_eq!(
            parse(&[("repo.ref", "hotfix/x")])
                .expect("parses")
                .check_ref(&gate),
            Err(SettingsError::RefNotListed {
                got: "hotfix/x".into()
            })
        );
        assert_eq!(
            parse(&[("repo.ref", "refs/pull/7/head")])
                .expect("parses")
                .check_ref(&ref_gate(&["**"])),
            Err(SettingsError::RefNotListed {
                got: "refs/pull/7/head".into()
            }),
            "a fork's ref needs a pattern that names refs/pull/"
        );
        assert_eq!(
            parse(&[("repo.ref", "main")])
                .expect("parses")
                .check_ref(&RepoOverridesCfg::default()),
            Err(SettingsError::NoRefPatterns { got: "main".into() })
        );
        parse(&[("repo.ref", "308097caa4b55975e27d58465b1d441bc0bb6c63")])
            .expect("parses")
            .check_ref(&gate)
            .expect("a commit is checked against the refs that hold it once cloned");
        LaunchSettings::default()
            .check_ref(&RepoOverridesCfg::default())
            .expect("no ref, nothing to check");
    }
}
