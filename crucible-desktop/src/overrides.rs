//! The controller's setting overrides: which settings are overridden now, and that set with one
//! changed. A save replaces the whole set, so every other override has to ride along.

use crux::dto::Knob;
use serde_json::{Map, Value};

/// The overrides in effect, by setting name.
pub fn current(knobs: &[Knob]) -> Map<String, Value> {
    knobs
        .iter()
        .filter(|knob| knob.source == "override")
        .map(|knob| (knob.name.clone(), knob.value.clone()))
        .collect()
}

/// A value as the edit field shows it: a list as comma-separated items.
pub fn shown(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map_or_else(|| item.to_string(), str::to_string)
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// `typed` as a value of the same kind as `like`, the setting's value now.
pub fn parse(like: &Value, typed: &str) -> Result<Value, String> {
    let typed = typed.trim();
    match like {
        Value::Bool(_) => match typed {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err("true or false".into()),
        },
        Value::Number(now) if now.is_f64() => typed
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| "a number, 0 or more".to_string()),
        Value::Number(_) => typed
            .parse::<u64>()
            .map(|n| Value::Number(n.into()))
            .map_err(|_| "a whole number, 0 or more".to_string()),
        Value::Array(_) => Ok(Value::Array(
            typed
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| Value::String(item.to_string()))
                .collect(),
        )),
        _ if typed.is_empty() => Err("a value".into()),
        _ => Ok(Value::String(typed.to_string())),
    }
}

/// The override set with `name` set to what `typed` reads as.
pub fn with(knobs: &[Knob], name: &str, typed: &str) -> Result<Map<String, Value>, String> {
    let knob = knobs
        .iter()
        .find(|knob| knob.name == name && knob.overridable)
        .ok_or_else(|| format!("{name} cannot be overridden"))?;
    let mut set = current(knobs);
    set.insert(name.to_string(), parse(&knob.value, typed)?);
    Ok(set)
}

/// The override set with `name` back at its env or default value.
pub fn without(knobs: &[Knob], name: &str) -> Map<String, Value> {
    let mut set = current(knobs);
    set.remove(name);
    set
}

#[cfg(test)]
mod tests {
    use crux::dto::Knob;
    use serde_json::{Value, json};

    use crate::overrides::{current, parse, shown, with, without};

    fn knob(name: &str, value: Value, source: &str) -> Knob {
        Knob {
            name: name.into(),
            value,
            source: source.into(),
            overridable: name != "locked",
            description: String::new(),
        }
    }

    fn knobs() -> Vec<Knob> {
        vec![
            knob("max_concurrent_pods", json!(10), "override"),
            knob("daily_cost_ceiling", json!(1000.0), "override"),
            knob("allow_t3", json!(false), "env"),
            knob("allowed_tiers", json!(["t0", "t1"]), "default"),
            knob("locked", json!(1), "env"),
        ]
    }

    #[test]
    fn a_change_to_one_setting_carries_every_other_override() {
        let knobs = knobs();
        assert_eq!(
            Value::Object(current(&knobs)),
            json!({ "max_concurrent_pods": 10, "daily_cost_ceiling": 1000.0 })
        );
        assert_eq!(
            Value::Object(with(&knobs, "allow_t3", "true").unwrap()),
            json!({ "max_concurrent_pods": 10, "daily_cost_ceiling": 1000.0, "allow_t3": true })
        );
        assert_eq!(
            Value::Object(with(&knobs, "max_concurrent_pods", " 4 ").unwrap()),
            json!({ "max_concurrent_pods": 4, "daily_cost_ceiling": 1000.0 })
        );
        assert_eq!(
            Value::Object(without(&knobs, "max_concurrent_pods")),
            json!({ "daily_cost_ceiling": 1000.0 })
        );
        assert!(with(&knobs, "locked", "2").is_err());
        assert!(with(&knobs, "no_such", "2").is_err());
    }

    #[test]
    fn a_typed_value_reads_as_the_kind_the_setting_holds() {
        assert_eq!(parse(&json!(10), "12"), Ok(json!(12)));
        assert!(parse(&json!(10), "1.5").is_err());
        assert!(parse(&json!(10), "-1").is_err());
        assert_eq!(parse(&json!(0.05), "2"), Ok(json!(2.0)));
        assert!(parse(&json!(0.05), "cheap").is_err());
        assert_eq!(parse(&json!(true), "false"), Ok(json!(false)));
        assert!(parse(&json!(true), "yes").is_err());
        assert_eq!(parse(&json!(["t0"]), " t0, t2 ,,"), Ok(json!(["t0", "t2"])));
        assert_eq!(parse(&json!(["t0"]), ""), Ok(json!([])));
        assert_eq!(parse(&json!("a"), " b "), Ok(json!("b")));
        assert!(parse(&json!("a"), " ").is_err());
    }

    #[test]
    fn a_value_shows_the_way_it_is_typed_back() {
        for value in [
            json!(10),
            json!(0.05),
            json!(false),
            json!(["t0", "t1"]),
            json!("x"),
        ] {
            assert_eq!(parse(&value, &shown(&value)), Ok(value.clone()), "{value}");
        }
        assert_eq!(shown(&json!(["t0", "t1"])), "t0, t1");
    }
}
