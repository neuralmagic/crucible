//! The webhook form: which playbook a delivery launches, how the sender is verified, the CEL that
//! filters, dedupes and derives params from a delivery, and the bounds on what it may launch.

use std::collections::BTreeMap;

use crux::dto::{Webhook, WebhookBody, WebhookPreset, WebhookVerifier};
use rune::TextEdit;
use serde_json::Value;

use crate::form::Field;

/// How one declared param gets its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The same value on every launch.
    Fixed,
    /// A CEL expression over each delivery.
    Derived,
}

pub struct Choice {
    pub name: String,
    pub mode: Mode,
    pub value: TextEdit,
    pub expression: TextEdit,
}

pub struct WebhookForm {
    /// The webhook being edited, by id; a new one otherwise.
    pub editing: Option<String>,
    pub playbook: String,
    pub verifier: WebhookVerifier,
    /// The header an HMAC signature arrives in.
    pub header: TextEdit,
    pub filter: TextEdit,
    pub dedupe: TextEdit,
    pub choices: Vec<Choice>,
    pub per_hour: TextEdit,
    pub retention: TextEdit,
    pub cost: TextEdit,
    pub time: TextEdit,
    /// The dispatch target, or blank for the controller's default.
    pub target: TextEdit,
    /// A sample delivery body and its `Name: value` headers, for the preview.
    pub sample: TextEdit,
    pub headers: TextEdit,
    /// Derivations the last preset suggested for params this playbook does not declare.
    pub unmatched: Vec<String>,
    /// The playbook `choices` were laid out for.
    laid: Option<String>,
    /// What a stored webhook derives and fixes, until its playbook's params are known.
    stored: (BTreeMap<String, String>, BTreeMap<String, String>),
}

impl WebhookForm {
    pub fn new(playbook: &str) -> WebhookForm {
        WebhookForm {
            editing: None,
            playbook: playbook.to_string(),
            verifier: WebhookVerifier::PathToken,
            header: TextEdit::new(""),
            filter: TextEdit::new("true"),
            dedupe: TextEdit::new("delivery"),
            choices: Vec::new(),
            per_hour: TextEdit::new("10"),
            retention: TextEdit::new("14"),
            cost: TextEdit::new("5"),
            time: TextEdit::new("30m"),
            target: TextEdit::new(""),
            sample: TextEdit::new("{}"),
            headers: TextEdit::new(""),
            unmatched: Vec::new(),
            laid: None,
            stored: (BTreeMap::new(), BTreeMap::new()),
        }
    }

    pub fn editing(hook: &Webhook) -> WebhookForm {
        let verifier = if hook.verifier == "hmac_sha256" {
            WebhookVerifier::HmacSha256
        } else {
            WebhookVerifier::PathToken
        };
        WebhookForm {
            editing: Some(hook.id.clone()),
            verifier,
            header: TextEdit::new(hook.header.as_deref().unwrap_or_default()),
            filter: TextEdit::new(hook.filter.as_deref().unwrap_or("true")),
            dedupe: TextEdit::new(hook.dedupe.as_deref().unwrap_or_default()),
            per_hour: TextEdit::new(hook.max_launches_per_hour.to_string()),
            retention: TextEdit::new(hook.retention_days.to_string()),
            cost: TextEdit::new(format!("{}", hook.max_cost)),
            time: TextEdit::new(&hook.max_time),
            target: TextEdit::new(hook.dispatch_target.as_deref().unwrap_or_default()),
            stored: (hook.derive.clone(), hook.params.clone()),
            ..WebhookForm::new(&hook.playbook)
        }
    }

    /// Change the playbook a new webhook launches; its params are laid out afresh.
    pub fn pick_playbook(&mut self, playbook: &str) {
        if self.playbook != playbook {
            self.playbook = playbook.to_string();
            self.choices.clear();
            self.laid = None;
        }
    }

