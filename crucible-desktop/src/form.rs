//! A launch form from a playbook's params schema: the fields it asks for, the params object the
//! answers make, and the spend and time ceilings a launch carries.
//!
//! The controller validates the params against the schema in full (patterns included); this
//! side catches what it can say before a round trip: a missing required field, a number that
//! is not one, a ceiling over the admin cap.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crucible_contract::calls::{MAX_FILE_PARAM_BYTES, MAX_JSON_PARAM_BYTES};
use crucible_contract::emits::JsonSchema;
use rune::TextEdit;
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Text,
    Multiline,
    Integer,
    Number,
    Bool,
    /// One of these values.
    Choice(Vec<String>),
    /// Any of these values, sent as a list.
    MultiChoice(Vec<String>),
    Json(Option<JsonSchema>),
    File(FileSpec),
}

/// What a file parameter admits.
#[derive(Clone, Debug, PartialEq)]
pub struct FileSpec {
    pub max_bytes: u64,
    pub media_types: Vec<String>,
}

/// A file parameter's answer: the local file to upload as parameter `param`'s value.
#[derive(Clone, Debug, PartialEq)]
pub struct Upload {
    pub param: String,
    pub path: PathBuf,
    pub max_bytes: u64,
    pub media_type: Option<String>,
}

/// One field's answer, in the shape its kind takes.
#[derive(Clone, PartialEq)]
pub enum Answer {
    Text(Box<TextEdit>),
    Flag(bool),
    /// The picked choices, by their place in the field's list.
    MultiChoice(BTreeSet<usize>),
}

impl Answer {
    /// A typed answer holding `text`.
    pub fn typed(text: impl Into<String>) -> Answer {
        Answer::Text(Box::new(TextEdit::new(text)))
    }

    /// The trimmed text of a typed answer; empty for a switch or picks.
    pub fn text(&self) -> &str {
        match self {
            Answer::Text(edit) => edit.text.trim(),
            Answer::Flag(_) | Answer::MultiChoice(_) => "",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    pub required: bool,
    pub description: String,
    /// The schema's default, else what its kind starts with.
    pub default: Value,
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
            let picks = prop
                .get("items")
                .and_then(|items| items.get("enum"))
                .and_then(strings);
            let kind = match (listed, prop.get("type").and_then(Value::as_str)) {
                _ if param_type(prop) == Some("json") => Kind::Json(
                    prop.get("allOf")
                        .and_then(Value::as_array)
                        .and_then(|all| all.first())
                        .and_then(|schema| JsonSchema::new(schema.clone()).ok()),
                ),
                _ if param_type(prop) == Some("file") => Kind::File(FileSpec {
                    max_bytes: prop
                        .get("x-max-bytes")
                        .and_then(Value::as_u64)
                        .unwrap_or(MAX_FILE_PARAM_BYTES)
                        .min(MAX_FILE_PARAM_BYTES),
                    media_types: prop
                        .get("x-media-type")
                        .and_then(strings)
                        .unwrap_or_default(),
                }),
                (_, Some("boolean")) => Kind::Bool,
                (_, Some("array")) if picks.is_some() => {
                    Kind::MultiChoice(picks.unwrap_or_default())
                }
                (Some(values), _) => Kind::Choice(values.iter().map(text_of).collect()),
                (None, Some("integer")) => Kind::Integer,
                (None, Some("number")) => Kind::Number,
                (None, Some("string"))
                    if prop.get("x-multiline").and_then(Value::as_bool) == Some(true) =>
                {
                    Kind::Multiline
                }
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
                default: prop.get("default").cloned().unwrap_or_else(|| match kind {
                    Kind::Bool => Value::Bool(false),
                    Kind::Choice(ref values) => Value::from(values.first().cloned()),
                    Kind::MultiChoice(_) => Value::Array(Vec::new()),
                    _ => Value::Null,
                }),
                kind,
                offer: offer(prop, props),
            })
        })
        .collect()
}

