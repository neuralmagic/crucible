//! Where a launch runs: the cluster it dispatches onto, and the provider and model its agent runs
//! against. A launch body, a stored schedule and a launch record all carry the same three pins.

use std::collections::BTreeMap;

use rune::TextEdit;
use serde::Serialize;
use serde_json::Value;

/// The three pins, as a launch or schedule body names them. `None` leaves the controller to
/// resolve its default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RunsOn {
    pub dispatch_target: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl RunsOn {
    /// The pins with blanks read as unset.
    pub fn new(dispatch_target: Option<&str>, provider: Option<&str>, model: Option<&str>) -> Self {
        let set = |v: Option<&str>| {
            v.map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        RunsOn {
            dispatch_target: set(dispatch_target),
            provider: set(provider),
            model: set(model),
        }
    }

    /// Writes the pins into a launch or schedule body, an unset one as null, so a schedule saved
    /// over a stored one drops a pin cleared here.
    pub fn pin(&self, body: &mut Value) {
        if let (Value::Object(body), Ok(Value::Object(pins))) = (body, serde_json::to_value(self)) {
            body.extend(pins);
        }
    }
}

/// A launch that never dispatched a run, so it says nothing about where one runs.
#[derive(Debug, PartialEq, Eq)]
pub struct NeverRan;

/// Where a launch's newest run went, and the agent the launch pinned.
impl TryFrom<&crux::dto::PlaybookLaunch> for RunsOn {
    type Error = NeverRan;

    fn try_from(launch: &crux::dto::PlaybookLaunch) -> Result<Self, NeverRan> {
        let run = launch.runs.first().ok_or(NeverRan)?;
        Ok(RunsOn::new(
            Some(&run.cluster),
            launch.launch.agent_provider.as_deref(),
            launch.launch.agent_model.as_deref(),
        ))
    }
}

impl From<&crux::dto::Schedule> for RunsOn {
    fn from(schedule: &crux::dto::Schedule) -> Self {
        RunsOn::new(
            schedule.dispatch_target.as_deref(),
            schedule.agent_provider.as_deref(),
            schedule.agent_model.as_deref(),
        )
    }
}

/// The key of the newest of `launches` that dispatched a run.
pub fn newest_dispatched(launches: &[crux::dto::PlaybookRun]) -> Option<&str> {
    launches
        .iter()
        .filter(|l| l.runs > 0)
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
        .map(|l| l.key.as_str())
}

/// One option of a picker: what the body sends, what the form shows, and what its tooltip or
/// second line says about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub value: String,
    pub label: String,
    pub detail: String,
}

fn detail(parts: impl IntoIterator<Item = Option<String>>) -> String {
    parts
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// A target by its label, with its kind, whether it is the default, and its owner.
impl From<&crux::dto::DispatchTarget> for Choice {
    fn from(t: &crux::dto::DispatchTarget) -> Self {
        Choice {
            value: t.name.clone(),
            label: if t.label.is_empty() {
                t.name.clone()
            } else {
                t.label.clone()
            },
            detail: detail([
                Some(t.kind.clone()),
                t.default.then(|| "default".to_string()),
                t.owner.clone(),
            ]),
        }
    }
}

/// A provider by its display name, with its harness, default model and owner.
impl From<&crux::dto::Provider> for Choice {
    fn from(p: &crux::dto::Provider) -> Self {
        Choice {
            value: p.id.clone(),
            label: p
                .display_name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| p.id.clone()),
            detail: detail([
                p.harness.clone(),
                Some(p.default_model.clone()),
                Some(p.owner.clone()),
            ]),
        }
    }
}

/// The launch form's pickers: what they offer, what is picked, and whether they show.
#[derive(Clone, Default)]
pub struct RunsOnForm {
    /// The caller's dispatch targets, default first.
    pub targets: Vec<Choice>,
    /// `Default` (the empty value), then the enabled agent providers.
    pub providers: Vec<Choice>,
    default_models: BTreeMap<String, String>,
    pub target: Option<String>,
    /// The picked provider's id; empty for the default.
    pub provider: String,
    pub model: TextEdit,
    pub search: rune_kit::ComboboxState,
    /// Whether the pickers show rather than the one-line summary.
    pub open: bool,
}