    /// Whether the params of `playbook` have been laid out.
    pub fn laid_out(&self) -> bool {
        self.laid.as_deref() == Some(self.playbook.as_str())
    }

    /// Lay out one choice per param `playbook` declares, once: derived or fixed as the stored
    /// webhook has it, else fixed at the param's default.
    pub fn lay_out(&mut self, playbook: &str, fields: &[Field]) {
        if playbook != self.playbook || self.laid_out() {
            return;
        }
        let (derive, params) = &self.stored;
        self.choices = fields
            .iter()
            .map(
                |field| match (derive.get(&field.name), params.get(&field.name)) {
                    (Some(expression), _) => Choice {
                        name: field.name.clone(),
                        mode: Mode::Derived,
                        value: TextEdit::new(&field.default),
                        expression: TextEdit::new(expression),
                    },
                    (None, fixed) => Choice {
                        name: field.name.clone(),
                        mode: Mode::Fixed,
                        value: TextEdit::new(fixed.unwrap_or(&field.default)),
                        expression: TextEdit::new(""),
                    },
                },
            )
            .collect();
        self.laid = Some(playbook.to_string());
    }

    /// Fill the verifier and transform from `preset`, deriving each declared param it names. A
    /// stored webhook keeps its verifier, which a save cannot change.
    pub fn apply(&mut self, preset: &WebhookPreset) {
        if self.editing.is_none() {
            self.verifier = preset.verifier;
            self.header = TextEdit::new(preset.header.as_deref().unwrap_or_default());
        }
        self.filter = TextEdit::new(&preset.filter);
        self.dedupe = TextEdit::new(&preset.dedupe);
        self.unmatched.clear();
        for (name, expression) in &preset.derive {
            match self.choices.iter_mut().find(|choice| &choice.name == name) {
                Some(choice) => {
                    choice.mode = Mode::Derived;
                    choice.expression = TextEdit::new(expression);
                }
                None => self.unmatched.push(name.clone()),
            }
        }
    }

    /// The body a save sends, or what is wrong with the form. Blank fixed values and blank
    /// derivations are left out, so a param's default applies.
    pub fn body(&self) -> Result<WebhookBody, String> {
        let whole = |edit: &TextEdit, what: &str, least: i32| {
            edit.text
                .trim()
                .parse::<i32>()
                .ok()
                .filter(|n| *n >= least)
                .ok_or_else(|| format!("{what} must be a whole number, {least} or more"))
        };
        let header = self.header.text.trim();
        if self.verifier == WebhookVerifier::HmacSha256 && header.is_empty() {
            return Err("Name the signature header".into());
        }
        let dedupe = self.dedupe.text.trim();
        if dedupe.is_empty() {
            return Err("Dedupe needs an expression".into());
        }
        let max_cost = self
            .cost
            .text
            .trim()
            .trim_start_matches('$')
            .parse::<f64>()
            .ok()
            .filter(|cost| cost.is_finite() && *cost > 0.0)
            .ok_or("Max cost must be a dollar amount above 0")?;
        let max_time = self.time.text.trim();
        if crate::form::minutes(max_time).is_none() {
            return Err("Max time is a duration such as 30m or 2h".into());
        }
        let mut derive = BTreeMap::new();
        let mut params = BTreeMap::new();
        for choice in &self.choices {
            let (into, text) = match choice.mode {
                Mode::Derived => (&mut derive, choice.expression.text.trim()),
                Mode::Fixed => (&mut params, choice.value.text.trim()),
            };
            if !text.is_empty() {
                into.insert(choice.name.clone(), text.to_string());
            }
        }
        let filter = self.filter.text.trim();
        let target = self.target.text.trim();
        Ok(WebhookBody {
            playbook: self.playbook.clone(),
            verifier: self.verifier,
            header: (self.verifier == WebhookVerifier::HmacSha256).then(|| header.to_string()),
            filter: if filter.is_empty() { "true" } else { filter }.to_string(),
            dedupe: dedupe.to_string(),
            derive,
            params,
            max_launches_per_hour: whole(&self.per_hour, "Launches per hour", 1)?,
            retention_days: whole(&self.retention, "Retention days", 1)?,
            max_cost,
            max_time: max_time.to_string(),
            dispatch_target: (!target.is_empty()).then(|| target.to_string()),
        })
    }