fn param_type(prop: &Value) -> Option<&str> {
    prop.get("x-param-type").and_then(Value::as_str)
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
    Offer {
        enabled_by,
        snapshot,
        allow_other: prop
            .get("x-allow-other")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        placeholder,
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

/// The answer to the field named `name`.
fn answer_to<'a>(fields: &[Field], answers: &'a [Answer], name: &str) -> Option<&'a Answer> {
    fields
        .iter()
        .position(|f| f.name == name)
        .and_then(|i| answers.get(i))
}

impl Field {
    /// Whether this field is sent: it has no gate, or its gate is on.
    pub fn enabled(&self, fields: &[Field], answers: &[Answer]) -> bool {
        self.offer
            .enabled_by
            .as_deref()
            .is_none_or(|gate| matches!(answer_to(fields, answers, gate), Some(Answer::Flag(true))))
    }

    /// The values to pick from now: the declared choices, else the snapshot's, narrowed to the
    /// key field's answer. `None` when the field is typed.
    pub fn offered(&self, fields: &[Field], answers: &[Answer]) -> Option<Vec<String>> {
        if let Kind::Choice(values) = &self.kind {
            return Some(values.clone());
        }
        match self.offer.snapshot.as_ref()? {
            Lists::One(values) => Some(values.clone()),
            Lists::Keyed(keyed) => Some(
                keyed
                    .entries
                    .get(answer_to(fields, answers, &keyed.by).map_or("", Answer::text))
                    .cloned()
                    .unwrap_or_default(),
            ),
        }
    }

    /// What the empty field shows: the pack's pick for the key field's answer, else the default.
    pub fn placeholder(&self, fields: &[Field], answers: &[Answer]) -> String {
        self.offer
            .placeholder
            .as_ref()
            .and_then(|keyed| {
                keyed
                    .entries
                    .get(answer_to(fields, answers, &keyed.by).map_or("", Answer::text))
            })
            .cloned()
            .unwrap_or_else(|| self.default_text())
    }

    /// The default as the text a typed field holds.
    pub fn default_text(&self) -> String {
        text_of(&self.default)
    }

