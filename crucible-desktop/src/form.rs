//! A launch form from a playbook's params schema: the fields it asks for, the params object the
//! answers make, and the spend and time ceilings a launch carries.
//!
//! The controller validates the params against the schema in full (patterns included); this
//! side catches what it can say before a round trip: a missing required field, a number that
//! is not one, a ceiling over the admin cap.

use std::collections::{BTreeMap, BTreeSet};

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
    pub offer: Offer,
}

/// What a field offers beyond its kind, from the schema's `x-` keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Offer {
    /// The bool field that must be on for this one to be sent (`x-enabled-by`).
    pub enabled_by: Option<String>,
    /// Choices from the latest snapshot of a task output (`x-choices-snapshot`).
    pub snapshot: Option<Lists>,
    /// The choices are suggestions and any value is accepted (`x-allow-other`).
    pub allow_other: bool,
    /// What the empty field shows for each value of a key field (`x-placeholder-snapshot`).
    pub placeholder: Option<Keyed<String>>,
    /// Several of the values may be picked, each launching its own run (`x-fan-out`).
    pub fan_out: bool,
    /// The fields whose answer keys this one's choices, placeholder or confirmation.
    pub keys: Vec<String>,
}

/// Choices a task output computed: one list, or one per value of a key field.
#[derive(Clone, Debug, PartialEq)]
pub enum Lists {
    One(Vec<String>),
    Keyed(Keyed<Vec<String>>),
}

/// Entries picked by the answer to the field named `by`.
#[derive(Clone, Debug, PartialEq)]
pub struct Keyed<T> {
    pub by: String,
    pub entries: BTreeMap<String, T>,
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
            let listed = prop
                .get("enum")
                .or_else(|| prop.get("x-choices"))
                .and_then(Value::as_array);
            let kind = match (listed, prop.get("type").and_then(Value::as_str)) {
                (_, Some("boolean")) => Kind::Bool,
                (Some(values), _) => Kind::Choice(values.iter().map(text_of).collect()),
                (None, Some("integer")) => Kind::Integer,
                (None, Some("number")) => Kind::Number,
                _ => Kind::Text,
            };
            let offer = offer(prop, props);
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
                        Kind::Choice(ref values) if !offer.fan_out => {
                            values.first().cloned().unwrap_or_default()
                        }
                        _ => String::new(),
                    }),
                kind,
                offer,
            })
        })
        .collect()
}

fn offer(prop: &Value, props: &Map<String, Value>) -> Offer {
    let enabled_by = prop
        .get("x-enabled-by")
        .and_then(Value::as_str)
        .filter(|gate| {
            props
                .get(*gate)
                .and_then(|g| g.get("type"))
                .and_then(Value::as_str)
                == Some("boolean")
        })
        .map(str::to_string);
    let snapshot = prop.get("x-choices-from").and_then(|from| {
        let value = prop.get("x-choices-snapshot")?.get("value")?;
        match keyed_by(from, props)? {
            Some(by) => Some(Lists::Keyed(Keyed {
                by,
                entries: entries(value, strings)?,
            })),
            None => strings(value).map(Lists::One),
        }
    });
    let placeholder = prop.get("x-placeholder-from").and_then(|from| {
        Some(Keyed {
            by: keyed_by(from, props)??,
            entries: entries(prop.get("x-placeholder-snapshot")?.get("value")?, |v| {
                v.as_str().map(str::to_string)
            })?,
        })
    });
    let keys = ["x-choices-from", "x-placeholder-from", "x-confirm-from"]
        .iter()
        .filter_map(|source| keyed_by(prop.get(*source)?, props)?)
        .collect();
    Offer {
        enabled_by,
        snapshot,
        allow_other: prop
            .get("x-allow-other")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        placeholder,
        fan_out: prop.get("x-fan-out").and_then(Value::as_bool) == Some(true),
        keys,
    }
}