    /// The sample delivery a preview runs on: its JSON body and its headers.
    pub fn sample(&self) -> Result<(Value, BTreeMap<String, String>), String> {
        let body = serde_json::from_str(&self.sample.text)
            .map_err(|error| format!("The sample is not JSON: {error}"))?;
        Ok((body, headers(&self.headers.text)?))
    }
}

/// `Name: value` lines as a header map, names lowercased. Blank lines are skipped.
pub fn headers(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once(':') {
            Some((name, value)) if !name.trim().is_empty() => {
                map.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
            _ => return Err(format!("Header line {} is not \"Name: value\"", index + 1)),
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crux::dto::{Webhook, WebhookPreset, WebhookVerifier};
    use rune::TextEdit;

    use crate::form::{Field, Kind};
    use crate::webhook::{Mode, WebhookForm, headers};

    fn field(name: &str, default: &str) -> Field {
        Field {
            name: name.into(),
            kind: Kind::Text,
            required: false,
            description: String::new(),
            default: default.into(),
        }
    }

    fn triage() -> Vec<Field> {
        vec![field("repo", ""), field("label", ""), field("limit", "6")]
    }

    fn github() -> WebhookPreset {
        WebhookPreset {
            id: "github-issues".into(),
            title: "GitHub issues".into(),
            verifier: WebhookVerifier::HmacSha256,
            header: Some("x-hub-signature-256".into()),
            filter: "body.action == \"opened\"".into(),
            dedupe: "string(body.issue.id)".into(),
            derive: BTreeMap::from([
                ("repo".into(), "body.repository.full_name".into()),
                ("sender".into(), "body.sender.login".into()),
            ]),
        }
    }

    #[test]
    fn a_new_form_fixes_each_param_at_its_default_and_sends_only_what_is_set() {
        let mut form = WebhookForm::new("triage");
        assert!(!form.laid_out());
        form.lay_out("other", &triage());
        assert!(
            form.choices.is_empty(),
            "another playbook's params were laid out"
        );
        form.lay_out("triage", &triage());
        assert!(form.laid_out());
        form.choices[0].value = TextEdit::new(" llm-d/llm-d ");

        let body = form.body().unwrap();
        assert_eq!(body.playbook, "triage");
        assert_eq!(body.verifier, WebhookVerifier::PathToken);
        assert_eq!(body.header, None);
        assert_eq!(
            (body.filter.as_str(), body.dedupe.as_str()),
            ("true", "delivery")
        );
        assert_eq!(
            body.params,
            BTreeMap::from([
                ("repo".to_string(), "llm-d/llm-d".to_string()),
                ("limit".to_string(), "6".to_string()),
            ])
        );
        assert!(body.derive.is_empty());
        assert_eq!(
            (
                body.max_launches_per_hour,
                body.retention_days,
                body.max_cost
            ),
            (10, 14, 5.0)
        );
        assert_eq!(
            (body.max_time.as_str(), body.dispatch_target),
            ("30m", None)
        );

        form.lay_out("triage", &[field("only", "x")]);
        assert_eq!(
            form.choices.len(),
            3,
            "a second schema reply refilled the form"
        );
    }

    #[test]
    fn a_preset_sets_the_verifier_and_derives_the_params_it_names() {
        let mut form = WebhookForm::new("triage");
        form.lay_out("triage", &triage());
        form.apply(&github());
        assert_eq!(form.verifier, WebhookVerifier::HmacSha256);
        assert_eq!(form.unmatched, ["sender"]);
        assert_eq!(form.choices[0].mode, Mode::Derived);
        assert_eq!(form.choices[2].mode, Mode::Fixed);

        let body = form.body().unwrap();
        assert_eq!(body.header.as_deref(), Some("x-hub-signature-256"));
        assert_eq!(body.filter, "body.action == \"opened\"");
        assert_eq!(body.derive["repo"], "body.repository.full_name");
        assert_eq!(
            body.params,
            BTreeMap::from([("limit".to_string(), "6".to_string())])
        );
    }

    #[test]
    fn editing_reads_the_stored_webhook_and_keeps_its_verifier_through_a_preset() {
        let hook = Webhook {
            id: "w1".into(),
            playbook: "triage".into(),
            verifier: "path_token".into(),
            max_launches_per_hour: 3,
            retention_days: 7,
            max_cost: 3.0,
            max_time: "25m".into(),
            dispatch_target: Some("waldorf".into()),
            filter: Some("headers[\"x-github-event\"] == \"issues\"".into()),
            dedupe: Some("string(body.issue.id)".into()),
            derive: BTreeMap::from([("repo".into(), "body.repository.full_name".into())]),
            params: BTreeMap::from([("limit".into(), "1".into())]),
            ..Webhook::default()
        };
        let mut form = WebhookForm::editing(&hook);
        form.lay_out("triage", &triage());
        let modes: Vec<(&str, Mode, &str)> = form
            .choices
            .iter()
            .map(|c| {
                let text = match c.mode {
                    Mode::Derived => c.expression.text.as_str(),
                    Mode::Fixed => c.value.text.as_str(),
                };
                (c.name.as_str(), c.mode, text)
            })
            .collect();
        assert_eq!(
            modes,
            [
                ("repo", Mode::Derived, "body.repository.full_name"),
                ("label", Mode::Fixed, ""),
                ("limit", Mode::Fixed, "1"),
            ]
        );
        let body = form.body().unwrap();
        assert_eq!(
            (
                body.max_launches_per_hour,
                body.retention_days,
                body.max_cost
            ),
            (3, 7, 3.0)
        );
        assert_eq!(body.dispatch_target.as_deref(), Some("waldorf"));

        form.apply(&github());
        assert_eq!(form.verifier, WebhookVerifier::PathToken);
        assert_eq!(form.header.text, "");
    }

    #[test]
    fn the_form_refuses_what_the_controller_would() {
        let broken = |change: fn(&mut WebhookForm)| {
            let mut form = WebhookForm::new("triage");
            change(&mut form);
            form.body().unwrap_err()
        };
        assert!(broken(|f| f.verifier = WebhookVerifier::HmacSha256).contains("header"));
        assert!(broken(|f| f.dedupe = TextEdit::new("  ")).contains("Dedupe"));
        assert!(broken(|f| f.cost = TextEdit::new("0")).contains("cost"));
        assert!(broken(|f| f.time = TextEdit::new("soon")).contains("time"));
        assert!(broken(|f| f.per_hour = TextEdit::new("0")).contains("per hour"));
        assert!(broken(|f| f.retention = TextEdit::new("1.5")).contains("Retention"));
    }

    #[test]
    fn a_sample_is_a_json_body_with_name_value_header_lines() {
        let mut form = WebhookForm::new("triage");
        form.sample = TextEdit::new(r#"{"action":"opened"}"#);
        form.headers = TextEdit::new("X-GitHub-Event: issues\n\n  Content-Type:application/json ");
        let (body, sent) = form.sample().unwrap();
        assert_eq!(body["action"], "opened");
        assert_eq!(
            sent,
            BTreeMap::from([
                ("content-type".to_string(), "application/json".to_string()),
                ("x-github-event".to_string(), "issues".to_string()),
            ])
        );
        form.sample = TextEdit::new("{");
        assert!(form.sample().unwrap_err().contains("not JSON"));
        assert_eq!(
            headers("ok: 1\nbroken").unwrap_err(),
            "Header line 2 is not \"Name: value\""
        );
        assert!(headers(": nameless").is_err());
    }
}
