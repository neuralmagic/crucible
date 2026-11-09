//! A launch form from a playbook's params schema: the fields it asks for, the params object the
//! answers make, and the spend and time ceilings a launch carries.
//!
//! The controller validates the params against the schema in full (patterns included); this
//! side catches what it can say before a round trip: a missing required field, a number that
//! is not one, a ceiling over the admin cap.

use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Text,
    Integer,
    Number,
    Bool,
    /// One of these values.
    Choice(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    pub required: bool,
    pub description: String,
    /// The schema's default, as the text the field starts with.
    pub default: String,
}

/// The fields a schema asks for: its `x-order` first, then the rest by name.
pub fn fields(schema: &Value) -> Vec<Field> {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut order: Vec<&str> = schema
        .get("x-order")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .filter(|name| props.contains_key(*name))
                .collect()
        })
        .unwrap_or_default();
    let mut rest: Vec<&str> = props
        .keys()
        .map(String::as_str)
        .filter(|name| !order.contains(name))
        .collect();
    rest.sort_unstable();
    order.extend(rest);

    order
        .into_iter()
        .filter_map(|name| {
            let prop = props.get(name)?;
            let kind = match (
                prop.get("enum").and_then(Value::as_array),
                prop.get("type").and_then(Value::as_str),
            ) {
                (Some(values), _) => Kind::Choice(values.iter().map(text_of).collect()),
                (None, Some("integer")) => Kind::Integer,
                (None, Some("number")) => Kind::Number,
                (None, Some("boolean")) => Kind::Bool,
                _ => Kind::Text,
            };
            Some(Field {
                name: name.to_string(),
                required: required.contains(&name),
                description: prop
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                default: prop
                    .get("default")
                    .map(text_of)
                    .unwrap_or_else(|| match kind {
                        Kind::Bool => "false".to_string(),
                        Kind::Choice(ref values) => values.first().cloned().unwrap_or_default(),
                        _ => String::new(),
                    }),
                kind,
            })
        })
        .collect()
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// What is wrong with one field's answer, by field index.
pub type Problems = Vec<(usize, String)>;

/// The params object the answers make. An empty optional field is left out, so the schema's
/// own default applies.
pub fn params(fields: &[Field], answers: &[String]) -> Result<Value, Problems> {
    let mut out = Map::new();
    let mut problems = Vec::new();
    for (i, field) in fields.iter().enumerate() {
        let answer = answers.get(i).map(|a| a.trim()).unwrap_or_default();
        if answer.is_empty() {
            if field.required {
                problems.push((i, "Required".to_string()));
            }
            continue;
        }
        let value = match &field.kind {
            Kind::Text => Value::String(answer.to_string()),
            Kind::Integer => match answer.parse::<i64>() {
                Ok(n) => Value::from(n),
                Err(_) => {
                    problems.push((i, "A whole number".to_string()));
                    continue;
                }
            },
            Kind::Number => match answer.parse::<f64>().ok().filter(|n| n.is_finite()) {
                Some(n) => Value::from(n),
                None => {
                    problems.push((i, "A number".to_string()));
                    continue;
                }
            },
            Kind::Bool => Value::Bool(answer == "true"),
            Kind::Choice(values) => {
                if !values.iter().any(|v| v == answer) {
                    problems.push((i, format!("One of {}", values.join(", "))));
                    continue;
                }
                Value::String(answer.to_string())
            }
        };
        out.insert(field.name.clone(), value);
    }
    if problems.is_empty() {
        Ok(Value::Object(out))
    } else {
        Err(problems)
    }
}

/// Minutes in a duration written as `45m`, `2h` or `1h30m`.
pub fn minutes(duration: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut digits = String::new();
    let mut any = false;
    for c in duration.trim().chars() {
        match c {
            '0'..='9' => digits.push(c),
            'h' | 'm' => {
                let n: u64 = digits.parse().ok()?;
                total += if c == 'h' { n * 60 } else { n };
                digits.clear();
                any = true;
            }
            _ => return None,
        }
    }
    (any && digits.is_empty() && total > 0).then_some(total)
}