    /// The answer this field starts with: what `previous` sent for it, else its default.
    pub fn answer(&self, previous: Option<&Value>) -> Answer {
        let from = previous.unwrap_or(&self.default);
        match &self.kind {
            Kind::Bool => Answer::Flag(from.as_bool().unwrap_or(false)),
            Kind::MultiChoice(choices) => Answer::MultiChoice(
                from.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| {
                        choices
                            .iter()
                            .position(|c| value.as_str() == Some(c.as_str()))
                    })
                    .collect(),
            ),
            Kind::Text
            | Kind::Multiline
            | Kind::Integer
            | Kind::Number
            | Kind::Choice(_)
            | Kind::Json(_)
            | Kind::File(_) => Answer::typed(text_of(from)),
        }
    }

    /// The param `answer` makes, `None` when an optional field is left empty; else what is
    /// wrong with it.
    fn param(
        &self,
        fields: &[Field],
        answers: &[Answer],
        answer: Option<&Answer>,
    ) -> Result<Option<Value>, String> {
        let empty = || {
            if self.required {
                Err("Required".to_string())
            } else {
                Ok(None)
            }
        };
        match (&self.kind, answer) {
            (Kind::Bool, Some(Answer::Flag(on))) => Ok(Some(Value::Bool(*on))),
            (Kind::MultiChoice(_), Some(Answer::MultiChoice(held)))
                if self.required && held.is_empty() =>
            {
                Err("Pick at least one".to_string())
            }
            (Kind::MultiChoice(choices), Some(Answer::MultiChoice(held))) => Ok(Some(Value::from(
                held.iter()
                    .filter_map(|at| choices.get(*at).cloned())
                    .collect::<Vec<_>>(),
            ))),
            (Kind::File(_), _) => Ok(None),
            (Kind::Json(schema), Some(Answer::Text(edit))) => match edit.text.trim() {
                "" => empty(),
                text => json_value(text, schema.as_ref()).map(Some),
            },
            (
                Kind::Text | Kind::Multiline | Kind::Integer | Kind::Number | Kind::Choice(_),
                Some(Answer::Text(edit)),
            ) => {
                let text = edit.text.trim();
                if text.is_empty() {
                    return empty();
                }
                if !self.offer.allow_other
                    && let Some(values) = self.offered(fields, answers)
                    && !values.iter().any(|v| v == text)
                {
                    return Err(match (values.is_empty(), self.keyed_by()) {
                        (true, Some(key)) => format!("No options for this {key}"),
                        (true, None) => "No options".to_string(),
                        (false, _) => format!("One of {}", values.join(", ")),
                    });
                }
                match &self.kind {
                    Kind::Integer => text
                        .parse::<i64>()
                        .map(Value::from)
                        .map_err(|_| "A whole number".to_string()),
                    Kind::Number => text
                        .parse::<f64>()
                        .ok()
                        .filter(|n| n.is_finite())
                        .map(Value::from)
                        .ok_or_else(|| "A number".to_string()),
                    _ => Ok(Value::String(text.to_string())),
                }
                .map(Some)
            }
            _ => empty(),
        }
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

/// What is wrong with one field's answer, by field index.
pub type Problems = Vec<(usize, String)>;

/// The params object the answers make. An empty optional field is left out, so the schema's
/// own default applies. A file field is left out too: [`uploads`] reads it.
pub fn params(fields: &[Field], answers: &[Answer]) -> Result<Value, Problems> {
    let mut out = Map::new();
    let mut problems = Vec::new();
    for (i, field) in fields.iter().enumerate() {
        if !field.enabled(fields, answers) || matches!(field.kind, Kind::File(_)) {
            continue;
        }
        match field.param(fields, answers, answers.get(i)) {
            Ok(Some(value)) => {
                out.insert(field.name.clone(), value);
            }
            Ok(None) => {}
            Err(why) => problems.push((i, why)),
        }
    }
    if problems.is_empty() {
        Ok(Value::Object(out))
    } else {
        Err(problems)
    }
}

fn json_value(answer: &str, schema: Option<&JsonSchema>) -> Result<Value, String> {
    if answer.len() > MAX_JSON_PARAM_BYTES {
        return Err(format!("At most {} KiB", MAX_JSON_PARAM_BYTES / 1024));
    }
    let value: Value = serde_json::from_str(answer).map_err(|_| "Not JSON".to_string())?;
    if !(value.is_object() || value.is_array()) {
        return Err("An object or array".to_string());
    }
    match schema.and_then(|schema| schema.refusal(&value)) {
        Some(why) => Err(why),
        None => Ok(value),
    }
}

pub fn uploads(fields: &[Field], answers: &[Answer]) -> Result<Vec<Upload>, Problems> {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    for (i, field) in fields.iter().enumerate() {
        let Kind::File(spec) = &field.kind else {
            continue;
        };
        if !field.enabled(fields, answers) {
            continue;
        }
        let answer = answers.get(i).map_or("", Answer::text);
        if answer.is_empty() {
            if field.required {
                problems.push((i, "Required".to_string()));
            }
            continue;
        }
        match upload(&field.name, spec, Path::new(answer)) {
            Ok(upload) => out.push(upload),
            Err(why) => problems.push((i, why)),
        }
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems)
    }
}

