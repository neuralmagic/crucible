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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::form::{Kind, ceilings, fields, minutes, params};

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