impl RunsOnForm {
    /// The pickers over what the controller offers, starting from `before` where it is still
    /// offered and from the default target otherwise. With nothing to start from they open, so a
    /// first launch says where it goes.
    pub fn new(
        targets: &crux::dto::DispatchTargets,
        providers: &[crux::dto::Provider],
        before: RunsOn,
    ) -> Self {
        let mut offered: Vec<&crux::dto::DispatchTarget> = targets.targets.iter().collect();
        offered.sort_by_key(|t| !t.default);
        let agents: Vec<&crux::dto::Provider> = providers
            .iter()
            .filter(|p| p.enabled && (p.roles.is_empty() || p.roles.iter().any(|r| r == "agent")))
            .collect();
        let target = before
            .dispatch_target
            .clone()
            .filter(|name| offered.iter().any(|t| &t.name == name))
            .or_else(|| {
                offered
                    .first()
                    .filter(|t| t.default)
                    .map(|t| t.name.clone())
            });
        let provider = before
            .provider
            .clone()
            .filter(|id| agents.iter().any(|p| &p.id == id))
            .unwrap_or_default();
        RunsOnForm {
            open: before == RunsOn::default(),
            targets: offered.into_iter().map(Choice::from).collect(),
            providers: std::iter::once(Choice {
                value: String::new(),
                label: "Default".to_string(),
                detail: "the controller's dispatch default".to_string(),
            })
            .chain(agents.iter().copied().map(Choice::from))
            .collect(),
            default_models: agents
                .iter()
                .map(|p| (p.id.clone(), p.default_model.clone()))
                .collect(),
            target,
            provider,
            model: TextEdit::new(before.model.as_deref().unwrap_or_default()),
            search: rune_kit::ComboboxState::default(),
        }
    }

    /// What the form pins.
    pub fn runs_on(&self) -> RunsOn {
        RunsOn::new(
            self.target.as_deref(),
            Some(&self.provider),
            Some(&self.model.text),
        )
    }

    /// The model the picked provider runs when none is asked for; empty for the default provider.
    pub fn default_model(&self) -> &str {
        self.default_models
            .get(&self.provider)
            .map_or("", String::as_str)
    }