/// A task-output source's key field: `Some(None)` when it has none, `None` when the source is
/// malformed or names a field that is not a declared string.
fn keyed_by(from: &Value, props: &Map<String, Value>) -> Option<Option<String>> {
    from.get("task")?.as_str()?;
    from.get("field")?.as_str()?;
    match from.get("keyed_by") {
        None | Some(Value::Null) => Some(None),
        Some(key) => {
            let key = key.as_str()?;
            let typed = props.get(key)?.get("type").and_then(Value::as_str);
            matches!(typed, None | Some("string")).then(|| Some(key.to_string()))
        }
    }
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn entries<T>(value: &Value, read: impl Fn(&Value) -> Option<T>) -> Option<BTreeMap<String, T>> {
    value
        .as_object()?
        .iter()
        .map(|(k, v)| Some((k.clone(), read(v)?)))
        .collect()
}

/// The trimmed answer to the field named `name`.
fn answer_to<'a>(fields: &[Field], answers: &'a [String], name: &str) -> &'a str {
    fields
        .iter()
        .position(|f| f.name == name)
        .and_then(|i| answers.get(i))
        .map_or("", |a| a.trim())
}

impl Field {
    /// Whether this field is sent: it has no gate, or its gate is on.
    pub fn enabled(&self, fields: &[Field], answers: &[String]) -> bool {
        self.offer
            .enabled_by
            .as_deref()
            .is_none_or(|gate| answer_to(fields, answers, gate).eq_ignore_ascii_case("true"))
    }

    /// The values to pick from now: the declared choices, else the snapshot's, narrowed to the
    /// key field's answer. `None` when the field is typed.
    pub fn offered(&self, fields: &[Field], answers: &[String]) -> Option<Vec<String>> {
        if let Kind::Choice(values) = &self.kind {
            return Some(values.clone());
        }
        match self.offer.snapshot.as_ref()? {
            Lists::One(values) => Some(values.clone()),
            Lists::Keyed(keyed) => Some(
                keyed
                    .entries
                    .get(answer_to(fields, answers, &keyed.by))
                    .cloned()
                    .unwrap_or_default(),
            ),
        }
    }

    /// What the empty field shows: the pack's pick for the key field's answer, else the default.
    pub fn placeholder(&self, fields: &[Field], answers: &[String]) -> String {
        self.offer
            .placeholder
            .as_ref()
            .and_then(|keyed| keyed.entries.get(answer_to(fields, answers, &keyed.by)))
            .unwrap_or(&self.default)
            .clone()
    }