pub fn upload(param: &str, spec: &FileSpec, path: &Path) -> Result<Upload, String> {
    let size = std::fs::metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|meta| meta.len())
        .ok_or_else(|| "No such file".to_string())?;
    too_large(size, spec.max_bytes)?;
    let media_type = media_type(path).map(str::to_string);
    if !spec.media_types.is_empty()
        && !media_type
            .as_ref()
            .is_some_and(|m| spec.media_types.contains(m))
    {
        return Err(format!("One of {}", spec.media_types.join(", ")));
    }
    Ok(Upload {
        param: param.to_string(),
        path: path.to_path_buf(),
        max_bytes: spec.max_bytes,
        media_type,
    })
}

pub fn too_large(size: u64, max_bytes: u64) -> Result<(), String> {
    if size > max_bytes {
        return Err(format!("At most {}", bytes(max_bytes)));
    }
    Ok(())
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 && n % (1 << 20) == 0 => format!("{} MiB", n >> 20),
        n if n >= 1 << 10 && n % (1 << 10) == 0 => format!("{} KiB", n >> 10),
        n => format!("{n} B"),
    }
}

fn media_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "md" => "text/markdown",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        _ => return None,
    })
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

    use std::collections::BTreeSet;

    use serde_json::Value;

    use crate::form::{
        Answer, Field, FileSpec, Kind, Refusal, Scale, Upload, ceilings, duration, fields, minutes,
        params, upload, uploads,
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
        assert_eq!(f[2].default, json!("6"));
        assert_eq!((&f[3].kind, &f[3].default), (&Kind::Bool, &json!(false)));
        assert_eq!(
            f[4].kind,
            Kind::Choice(vec!["fast".into(), "thorough".into()])
        );
        assert_eq!(f[4].default, json!("fast"));
        assert_eq!((&f[5].kind, &f[5].default), (&Kind::Integer, &json!(2)));
        assert_eq!(f[5].default_text(), "2");
        assert_eq!(f[0].default_text(), "", "no default reads as empty text");
        assert!(fields(&json!({})).is_empty());
    }

    #[test]
    fn multiline_requests_keep_their_line_breaks_when_bound_to_launch_params() {
        let schema = json!({
            "properties": {
                "request": {"type": "string", "x-multiline": true},
                "mode": {"type": "string", "enum": ["quick", "full"], "x-multiline": true},
                "enabled": {"type": "boolean", "x-multiline": true}
            },
            "required": ["request"],
            "x-order": ["request", "mode", "enabled"]
        });
        let fields = fields(&schema);
        assert_eq!(fields[0].kind, Kind::Multiline);
        assert_eq!(
            fields[1].kind,
            Kind::Choice(vec!["quick".into(), "full".into()])
        );
        assert_eq!(fields[2].kind, Kind::Bool);
        let request = "Compare revisions:\n\n  baseline: v0.17.0\n  candidate: main";
        assert_eq!(
            params(
                &fields,
                &each(&fields, &[json!(request), json!("full"), json!(true)])
            )
            .unwrap(),
            json!({"request": request, "mode": "full", "enabled": true})
        );
    }

    #[test]
    fn answers_become_typed_params_and_empty_optionals_are_left_out() {
        let f = fields(&triage());
        let answers = each(
            &f,
            &[
                json!("vllm-project/speculators"),
                json!(""),
                json!("2"),
                json!(true),
                json!("thorough"),
                json!("3"),
            ],
        );
        assert_eq!(
            params(&f, &answers).unwrap(),
            json!({"repo": "vllm-project/speculators", "limit": "2", "deep": true, "mode": "thorough", "tries": 3})
        );
    }

    #[test]
    fn problems_name_each_field_that_cannot_be_sent() {
        let f = fields(&triage());
        let answers = each(
            &f,
            &[
                json!("  "),
                json!(""),
                json!(""),
                json!(false),
                json!("slow"),
                json!("three"),
            ],
        );
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

    /// Each field answered with the value named for it, and left empty otherwise.
    fn answer(f: &[Field], pairs: &[(&str, Value)]) -> Vec<Answer> {
        f.iter()
            .map(|field| {
                let named = pairs.iter().find(|(name, _)| *name == field.name);
                field.answer(Some(named.map_or(&Value::Null, |(_, value)| value)))
            })
            .collect()
    }

    /// Each field answered with the value at its place.
    fn each(f: &[Field], values: &[Value]) -> Vec<Answer> {
        f.iter()
            .zip(values)
            .map(|(field, value)| field.answer(Some(value)))
            .collect()
    }

    fn workloads() -> Vec<Field> {
        fields(&json!({
            "properties": {
                "workloads": {"type": "array", "uniqueItems": true, "default": ["chat", "agentic"],
                              "items": {"type": "string", "enum": ["chat", "agentic", "batch"]}},
                "tags": {"type": "array", "items": {"type": "string"}}
            }
        }))
    }

    #[test]
    fn a_list_with_choices_is_picked_as_a_subset_and_sent_as_a_list() {
        let f = workloads();
        assert_eq!(f[0].kind, Kind::Text, "a list without choices stays typed");
        assert_eq!(
            f[1].kind,
            Kind::MultiChoice(vec!["chat".into(), "agentic".into(), "batch".into()])
        );
        let held = |picks: &[usize]| {
            vec![
                f[0].answer(None),
                Answer::MultiChoice(BTreeSet::from_iter(picks.iter().copied())),
            ]
        };
        assert_eq!(
            params(&f, &held(&[2, 0])).unwrap()["workloads"],
            json!(["chat", "batch"]),
            "choice order, not tick order"
        );
        assert_eq!(params(&f, &held(&[])).unwrap()["workloads"], json!([]));
        assert_eq!(
            params(&f, &held(&[1, 9])).unwrap()["workloads"],
            json!(["agentic"]),
            "a place past the list picks nothing"
        );
    }

    #[test]
    fn a_required_list_with_nothing_picked_is_a_problem() {
        let f = fields(&json!({
            "properties": {
                "workloads": {"type": "array", "items": {"enum": ["chat", "agentic"]}}
            },
            "required": ["workloads"]
        }));
        assert_eq!(f[0].default, json!([]));
        assert_eq!(
            params(&f, &[f[0].answer(None)]).unwrap_err(),
            vec![(0, "Pick at least one".to_string())]
        );
        assert_eq!(
            params(&f, &[f[0].answer(Some(&json!(["agentic"])))]).unwrap(),
            json!({"workloads": ["agentic"]})
        );
    }

    #[test]
    fn a_field_starts_from_what_was_sent_before_else_its_default() {
        let f = workloads();
        let picks = |from: Option<&Value>| match f[1].answer(from) {
            Answer::MultiChoice(held) => held.into_iter().collect::<Vec<_>>(),
            _ => panic!("a list with choices is answered by picks"),
        };
        assert_eq!(picks(None), [0, 1], "the default");
        assert_eq!(picks(Some(&json!(["batch"]))), [2]);
        assert_eq!(
            picks(Some(&json!(["batch", "gone", 7, "chat"]))),
            [0, 2],
            "values that are no longer choices are dropped"
        );
        assert_eq!(picks(Some(&json!("chat"))), [0usize; 0], "not a list");

        let f = fields(&triage());
        assert!(f[3].answer(None) == Answer::Flag(false));
        assert!(f[3].answer(Some(&json!(true))) == Answer::Flag(true));
        assert!(
            f[3].answer(Some(&json!("true"))) == Answer::Flag(false),
            "only a bool turns a switch on"
        );
        assert_eq!(f[2].answer(None).text(), "6");
        assert_eq!(f[2].answer(Some(&json!(" 9 "))).text(), "9");
        assert_eq!(f[4].answer(None).text(), "fast", "the first choice");
        assert_eq!(f[5].answer(None).text(), "2");
        assert_eq!(f[5].answer(Some(&json!(7))).text(), "7");
        assert_eq!(f[0].answer(None).text(), "");
        assert_eq!(Answer::Flag(true).text(), "");
    }

    #[test]
    fn an_answer_that_is_missing_or_the_wrong_shape_is_no_answer() {
        let f = fields(&triage());
        assert_eq!(
            params(&f, &[]).unwrap_err(),
            vec![(0, "Required".to_string())]
        );
        let mut answers = answer(&f, &[("repo", json!("a/b"))]);
        answers[3] = f[0].answer(Some(&json!("true")));
        answers[5] = Answer::Flag(true);
        assert_eq!(params(&f, &answers).unwrap(), json!({"repo": "a/b"}));
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
        let on = |release: &str| f[1].offered(&f, &answer(&f, &[("release", json!(release))]));
        assert_eq!(on("3.1"), Some(vec!["vllm".into(), "llm-d".into()]));
        assert_eq!(on(" 3.0 "), Some(vec!["vllm".into()]));
        assert_eq!(on("9.9"), Some(vec![]));
    }

    #[test]
    fn a_keyed_placeholder_follows_the_key_and_falls_back_to_the_default() {
        let f = fields(&release());
        assert_eq!(
            f[3].placeholder(&f, &answer(&f, &[("release", json!("3.1"))])),
            "rhoai-3.1"
        );
        assert_eq!(
            f[3].placeholder(&f, &answer(&f, &[("release", json!("3.0"))])),
            ""
        );
    }

    #[test]
    fn a_gated_field_is_sent_only_while_its_switch_is_on() {
        let f = fields(&release());
        let base = [
            ("release", json!("3.1")),
            ("component", json!("vllm")),
            ("branch", json!("main")),
        ];
        let off = answer(
            &f,
            &[base.as_slice(), &[("override", json!(false))]].concat(),
        );
        assert!(!f[3].enabled(&f, &off));
        let sent = params(&f, &off).unwrap();
        assert_eq!(sent.get("branch"), None);
        assert_eq!(sent["override"], json!(false));

        let on = answer(
            &f,
            &[base.as_slice(), &[("override", json!(true))]].concat(),
        );
        assert!(f[3].enabled(&f, &on));
        assert_eq!(params(&f, &on).unwrap()["branch"], json!("main"));
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
        let problems =
            |pairs: &[(&str, Value)]| params(&f, &answer(&f, pairs)).err().unwrap_or_default();
        assert_eq!(
            problems(&[("release", json!("2.9")), ("stage", json!("qa"))]),
            vec![
                (0, "One of 3.0, 3.1".to_string()),
                (5, "One of dev, prod".to_string()),
            ]
        );
        assert_eq!(
            problems(&[("release", json!("3.2")), ("component", json!("vllm"))]),
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
                    ("release", json!("3.1")),
                    ("component", json!("llm-d")),
                    ("image", json!("alpine")),
                    ("unread", json!("anything")),
                ],
            ),
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

    fn typed() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "config": {"type": ["object", "array"], "x-param-type": "json",
                           "allOf": [{"type": "object", "required": ["name"],
                                      "properties": {"name": {"type": "string"}}}]},
                "extra": {"type": ["object", "array"], "x-param-type": "json"},
                "notes": {"type": "object", "x-param-type": "file", "x-max-bytes": 8,
                          "x-media-type": ["text/markdown"]},
                "blob": {"type": "object", "x-param-type": "file"}
            },
            "required": ["config", "notes"],
            "x-order": ["config", "extra", "notes", "blob"]
        })
    }

    #[test]
    fn json_and_file_params_read_their_kinds_from_x_param_type() {
        let f = fields(&typed());
        assert!(matches!(&f[0].kind, Kind::Json(Some(_))), "{:?}", f[0].kind);
        assert_eq!(f[1].kind, Kind::Json(None));
        assert_eq!(
            f[2].kind,
            Kind::File(FileSpec {
                max_bytes: 8,
                media_types: vec!["text/markdown".into()]
            })
        );
        assert_eq!(
            f[3].kind,
            Kind::File(FileSpec {
                max_bytes: crucible_contract::calls::MAX_FILE_PARAM_BYTES,
                media_types: Vec::new()
            })
        );
    }

    #[test]
    fn a_json_answer_is_sent_as_its_value_once_it_parses_and_fits_its_schema() {
        let f = fields(&typed());
        let with = |config: &str, extra: &str| {
            params(
                &f,
                &[
                    Answer::typed(config),
                    Answer::typed(extra),
                    Answer::typed(""),
                    Answer::typed(""),
                ],
            )
        };
        assert_eq!(
            with(r#"{"name": "a"}"#, "[1, 2]").unwrap(),
            json!({"config": {"name": "a"}, "extra": [1, 2]})
        );
        let problem = |config: &str, extra: &str| with(config, extra).unwrap_err();
        assert_eq!(problem("{", ""), vec![(0, "Not JSON".to_string())]);
        assert_eq!(
            problem(r#"{"name": "a"}"#, "3"),
            vec![(1, "An object or array".to_string())]
        );
        let refused = problem("{}", "");
        assert_eq!(refused.len(), 1);
        assert!(refused[0].1.contains("name"), "{refused:?}");
        let big = format!("[{}0]", "0,".repeat(40 * 1024));
        assert_eq!(
            problem(r#"{"name": "a"}"#, &big),
            vec![(1, "At most 64 KiB".to_string())]
        );
        assert_eq!(problem("", ""), vec![(0, "Required".to_string())]);
    }

    #[test]
    fn a_file_answer_is_an_upload_refused_past_its_cap_or_media_type_before_sending() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes.md");
        std::fs::write(&notes, "# hi").unwrap();
        let f = fields(&typed());
        let answers = |notes: &str, blob: &str| {
            vec![
                Answer::typed(r#"{"name": "a"}"#),
                Answer::typed(""),
                Answer::typed(notes),
                Answer::typed(blob),
            ]
        };
        let path = notes.display().to_string();
        assert_eq!(
            uploads(&f, &answers(&path, "")).unwrap(),
            vec![Upload {
                param: "notes".into(),
                path: notes.clone(),
                max_bytes: 8,
                media_type: Some("text/markdown".into()),
            }]
        );
        assert_eq!(
            params(&f, &answers(&path, &path)).unwrap(),
            json!({"config": {"name": "a"}}),
            "a file is never a param value before it is uploaded"
        );

        let problems = |notes: &str, blob: &str| uploads(&f, &answers(notes, blob)).unwrap_err();
        assert_eq!(problems("", ""), vec![(2, "Required".to_string())]);
        let missing = dir.path().join("gone.md").display().to_string();
        assert_eq!(
            problems(&missing, ""),
            vec![(2, "No such file".to_string())]
        );
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "hi").unwrap();
        assert_eq!(
            problems(&text.display().to_string(), ""),
            vec![(2, "One of text/markdown".to_string())]
        );
        std::fs::write(&notes, "# too long").unwrap();
        assert_eq!(problems(&path, ""), vec![(2, "At most 8 B".to_string())]);

        let any = FileSpec {
            max_bytes: 1024,
            media_types: Vec::new(),
        };
        let bare = dir.path().join("data");
        std::fs::write(&bare, "x").unwrap();
        assert_eq!(upload("blob", &any, &bare).unwrap().media_type, None);
        assert_eq!(
            upload("blob", &any, dir.path()),
            Err("No such file".to_string())
        );
        let sixteen = FileSpec {
            max_bytes: 16 << 20,
            media_types: Vec::new(),
        };
        assert_eq!(upload("blob", &sixteen, &bare).unwrap().max_bytes, 16 << 20);
        assert_eq!(
            crate::form::too_large((16 << 20) + 1, 16 << 20),
            Err("At most 16 MiB".to_string())
        );
    }
}