/// `90` minutes as `1h30m`, `120` as `2h`, `45` as `45m`.
pub fn duration(minutes: u64) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m}m"),
    }
}

/// A slider's range: whole steps of `step` up to `max`, never below one step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    pub step: f64,
    pub max: f64,
}

impl Scale {
    /// Dollars up to `cap`, in steps that keep the slider to a few hundred stops.
    pub fn dollars(cap: f64) -> Scale {
        let step = match cap {
            c if c <= 10.0 => 0.25,
            c if c <= 100.0 => 1.0,
            _ => 5.0,
        };
        Scale { step, max: cap }
    }

    /// Minutes up to `cap`.
    pub fn minutes(cap: u64) -> Scale {
        let step = match cap {
            0..=60 => 5,
            61..=360 => 15,
            _ => 30,
        };
        Scale {
            step: step as f64,
            max: cap as f64,
        }
    }

    /// The value at `frac` of the way along, on a step.
    pub fn at(self, frac: f32) -> f64 {
        let raw = f64::from(frac.clamp(0.0, 1.0)) * self.max;
        let stepped = (raw / self.step).round() * self.step;
        stepped.clamp(self.step.min(self.max), self.max)
    }

    /// How far along `value` sits.
    pub fn frac(self, value: f64) -> f32 {
        if self.max <= 0.0 {
            return 0.0;
        }
        (value / self.max).clamp(0.0, 1.0) as f32
    }

    /// Stops between the ends, for the arrow keys.
    pub fn steps(self) -> u32 {
        (self.max / self.step).round().max(1.0) as u32
    }
}

/// The ceilings a launch asks for, checked against the admin caps.
pub fn ceilings(
    cost: &str,
    time: &str,
    max_cost: f64,
    max_time: &str,
) -> Result<(f64, String), String> {
    let cost: f64 = cost
        .trim()
        .trim_start_matches('$')
        .parse()
        .ok()
        .filter(|c: &f64| c.is_finite() && *c > 0.0)
        .ok_or("The cost ceiling is a dollar amount above zero")?;
    if cost > max_cost {
        return Err(format!("The cost ceiling is at most ${max_cost:.2}"));
    }
    let asked = minutes(time).ok_or("The time ceiling reads like 30m or 2h")?;
    if let Some(cap) = minutes(max_time)
        && asked > cap
    {
        return Err(format!("The time ceiling is at most {max_time}"));
    }
    Ok((cost, time.trim().to_string()))
}

/// What the controller said when it refused a launch: each field's message, else its `error`,
/// else the error as it came.
pub fn refusal(error: &str) -> String {
    let body = error
        .find('{')
        .and_then(|at| serde_json::from_str::<Value>(error[at..].trim()).ok());
    let Some(body) = body else {
        return error.to_string();
    };
    let fields: Vec<String> = body
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| f.get("message").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if !fields.is_empty() {
        return fields.join("\n");
    }
    body.get("error")
        .and_then(Value::as_str)
        .map_or_else(|| error.to_string(), str::to_string)
}

/// The dispatch targets a launch can pick, as (name, label), default first.
pub fn targets(list: &crux::dto::DispatchTargets) -> Vec<(String, String)> {
    let mut targets: Vec<&crux::dto::DispatchTarget> = list.targets.iter().collect();
    targets.sort_by_key(|t| !t.default);
    targets
        .into_iter()
        .map(|t| {
            let label = if t.label.is_empty() {
                &t.name
            } else {
                &t.label
            };
            (t.name.clone(), label.clone())
        })
        .collect()
}

/// The providers an agent can run against, as (id, label), after `Default`, which leaves the
/// controller's dispatch default to pick.
pub fn providers(list: &[crux::dto::Provider]) -> Vec<(String, String)> {
    let mut out = vec![(String::new(), "Default".to_string())];
    out.extend(
        list.iter()
            .filter(|p| p.enabled && (p.roles.is_empty() || p.roles.iter().any(|r| r == "agent")))
            .map(|p| {
                let label = p.display_name.clone().filter(|n| !n.is_empty());
                (p.id.clone(), label.unwrap_or_else(|| p.id.clone()))
            }),
    );
    out
}