    /// The key field whose answer picks this field's choices.
    fn keyed_by(&self) -> Option<&str> {
        match self.offer.snapshot.as_ref()? {
            Lists::Keyed(keyed) => Some(&keyed.by),
            Lists::One(_) => None,
        }
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// What is wrong with one field's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub field: usize,
    /// The picked value whose answer it is, for a field set per picked value.
    pub value: Option<String>,
    pub why: String,
}

/// The fan-out field, by index.
pub fn fan_out(fields: &[Field]) -> Option<usize> {
    fields.iter().position(|f| f.offer.fan_out)
}

/// The values picked for a fan-out field: its answer is a JSON array of them, or one plain value.
pub fn picked(answer: &str) -> Vec<String> {
    let text = answer.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if !text.starts_with('[') {
        return vec![text.to_string()];
    }
    serde_json::from_str::<Vec<Value>>(text)
        .map(|values| {
            values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A fan-out answer with `value` picked or unpicked, keeping the order picked.
pub fn with_pick(answer: &str, value: &str, on: bool) -> String {
    let mut picks: Vec<String> = picked(answer).into_iter().filter(|v| v != value).collect();
    if on {
        picks.push(value.to_string());
    }
    Value::from(picks).to_string()
}

/// How many runs the answers launch: one per picked value of a fan-out field, else one.
pub fn runs(fields: &[Field], answers: &[String]) -> usize {
    fan_out(fields)
        .and_then(|fan| answers.get(fan))
        .map_or(1, |answer| picked(answer).len().max(1))
}

/// The fields set per picked value of the fan-out field `fan`: those keyed by it, and their gates.
pub fn dependents(fields: &[Field], fan: usize) -> BTreeSet<usize> {
    let Some(name) = fields.get(fan).map(|f| f.name.as_str()) else {
        return BTreeSet::new();
    };
    let keyed: Vec<&Field> = fields
        .iter()
        .filter(|f| f.offer.keys.iter().any(|k| k == name))
        .collect();
    fields
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            keyed
                .iter()
                .any(|k| k.name == f.name || k.offer.enabled_by.as_deref() == Some(&f.name))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Each picked value's answers to the fan-out field's dependents, indexed like the fields.
pub type PerValue = BTreeMap<String, Vec<String>>;

/// The answers one run of a fan-out launch sees: `value` picked, and that value's answers to the
/// dependents.
pub fn run_answers(
    fields: &[Field],
    answers: &[String],
    per_value: &PerValue,
    fan: usize,
    value: &str,
) -> Vec<String> {
    let dependents = dependents(fields, fan);
    let own = per_value.get(value);
    answers
        .iter()
        .zip(fields)
        .enumerate()
        .map(|(i, (answer, field))| {
            if i == fan {
                value.to_string()
            } else if dependents.contains(&i) {
                own.and_then(|own| own.get(i))
                    .cloned()
                    .unwrap_or_else(|| field.default.clone())
            } else {
                answer.clone()
            }
        })
        .collect()
}

/// Splits prefilled answers into per-value ones: a dependent's JSON object from picked value to
/// answer goes to each value, and every picked value starts from the other prefilled answers.
pub fn spread(fields: &[Field], answers: &mut [String]) -> PerValue {
    let Some(fan) = fan_out(fields) else {
        return PerValue::new();
    };
    let picks = answers.get(fan).map(|a| picked(a)).unwrap_or_default();
    let mut per_value: PerValue = picks
        .iter()
        .map(|value| (value.clone(), answers.to_vec()))
        .collect();
    for i in dependents(fields, fan) {
        let (Some(answer), Some(field)) = (answers.get_mut(i), fields.get(i)) else {
            continue;
        };
        let Ok(Value::Object(by_value)) = serde_json::from_str(answer.trim()) else {
            continue;
        };
        for (value, own) in &mut per_value {
            if let Some(slot) = own.get_mut(i) {
                *slot = by_value
                    .get(value)
                    .map_or_else(|| field.default.clone(), text_of);
            }
        }
        *answer = field.default.clone();
    }
    per_value
}

/// The params object the answers make. An empty optional field is left out, so the schema's
/// own default applies. Picking several values of a fan-out field sends it as a JSON array and
/// each dependent as a JSON object from picked value to that run's answer.
pub fn params(
    fields: &[Field],
    answers: &[String],
    per_value: &PerValue,
) -> Result<Value, Vec<Problem>> {
    let Some(fan) = fan_out(fields) else {
        return one_run(fields, answers, &BTreeSet::new());
    };
    let dependents = dependents(fields, fan);
    let picks = answers.get(fan).map(|a| picked(a)).unwrap_or_default();
    if picks.is_empty() {
        return one_run(fields, answers, &dependents);
    }
    let mut out = Map::new();
    let mut by_value: BTreeMap<&str, Map<String, Value>> = BTreeMap::new();
    let mut problems: Vec<Problem> = Vec::new();
    for value in &picks {
        let run = run_answers(fields, answers, per_value, fan, value);
        let sent = match one_run(fields, &run, &BTreeSet::new()) {
            Ok(Value::Object(sent)) => sent,
            Ok(_) => Map::new(),
            Err(found) => {
                for mut problem in found {
                    if dependents.contains(&problem.field) {
                        problem.value = Some(value.clone());
                    }
                    if !problems.contains(&problem) {
                        problems.push(problem);
                    }
                }
                continue;
            }
        };
        for (i, field) in fields.iter().enumerate() {
            let Some(sent) = sent.get(&field.name) else {
                continue;
            };
            if dependents.contains(&i) {
                by_value
                    .entry(field.name.as_str())
                    .or_default()
                    .insert(value.clone(), sent.clone());
            } else if i != fan {
                out.insert(field.name.clone(), sent.clone());
            }
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let several = picks.len() > 1;
    for (name, sent) in by_value {
        let value = if several {
            let texts: Map<String, Value> = sent
                .into_iter()
                .map(|(k, v)| (k, Value::String(text_of(&v))))
                .collect();
            Value::String(Value::Object(texts).to_string())
        } else {
            sent.into_iter().next().map_or(Value::Null, |(_, v)| v)
        };
        out.insert(name.to_string(), value);
    }
    let fanned = match picks.as_slice() {
        [only] => only.clone(),
        _ => Value::from(picks.clone()).to_string(),
    };
    if let Some(field) = fields.get(fan) {
        out.insert(field.name.clone(), Value::String(fanned));
    }
    Ok(Value::Object(out))
}

/// The params one run's answers make, leaving out the fields in `skip`.
fn one_run(
    fields: &[Field],
    answers: &[String],
    skip: &BTreeSet<usize>,
) -> Result<Value, Vec<Problem>> {
    let mut out = Map::new();
    let mut problems = Vec::new();
    let mut refuse = |field: usize, why: String| {
        problems.push(Problem {
            field,
            value: None,
            why,
        })
    };
    for (i, field) in fields.iter().enumerate() {
        if skip.contains(&i) || !field.enabled(fields, answers) {
            continue;
        }
        let answer = answers.get(i).map(|a| a.trim()).unwrap_or_default();
        if answer.is_empty() {
            if field.required {
                refuse(
                    i,
                    if field.offer.fan_out {
                        "Pick at least one"
                    } else {
                        "Required"
                    }
                    .to_string(),
                );
            }
            continue;
        }
        if !field.offer.allow_other
            && let Some(values) = field.offered(fields, answers)
            && !values.iter().any(|v| v == answer)
        {
            refuse(
                i,
                match (values.is_empty(), field.keyed_by()) {
                    (true, Some(key)) => format!("No options for this {key}"),
                    (true, None) => "No options".to_string(),
                    (false, _) => format!("One of {}", values.join(", ")),
                },
            );
            continue;
        }
        let value = match &field.kind {
            Kind::Text | Kind::Choice(_) => Value::String(answer.to_string()),
            Kind::Integer => match answer.parse::<i64>() {
                Ok(n) => Value::from(n),
                Err(_) => {
                    refuse(i, "A whole number".to_string());
                    continue;
                }
            },
            Kind::Number => match answer.parse::<f64>().ok().filter(|n| n.is_finite()) {
                Some(n) => Value::from(n),
                None => {
                    refuse(i, "A number".to_string());
                    continue;
                }
            },
            Kind::Bool => Value::Bool(answer == "true"),
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal(pub String);

impl From<&str> for Refusal {
    fn from(error: &str) -> Self {
        let body = error
            .find('{')
            .and_then(|at| serde_json::from_str::<Value>(error[at..].trim()).ok());
        let Some(body) = body else {
            return Refusal(error.to_string());
        };
        let fields: Vec<&str> = body
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|f| f.get("message").and_then(Value::as_str))
            .collect();
        if !fields.is_empty() {
            return Refusal(fields.join("\n"));
        }
        Refusal(
            body.get("error")
                .and_then(Value::as_str)
                .unwrap_or(error)
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::form::{
        Field, Kind, PerValue, Problem, Refusal, Scale, ceilings, dependents, duration, fan_out,
        fields, minutes, params, picked, runs, spread, with_pick,
    };

    #[test]
    fn a_refusal_says_what_the_controller_said_about_each_field() {
        let raw = r#"/api/playbook-drafts/x/launch -> HTTP 422 Unprocessable Entity: {"error":"the supplied parameters do not satisfy the playbook's schema","fields":[{"field":"max_time","message":"task \"climb\" declares timeout 4h, longer than this run's --max-time 3h"}]}"#;
        let said = |raw: &str| Refusal::from(raw).0;
        assert_eq!(
            said(raw),
            "task \"climb\" declares timeout 4h, longer than this run's --max-time 3h"
        );
        assert_eq!(
            said(r#"/x -> HTTP 409 Conflict: {"error":"a newer save landed"}"#),
            "a newer save landed"
        );
        assert_eq!(
            said("the launch timed out after 20s"),
            "the launch timed out after 20s"
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
            params(&f, &answers, &PerValue::new()).unwrap(),
            json!({"repo": "vllm-project/speculators", "limit": "2", "deep": true, "mode": "thorough", "tries": 3})
        );
    }

    #[test]
    fn problems_name_each_field_that_cannot_be_sent() {
        let f = fields(&triage());
        let answers: Vec<String> = ["  ", "", "", "false", "slow", "three"]
            .map(String::from)
            .to_vec();
        let problems: Vec<(usize, String)> = params(&f, &answers, &PerValue::new())
            .unwrap_err()
            .into_iter()
            .map(|p| (p.field, p.why))
            .collect();
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

    fn release() -> serde_json::Value {
        json!({
            "properties": {
                "release": {
                    "type": "string",
                    "x-choices-from": {"task": "sheet", "field": "releases"},
                    "x-choices-snapshot": {"value": ["3.0", "3.1"], "captured_at": "2026-11-09T10:00:00Z"}
                },
                "component": {
                    "type": "string",
                    "x-choices-from": {"task": "sheet", "field": "components", "keyed_by": "release"},
                    "x-choices-snapshot": {"value": {"3.0": ["vllm"], "3.1": ["vllm", "llm-d"], "3.2": []}}
                },
                "branch": {
                    "type": "string",
                    "x-enabled-by": "override",
                    "x-placeholder-from": {"task": "sheet", "field": "branches", "keyed_by": "release"},
                    "x-placeholder-snapshot": {"value": {"3.1": "rhoai-3.1"}}
                },
                "override": {"type": "boolean"},
                "image": {"type": "string", "x-choices": ["ubi9", "fedora"], "x-allow-other": true},
                "stage": {"type": "string", "x-choices": ["dev", "prod"]},
                "fresh": {"type": "boolean", "enum": [true, false]},
                "unread": {
                    "type": "string",
                    "x-choices-from": {"task": "sheet", "field": "x"}
                }
            },
            "x-order": ["release", "component", "override", "branch", "image", "stage", "fresh", "unread"]
        })
    }

    fn answer(f: &[Field], pairs: &[(&str, &str)]) -> Vec<String> {
        f.iter()
            .map(|field| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == field.name)
                    .map_or(String::new(), |(_, v)| (*v).to_string())
            })
            .collect()
    }

    #[test]
    fn choices_come_from_x_choices_and_from_a_task_snapshot() {
        let f = fields(&release());
        let none = answer(&f, &[]);
        assert_eq!(
            f[0].offered(&f, &none),
            Some(vec!["3.0".into(), "3.1".into()])
        );
        assert_eq!(
            f[4].kind,
            Kind::Choice(vec!["ubi9".into(), "fedora".into()])
        );
        assert!(f[4].offer.allow_other && !f[5].offer.allow_other);
        assert_eq!(f[6].kind, Kind::Bool, "a bool with an enum stays a switch");
        assert_eq!(f[7].offered(&f, &none), None, "no snapshot yet: typed");
        assert_eq!(f[3].offered(&f, &none), None);
    }

    #[test]
    fn a_keyed_snapshot_offers_the_list_for_the_key_fields_answer() {
        let f = fields(&release());
        let on = |release: &str| f[1].offered(&f, &answer(&f, &[("release", release)]));
        assert_eq!(on("3.1"), Some(vec!["vllm".into(), "llm-d".into()]));
        assert_eq!(on(" 3.0 "), Some(vec!["vllm".into()]));
        assert_eq!(on("9.9"), Some(vec![]));
    }

    #[test]
    fn a_keyed_placeholder_follows_the_key_and_falls_back_to_the_default() {
        let f = fields(&release());
        assert_eq!(
            f[3].placeholder(&f, &answer(&f, &[("release", "3.1")])),
            "rhoai-3.1"
        );
        assert_eq!(f[3].placeholder(&f, &answer(&f, &[("release", "3.0")])), "");
    }

    #[test]
    fn a_gated_field_is_sent_only_while_its_switch_is_on() {
        let f = fields(&release());
        let base = [
            ("release", "3.1"),
            ("component", "vllm"),
            ("branch", "main"),
        ];
        let off = answer(&f, &[base.as_slice(), &[("override", "false")]].concat());
        assert!(!f[3].enabled(&f, &off));
        let sent = params(&f, &off, &PerValue::new()).unwrap();
        assert_eq!(sent.get("branch"), None);
        assert_eq!(sent["override"], json!(false));

        let on = answer(&f, &[base.as_slice(), &[("override", "true")]].concat());
        assert!(f[3].enabled(&f, &on));
        assert_eq!(
            params(&f, &on, &PerValue::new()).unwrap()["branch"],
            json!("main")
        );
        assert!(f[0].enabled(&f, &on), "an ungated field is always sent");
    }

    #[test]
    fn a_gate_that_is_not_a_declared_bool_gates_nothing() {
        let f = fields(&json!({
            "properties": {
                "a": {"type": "string", "x-enabled-by": "b"},
                "b": {"type": "string"}
            }
        }));
        assert_eq!(f[0].offer.enabled_by, None);
    }

    #[test]
    fn offered_choices_refuse_other_values_unless_other_is_allowed() {
        let f = fields(&release());
        let problems = |pairs: &[(&str, &str)]| -> Vec<(usize, String)> {
            params(&f, &answer(&f, pairs), &PerValue::new())
                .err()
                .unwrap_or_default()
                .into_iter()
                .map(|p| (p.field, p.why))
                .collect()
        };
        assert_eq!(
            problems(&[("release", "2.9"), ("stage", "qa")]),
            vec![
                (0, "One of 3.0, 3.1".to_string()),
                (5, "One of dev, prod".to_string()),
            ]
        );
        assert_eq!(
            problems(&[("release", "3.2"), ("component", "vllm")]),
            vec![
                (0, "One of 3.0, 3.1".to_string()),
                (1, "No options for this release".to_string()),
            ]
        );
        let sent = params(
            &f,
            &answer(
                &f,
                &[
                    ("release", "3.1"),
                    ("component", "llm-d"),
                    ("image", "alpine"),
                    ("unread", "anything"),
                ],
            ),
            &PerValue::new(),
        )
        .unwrap();
        assert_eq!(sent["image"], json!("alpine"));
        assert_eq!(sent["component"], json!("llm-d"));
        assert_eq!(sent["unread"], json!("anything"));
    }

    #[test]
    fn a_malformed_snapshot_or_source_offers_nothing() {
        let f = fields(&json!({
            "properties": {
                "a": {
                    "type": "string",
                    "x-choices-from": {"task": "t"},
                    "x-choices-snapshot": {"value": ["x"]}
                },
                "b": {
                    "type": "string",
                    "x-choices-from": {"task": "t", "field": "f"},
                    "x-choices-snapshot": {"value": [1, 2]}
                },
                "c": {
                    "type": "string",
                    "x-choices-from": {"task": "t", "field": "f", "keyed_by": "n"},
                    "x-choices-snapshot": {"value": {"k": ["x"]}}
                },
                "n": {"type": "integer"}
            }
        }));
        assert!(f.iter().all(|field| field.offer.snapshot.is_none()));
    }

    fn backport() -> serde_json::Value {
        json!({
            "properties": {
                "release": {
                    "type": "string",
                    "x-fan-out": true,
                    "x-choices-from": {"task": "sheet", "field": "releases"},
                    "x-choices-snapshot": {"value": ["3.7 Fast-1", "3.6", "3.5"]}
                },
                "component": {
                    "type": "string",
                    "x-choices-from": {"task": "sheet", "field": "components", "keyed_by": "release"},
                    "x-choices-snapshot": {"value": {
                        "3.7 Fast-1": ["vLLM", "vLLM [TPU]"], "3.6": ["vLLM"], "3.5": ["vLLM"]
                    }}
                },
                "override": {"type": "boolean"},
                "branch": {
                    "type": "string",
                    "x-enabled-by": "override",
                    "x-placeholder-from": {"task": "sheet", "field": "branches", "keyed_by": "release"},
                    "x-placeholder-snapshot": {"value": {"3.6": "rhai-3.6"}}
                },
                "tentative": {
                    "type": "boolean",
                    "x-confirm-from": {"task": "sheet", "field": "tentative", "keyed_by": "release"}
                },
                "context": {"type": "string"}
            },
            "required": ["release", "component"],
            "x-order": ["release", "component", "override", "branch", "tentative", "context"]
        })
    }

    fn per(f: &[Field], picks: &[(&str, &[(&str, &str)])]) -> PerValue {
        picks
            .iter()
            .map(|(value, pairs)| ((*value).to_string(), answer(f, pairs)))
            .collect()
    }

    #[test]
    fn a_fan_out_field_sets_the_fields_keyed_by_it_and_their_gates_per_value() {
        let f = fields(&backport());
        assert_eq!(fan_out(&f), Some(0));
        assert!(f[0].offer.fan_out && !f[1].offer.fan_out);
        assert_eq!(
            dependents(&f, 0).into_iter().collect::<Vec<_>>(),
            [1, 2, 3, 4],
            "component, override (the gate of branch), branch and tentative; not context"
        );
        assert_eq!(fan_out(&fields(&release())), None);
        let listed = fields(&json!({"properties": {
            "fan": {"type": "string", "x-fan-out": true, "x-choices": ["a", "b"]},
            "one": {"type": "string", "x-choices": ["a", "b"]}
        }}));
        assert_eq!(
            (listed[0].default.as_str(), listed[1].default.as_str()),
            ("", "a"),
            "a fan-out field starts with nothing picked"
        );
    }

    #[test]
    fn picks_toggle_in_the_order_picked() {
        assert_eq!(picked(""), Vec::<String>::new());
        assert_eq!(picked(" 3.6 "), ["3.6"]);
        assert_eq!(picked(r#"["3.6","3.5"]"#), ["3.6", "3.5"]);
        let two = with_pick(&with_pick("", "3.6", true), "3.5", true);
        assert_eq!(two, r#"["3.6","3.5"]"#);
        assert_eq!(picked(&with_pick(&two, "3.6", false)), ["3.5"]);
        assert_eq!(picked(&with_pick(&two, "3.5", true)), ["3.6", "3.5"]);
        let f = fields(&backport());
        assert_eq!(runs(&f, &answer(&f, &[])), 1);
        assert_eq!(runs(&f, &answer(&f, &[("release", &two)])), 2);
    }

    #[test]
    fn a_required_fan_out_field_with_nothing_picked_asks_for_one_pick() {
        let f = fields(&backport());
        assert_eq!(
            params(&f, &answer(&f, &[("context", "x")]), &PerValue::new()).unwrap_err(),
            vec![Problem {
                field: 0,
                value: None,
                why: "Pick at least one".to_string()
            }]
        );
    }

    #[test]
    fn one_pick_sends_plain_values() {
        let f = fields(&backport());
        let answers = answer(&f, &[("release", r#"["3.6"]"#), ("context", "why")]);
        let per_value = per(
            &f,
            &[(
                "3.6",
                &[("component", "vLLM"), ("override", "true"), ("branch", "b")],
            )],
        );
        assert_eq!(
            params(&f, &answers, &per_value).unwrap(),
            json!({"release": "3.6", "component": "vLLM", "override": true, "branch": "b",
                   "context": "why"})
        );
    }

    #[test]
    fn several_picks_send_an_array_and_each_dependent_by_picked_value() {
        let f = fields(&backport());
        let answers = answer(
            &f,
            &[("release", r#"["3.7 Fast-1","3.6"]"#), ("context", "why")],
        );
        let per_value = per(
            &f,
            &[
                (
                    "3.7 Fast-1",
                    &[
                        ("component", "vLLM [TPU]"),
                        ("override", "false"),
                        ("branch", "ignored while off"),
                    ],
                ),
                (
                    "3.6",
                    &[("component", "vLLM"), ("override", "true"), ("branch", "b")],
                ),
            ],
        );
        let sent = params(&f, &answers, &per_value).unwrap();
        let object = |name: &str| -> serde_json::Value {
            serde_json::from_str(sent[name].as_str().unwrap()).unwrap()
        };
        assert_eq!(sent["release"], json!(r#"["3.7 Fast-1","3.6"]"#));
        assert_eq!(
            object("component"),
            json!({"3.7 Fast-1": "vLLM [TPU]", "3.6": "vLLM"})
        );
        assert_eq!(
            object("override"),
            json!({"3.7 Fast-1": "false", "3.6": "true"})
        );
        assert_eq!(object("branch"), json!({"3.6": "b"}));
        assert_eq!(sent["context"], json!("why"));
    }

    #[test]
    fn a_dependents_problem_names_the_picked_value_it_belongs_to() {
        let f = fields(&backport());
        let answers = answer(&f, &[("release", r#"["3.7 Fast-1","3.6"]"#)]);
        let per_value = per(
            &f,
            &[
                ("3.7 Fast-1", &[("component", "vLLM [TPU]")]),
                ("3.6", &[("component", "vLLM [TPU]")]),
            ],
        );
        assert_eq!(
            params(&f, &answers, &per_value).unwrap_err(),
            vec![Problem {
                field: 1,
                value: Some("3.6".to_string()),
                why: "One of vLLM".to_string()
            }]
        );
    }

    #[test]
    fn prefilled_objects_spread_to_each_picked_value() {
        let f = fields(&backport());
        let mut answers = answer(
            &f,
            &[
                ("release", r#"["3.7 Fast-1","3.6"]"#),
                ("component", r#"{"3.6":"vLLM"}"#),
                ("override", "true"),
            ],
        );
        let per_value = spread(&f, &mut answers);
        assert_eq!(answers[1], "", "the object is not left as a typed answer");
        assert_eq!(per_value["3.6"][1], "vLLM");
        assert_eq!(per_value["3.7 Fast-1"][1], "", "an absent value is unset");
        assert_eq!(
            per_value["3.6"][2], "true",
            "a plain answer applies to each"
        );
    }
}