    /// One line: the target, provider and model, each as the pickers show it.
    pub fn summary(&self) -> String {
        let label = |choices: &[Choice], value: &str| {
            choices
                .iter()
                .find(|c| c.value == value)
                .map_or_else(|| value.to_string(), |c| c.label.clone())
        };
        let model = self.model.text.trim();
        detail([
            self.target.as_deref().map(|t| label(&self.targets, t)),
            Some(label(&self.providers, &self.provider)),
            Some(
                if model.is_empty() {
                    self.default_model()
                } else {
                    model
                }
                .to_string(),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::runs_on::{NeverRan, RunsOn, RunsOnForm, newest_dispatched};

    fn targets() -> crux::dto::DispatchTargets {
        serde_json::from_value(json!({
            "targets": [
                {"name": "pirate", "label": "pirate", "kind": "spoke", "default": false},
                {"name": "waldorf", "label": "waldorf", "kind": "spoke", "default": true},
                {"name": "personal:01a0", "label": "pirate-dispatch", "kind": "personal",
                 "default": false, "owner": "user:weaton"}
            ],
            "refusal": null
        }))
        .unwrap()
    }

    fn providers() -> Vec<crux::dto::Provider> {
        serde_json::from_value(json!([
            {"id": "enmaas", "display_name": "EnMaaS (Claude)", "enabled": true,
             "roles": ["agent"], "harness": "claude", "default_model": "claude-opus-5-5",
             "owner": "team:t"},
            {"id": "off", "enabled": false, "roles": ["agent"]},
            {"id": "judge-only", "enabled": true, "roles": ["judge"]},
            {"id": "openai", "enabled": true}
        ]))
        .unwrap()
    }

    fn shown(choices: &[crate::runs_on::Choice]) -> Vec<(&str, &str, &str)> {
        choices
            .iter()
            .map(|c| (c.value.as_str(), c.label.as_str(), c.detail.as_str()))
            .collect()
    }

    #[test]
    fn the_form_offers_targets_default_first_and_enabled_agent_providers_after_default() {
        let form = RunsOnForm::new(&targets(), &providers(), RunsOn::default());
        assert_eq!(
            shown(&form.targets),
            [
                ("waldorf", "waldorf", "spoke \u{b7} default"),
                ("pirate", "pirate", "spoke"),
                (
                    "personal:01a0",
                    "pirate-dispatch",
                    "personal \u{b7} user:weaton"
                ),
            ]
        );
        assert_eq!(
            shown(&form.providers),
            [
                ("", "Default", "the controller's dispatch default"),
                (
                    "enmaas",
                    "EnMaaS (Claude)",
                    "claude \u{b7} claude-opus-5-5 \u{b7} team:t"
                ),
                ("openai", "openai", ""),
            ]
        );
        assert_eq!(form.target.as_deref(), Some("waldorf"));
        assert!(form.open, "with nothing to start from, the pickers show");
    }

    #[test]
    fn the_form_starts_from_what_ran_before_while_it_is_still_offered() {
        let before = RunsOn::new(Some("personal:01a0"), Some("enmaas"), Some(""));
        let form = RunsOnForm::new(&targets(), &providers(), before);
        assert!(!form.open);
        assert_eq!(form.default_model(), "claude-opus-5-5");
        assert_eq!(
            form.summary(),
            "pirate-dispatch \u{b7} EnMaaS (Claude) \u{b7} claude-opus-5-5"
        );
        assert_eq!(
            form.runs_on(),
            RunsOn::new(Some("personal:01a0"), Some("enmaas"), None)
        );

        let gone = RunsOn::new(Some("retired"), Some("off"), Some("m"));
        let form = RunsOnForm::new(&targets(), &providers(), gone);
        assert_eq!(form.target.as_deref(), Some("waldorf"));
        assert_eq!(
            form.provider, "",
            "a disabled provider falls back to the default"
        );
        assert_eq!(form.summary(), "waldorf \u{b7} Default \u{b7} m");
    }

    #[test]
    fn pins_set_what_was_picked_and_null_what_was_left_to_the_default() {
        let mut body = json!({"params": {}, "max_cost": 1.0});
        RunsOn::new(
            Some("pirate"),
            Some("enmaas"),
            Some(" rits/deepseek-ai/DeepSeek-V4.1-Flash-1017 "),
        )
        .pin(&mut body);
        assert_eq!(
            body,
            json!({"params": {}, "max_cost": 1.0, "dispatch_target": "pirate",
                   "provider": "enmaas", "model": "rits/deepseek-ai/DeepSeek-V4.1-Flash-1017"})
        );
        let mut stored = json!({"dispatch_target": "pirate", "provider": "enmaas", "model": "m"});
        RunsOn::new(None, Some(""), Some(" ")).pin(&mut stored);
        assert_eq!(
            stored,
            json!({"dispatch_target": null, "provider": null, "model": null})
        );
    }

    #[test]
    fn a_launch_runs_on_its_newest_run_s_cluster_with_its_pinned_agent() {
        let launch: crux::dto::PlaybookLaunch = serde_json::from_value(json!({
            "launch": {"params": {}, "agent_provider": "enmaas",
                       "agent_model": "rits/deepseek-ai/DeepSeek-V4.1-Flash-1017"},
            "dispatch": {"state": "dispatched"},
            "runs": [{"run_id": "b", "cluster": "pirate"}, {"run_id": "a", "cluster": "waldorf"}]
        }))
        .unwrap();
        assert_eq!(
            RunsOn::try_from(&launch),
            Ok(RunsOn::new(
                Some("pirate"),
                Some("enmaas"),
                Some("rits/deepseek-ai/DeepSeek-V4.1-Flash-1017")
            ))
        );
        let never: crux::dto::PlaybookLaunch = serde_json::from_value(json!({
            "launch": {"params": {}}, "dispatch": {"state": "pending"}, "runs": []
        }))
        .unwrap();
        assert_eq!(RunsOn::try_from(&never), Err(NeverRan));
    }

    #[test]
    fn a_drop_follows_the_newest_launch_that_dispatched_a_run() {
        let launches: Vec<crux::dto::PlaybookRun> = serde_json::from_value(json!([
            {"key": "old", "runs": 1, "created_at": "2026-11-08T10:00:00Z"},
            {"key": "parked", "runs": 0, "created_at": "2026-11-09T10:00:00Z"},
            {"key": "newer", "runs": 2, "created_at": "2026-11-09T09:00:00Z"}
        ]))
        .unwrap();
        assert_eq!(newest_dispatched(&launches), Some("newer"));
        assert_eq!(newest_dispatched(&launches[1..2]), None);
    }
}