/// The key of the newest of `launches` that dispatched a run.
pub fn newest_dispatched(launches: &[crux::dto::PlaybookRun]) -> Option<String> {
    launches
        .iter()
        .filter(|l| l.runs > 0)
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
        .map(|l| l.key.clone())
}

/// The target a form starts on: the one asked for before when it is still offered, else the
/// default.
pub fn starting_target(before: Option<&str>, list: &crux::dto::DispatchTargets) -> Option<String> {
    before
        .filter(|name| list.targets.iter().any(|t| t.name == *name))
        .map(str::to_string)
        .or_else(|| {
            list.targets
                .iter()
                .find(|t| t.default)
                .map(|t| t.name.clone())
        })
}

/// Where a launch runs and which agent drives it. `None`, and an empty model, leave the
/// controller to resolve its default.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunsOn {
    pub target: Option<String>,
    pub provider: Option<String>,
    pub model: String,
}

impl RunsOn {
    /// Where `launch` ran and what drove its agent, or `None` when it never dispatched a run.
    pub fn of(launch: &crux::dto::PlaybookLaunch) -> Option<RunsOn> {
        let run = launch.runs.first()?;
        Some(RunsOn {
            target: Some(run.cluster.clone()).filter(|c| !c.is_empty()),
            provider: launch.launch.agent_provider.clone(),
            model: launch.launch.agent_model.clone().unwrap_or_default(),
        })
    }

    /// Sets the launch body's `dispatch_target`, `provider` and `model`, each as null when it
    /// is left to the default, so a schedule saved over a stored one drops a pin cleared here.
    pub fn apply(&self, body: &mut Value) {
        let model = self.model.trim();
        body["dispatch_target"] = self.target.clone().map_or(Value::Null, Value::String);
        body["provider"] = self
            .provider
            .clone()
            .filter(|p| !p.is_empty())
            .map_or(Value::Null, Value::String);
        body["model"] = if model.is_empty() {
            Value::Null
        } else {
            Value::String(model.to_string())
        };
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::form::{
        Kind, RunsOn, Scale, ceilings, duration, fields, minutes, newest_dispatched, params,
        providers, refusal, starting_target, targets,
    };

    #[test]
    fn a_refusal_says_what_the_controller_said_about_each_field() {
        let raw = r#"/api/playbook-drafts/x/launch -> HTTP 422 Unprocessable Entity: {"error":"the supplied parameters do not satisfy the playbook's schema","fields":[{"field":"max_time","message":"task \"climb\" declares timeout 4h, longer than this run's --max-time 3h"}]}"#;
        assert_eq!(
            refusal(raw),
            "task \"climb\" declares timeout 4h, longer than this run's --max-time 3h"
        );
        assert_eq!(
            refusal(r#"/x -> HTTP 409 Conflict: {"error":"a newer save landed"}"#),
            "a newer save landed"
        );
        assert_eq!(
            refusal("the launch timed out after 20s"),
            "the launch timed out after 20s"
        );
    }

    #[test]
    fn a_drop_follows_the_newest_launch_that_dispatched_a_run() {
        let launches: Vec<crux::dto::PlaybookRun> = serde_json::from_value(json!([
            {"key": "old", "runs": 1, "created_at": "2026-11-08T10:00:00Z"},
            {"key": "parked", "runs": 0, "created_at": "2026-11-09T10:00:00Z"},
            {"key": "newer", "runs": 2, "created_at": "2026-11-09T09:00:00Z"}
        ]))
        .unwrap();
        assert_eq!(newest_dispatched(&launches).as_deref(), Some("newer"));
        assert_eq!(newest_dispatched(&launches[1..2]), None);
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
            RunsOn::of(&launch),
            Some(RunsOn {
                target: Some("pirate".into()),
                provider: Some("enmaas".into()),
                model: "rits/deepseek-ai/DeepSeek-V4.1-Flash-1017".into(),
            })
        );
        let never: crux::dto::PlaybookLaunch = serde_json::from_value(json!({
            "launch": {"params": {}}, "dispatch": {"state": "pending"}, "runs": []
        }))
        .unwrap();
        assert_eq!(RunsOn::of(&never), None);
    }

    fn dispatch_targets() -> crux::dto::DispatchTargets {
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

    #[test]
    fn targets_list_the_default_first_by_label() {
        assert_eq!(
            targets(&dispatch_targets()),
            [
                ("waldorf".to_string(), "waldorf".to_string()),
                ("pirate".to_string(), "pirate".to_string()),
                ("personal:01a0".to_string(), "pirate-dispatch".to_string()),
            ]
        );
    }

    #[test]
    fn a_form_starts_on_the_target_asked_before_while_it_is_offered() {
        let list = dispatch_targets();
        assert_eq!(
            starting_target(Some("pirate"), &list).as_deref(),
            Some("pirate")
        );
        assert_eq!(
            starting_target(Some("gone"), &list).as_deref(),
            Some("waldorf"),
            "a target no longer offered falls back to the default"
        );
        assert_eq!(starting_target(None, &list).as_deref(), Some("waldorf"));
        assert_eq!(starting_target(Some("pirate"), &Default::default()), None);
    }

    #[test]
    fn providers_offer_default_then_the_enabled_agent_ones() {
        let list: Vec<crux::dto::Provider> = serde_json::from_value(json!([
            {"id": "enmaas", "display_name": "EnMaaS (Claude)", "enabled": true,
             "roles": ["agent"], "default_model": "claude-opus-5-5"},
            {"id": "off", "enabled": false, "roles": ["agent"]},
            {"id": "judge-only", "enabled": true, "roles": ["judge"]},
            {"id": "openai", "enabled": true}
        ]))
        .unwrap();
        assert_eq!(
            providers(&list),
            [
                (String::new(), "Default".to_string()),
                ("enmaas".to_string(), "EnMaaS (Claude)".to_string()),
                ("openai".to_string(), "openai".to_string()),
            ]
        );
    }

    #[test]
    fn runs_on_pins_what_was_picked_and_nulls_what_was_left_to_the_default() {
        let mut body = json!({"params": {}, "max_cost": 1.0});
        RunsOn {
            target: Some("pirate".into()),
            provider: Some("enmaas".into()),
            model: "  rits/deepseek-ai/DeepSeek-V4.1-Flash-1017 ".into(),
        }
        .apply(&mut body);
        assert_eq!(body["dispatch_target"], "pirate");
        assert_eq!(body["provider"], "enmaas");
        assert_eq!(body["model"], "rits/deepseek-ai/DeepSeek-V4.1-Flash-1017");

        let mut stored = json!({"dispatch_target": "pirate", "provider": "enmaas", "model": "m"});
        RunsOn {
            target: None,
            provider: Some(String::new()),
            model: " ".into(),
        }
        .apply(&mut stored);
        assert_eq!(
            stored,
            json!({"dispatch_target": null, "provider": null, "model": null}),
            "a cleared pin replaces the stored one"
        );
    }

    fn triage() -> serde_json::Value {
        json!({
            "properties": {
                "label": {"default": "", "description": "only open issues carrying this label", "type": "string"},
                "limit": {"default": "6", "type": "string"},
                "repo": {"description": "owner/name", "pattern": "^x$", "type": "string"},
                "deep": {"type": "boolean"},
                "tries": {"type": "integer", "default": 2},
                "mode": {"enum": ["fast", "thorough"]}
            },
            "required": ["repo"],
            "x-order": ["repo", "label", "limit", "ghost"]
        })
    }

    #[test]
    fn fields_follow_x_order_then_name_with_kinds_and_defaults() {
        let f = fields(&triage());
        let names: Vec<&str> = f.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["repo", "label", "limit", "deep", "mode", "tries"]);
        assert!(f[0].required && !f[1].required);
        assert_eq!(f[0].description, "owner/name");
        assert_eq!(f[2].default, "6");
        assert_eq!(
            (f[3].kind.clone(), f[3].default.as_str()),
            (Kind::Bool, "false")
        );
        assert_eq!(
            f[4].kind,
            Kind::Choice(vec!["fast".into(), "thorough".into()])
        );
        assert_eq!(f[4].default, "fast");
        assert_eq!(
            (f[5].kind.clone(), f[5].default.as_str()),
            (Kind::Integer, "2")
        );
        assert!(fields(&json!({})).is_empty());
    }

    #[test]
    fn answers_become_typed_params_and_empty_optionals_are_left_out() {
        let f = fields(&triage());
        let answers: Vec<String> = ["vllm-project/speculators", "", "2", "true", "thorough", "3"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            params(&f, &answers).unwrap(),
            json!({"repo": "vllm-project/speculators", "limit": "2", "deep": true, "mode": "thorough", "tries": 3})
        );
    }

    #[test]
    fn problems_name_each_field_that_cannot_be_sent() {
        let f = fields(&triage());
        let answers: Vec<String> = ["  ", "", "", "false", "slow", "three"]
            .map(String::from)
            .to_vec();
        let problems = params(&f, &answers).unwrap_err();
        assert_eq!(
            problems,
            vec![
                (0, "Required".to_string()),
                (4, "One of fast, thorough".to_string()),
                (5, "A whole number".to_string()),
            ]
        );
    }

    #[test]
    fn durations_read_in_hours_and_minutes() {
        assert_eq!(minutes("45m"), Some(45));
        assert_eq!(minutes("2h"), Some(120));
        assert_eq!(minutes("1h30m"), Some(90));
        assert_eq!(minutes(" 3h "), Some(180));
        for bad in ["", "30", "m", "0m", "1d", "1.5h", "h30"] {
            assert_eq!(minutes(bad), None, "{bad}");
        }
    }

    #[test]
    fn durations_write_back_the_way_they_read() {
        for m in [5, 45, 60, 90, 120, 185] {
            assert_eq!(minutes(&duration(m)), Some(m), "{m}");
        }
        assert_eq!(duration(90), "1h30m");
        assert_eq!(duration(120), "2h");
        assert_eq!(duration(45), "45m");
    }

    #[test]
    fn a_dollar_scale_lands_on_steps_inside_its_cap() {
        let small = Scale::dollars(5.0);
        assert_eq!(small.at(0.0), 0.25, "never below one step");
        assert_eq!(small.at(0.5), 2.5);
        assert_eq!(small.at(0.51), 2.5);
        assert_eq!(small.at(1.0), 5.0);
        assert_eq!(small.at(7.0), 5.0, "past the end clamps");
        assert_eq!(small.steps(), 20);
        let big = Scale::dollars(50.0);
        assert_eq!(big.at(0.333), 17.0);
        assert_eq!(big.frac(25.0), 0.5);
        assert_eq!(big.frac(80.0), 1.0);
        let tiny = Scale::dollars(0.1);
        assert_eq!(tiny.at(0.0), 0.1, "a cap under one step is the only stop");
    }

    #[test]
    fn a_minute_scale_steps_by_the_size_of_its_cap() {
        let hours = Scale::minutes(180);
        assert_eq!(hours.step, 15.0);
        assert_eq!(hours.at(0.5), 90.0);
        assert_eq!(hours.at(0.0), 15.0);
        assert_eq!(Scale::minutes(30).at(0.5), 15.0);
        assert_eq!(Scale::minutes(600).step, 30.0);
        assert_eq!(Scale::dollars(0.0).frac(1.0), 0.0);
    }

    #[test]
    fn ceilings_stay_under_the_admin_caps() {
        assert_eq!(ceilings("$3", "20m", 50.0, "3h"), Ok((3.0, "20m".into())));
        assert_eq!(ceilings("50", "3h", 50.0, "3h"), Ok((50.0, "3h".into())));
        assert!(
            ceilings("51", "20m", 50.0, "3h")
                .unwrap_err()
                .contains("$50.00")
        );
        assert!(ceilings("3", "4h", 50.0, "3h").unwrap_err().contains("3h"));
        assert!(ceilings("0", "20m", 50.0, "3h").is_err());
        assert!(ceilings("lots", "20m", 50.0, "3h").is_err());
        assert!(ceilings("3", "soon", 50.0, "3h").is_err());
    }
}
