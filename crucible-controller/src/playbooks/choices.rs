//! Launch-form choices a producer task supplies ([ADR-0074]): the latest passing value of the
//! producer's field among this playbook's runs, attached to the schema the form and admission read.
//!
//! [ADR-0074]: ../../../docs/adr/ADR-0074.md

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

pub const MAX_CHOICES: usize = 256;
pub const MAX_CHOICE_CHARS: usize = 256;

/// The snapshot key a served schema carries on each property that declares `x-choices-from`.
pub const SNAPSHOT_KEY: &str = "x-choices-snapshot";

/// The snapshot key a served schema carries on each property that declares
/// `x-placeholder-from`.
pub const PLACEHOLDER_KEY: &str = "x-placeholder-snapshot";

/// The snapshot key a served schema carries on each property that declares `x-confirm-from`.
pub const CONFIRM_KEY: &str = "x-confirm-snapshot";

/// How many recent passing outputs to try when the newest is out of bounds, so one bad refresh
/// keeps the previous list instead of emptying the form.
const CANDIDATES: i64 = 8;

/// What a producer's field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A list of choices.
    List,
    /// An object from another parameter's values to a list of choices each.
    KeyedLists,
    /// An object from another parameter's values to one string each.
    KeyedText,
    /// An object from another parameter's values to whether a launch must confirm it.
    KeyedFlags,
}

impl Shape {
    /// The field's value within the bounds, normalized: duplicates collapse to the first.
    /// `None` when it is not this shape or is out of bounds.
    pub fn bounded(self, value: &serde_json::Value) -> Option<serde_json::Value> {
        match self {
            Shape::List => bounded(value).map(|c| serde_json::json!(c)),
            Shape::KeyedLists | Shape::KeyedText | Shape::KeyedFlags => {
                let entries = value.as_object()?;
                if entries.len() > MAX_CHOICES {
                    return None;
                }
                let mut out = serde_json::Map::new();
                for (key, item) in entries {
                    if key.chars().count() > MAX_CHOICE_CHARS {
                        return None;
                    }
                    let item = match self {
                        Shape::KeyedLists => serde_json::json!(bounded(item)?),
                        Shape::KeyedFlags => serde_json::json!(item.as_bool()?),
                        _ => serde_json::json!(text(item)?),
                    };
                    out.insert(key.clone(), item);
                }
                Some(serde_json::Value::Object(out))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Snapshot {
    pub value: serde_json::Value,
    pub run_id: String,
    pub captured_at: Option<String>,
    pub digest: String,
}

/// The choices a producer field offers: strings within the bounds, duplicates collapsed to the
/// first. `None` when the value is not such a list.
pub fn bounded(value: &serde_json::Value) -> Option<Vec<String>> {
    let items = value.as_array()?;
    if items.len() > MAX_CHOICES {
        return None;
    }
    let mut choices: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let text = text(item)?;
        if !choices.iter().any(|seen| seen == text) {
            choices.push(text.to_owned());
        }
    }
    Some(choices)
}

/// A string within the length bound.
fn text(value: &serde_json::Value) -> Option<&str> {
    value
        .as_str()
        .filter(|text| text.chars().count() <= MAX_CHOICE_CHARS)
}

fn digest(value: &serde_json::Value) -> String {
    format!("sha256:{:x}", Sha256::digest(value.to_string()))
}

/// One passing attempt's output of a producer task.
pub struct Output {
    output: serde_json::Value,
    run_id: String,
    captured_at: Option<String>,
}

/// The recent passing outputs of `task` among this playbook's runs, newest first.
pub async fn outputs(pool: &PgPool, playbook: &str, task: &str) -> Result<Vec<Output>> {
    sqlx::query(
        "SELECT t.output, t.run_id, r.created_at \
         FROM run_task_results t \
         JOIN runs r ON r.run_id = t.run_id \
         JOIN playbook_launches l ON l.key = r.issue \
         WHERE l.playbook = $1 AND t.task = $2 AND t.status = 'pass' AND t.output IS NOT NULL \
         ORDER BY r.created_at DESC NULLS LAST, r.seq DESC, t.iter DESC \
         LIMIT $3",
    )
    .bind(playbook)
    .bind(task)
    .bind(CANDIDATES)
    .fetch_all(pool)
    .await
    .context("reading a choice producer's outputs")?
    .into_iter()
    .map(|row| {
        Ok(Output {
            output: row.try_get("output")?,
            run_id: row.try_get("run_id")?,
            captured_at: row.try_get("created_at")?,
        })
    })
    .collect()
}

/// The newest in-bounds value of `field` among a producer's `outputs`.
pub fn latest(outputs: &[Output], field: &str, shape: Shape) -> Option<Snapshot> {
    outputs.iter().find_map(|attempt| {
        let value = shape.bounded(attempt.output.get(field)?)?;
        Some(Snapshot {
            digest: digest(&value),
            value,
            run_id: attempt.run_id.clone(),
            captured_at: attempt.captured_at.clone(),
        })
    })
}

/// The task and field a property names under `key`, and whether the field is keyed.
fn producer<'p>(property: &'p serde_json::Value, key: &str) -> Option<(&'p str, &'p str, bool)> {
    let from = property.get(key)?;
    let task = from.get("task")?.as_str()?;
    let field = from.get("field")?.as_str()?;
    Some((task, field, from.get("keyed_by").is_some()))
}

/// `schema` with each `x-choices-from`, `x-placeholder-from`, and `x-confirm-from` property
/// carrying its snapshot, and the `{param: {run_id, digest}}` record of the choices and
/// confirmations a launch stores. A property with no snapshot yet is left as declared.
pub async fn attach(
    pool: &PgPool,
    playbook: &str,
    schema: &serde_json::Value,
) -> Result<(serde_json::Value, serde_json::Value)> {
    let mut served = schema.clone();
    let mut record = serde_json::Map::new();
    let Some(properties) = served
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok((served, serde_json::Value::Object(record)));
    };
    let mut by_task: std::collections::BTreeMap<String, Vec<Output>> = Default::default();
    for (name, property) in properties.iter_mut() {
        for (from, into) in [
            ("x-choices-from", SNAPSHOT_KEY),
            ("x-placeholder-from", PLACEHOLDER_KEY),
            ("x-confirm-from", CONFIRM_KEY),
        ] {
            let Some((task, field, keyed)) = producer(property, from) else {
                continue;
            };
            let placeholder = into == PLACEHOLDER_KEY;
            let shape = match into {
                PLACEHOLDER_KEY => Shape::KeyedText,
                CONFIRM_KEY => Shape::KeyedFlags,
                _ if keyed => Shape::KeyedLists,
                _ => Shape::List,
            };
            if !by_task.contains_key(task) {
                by_task.insert(task.to_owned(), outputs(pool, playbook, task).await?);
            }
            let Some(snapshot) = by_task.get(task).and_then(|o| latest(o, field, shape)) else {
                continue;
            };
            if !placeholder {
                record.insert(
                    name.clone(),
                    serde_json::json!({"run_id": snapshot.run_id, "digest": snapshot.digest}),
                );
            }
            if let Some(object) = property.as_object_mut() {
                object.insert(into.into(), serde_json::to_value(&snapshot)?);
            }
        }
    }
    Ok((served, serde_json::Value::Object(record)))
}

/// The value a launch binds for the parameter `property`'s `from` source is keyed by: the
/// supplied one, or that parameter's declared default.
pub fn key_value<'a>(
    schema: &'a serde_json::Value,
    property: &serde_json::Value,
    from: &str,
    params: &'a std::collections::BTreeMap<String, String>,
) -> Option<&'a str> {
    let key = property.get(from)?.get("keyed_by")?.as_str()?;
    params
        .get(key)
        .map(String::as_str)
        .or_else(|| schema.get("properties")?.get(key)?.get("default")?.as_str())
}

/// The key parameter and its value when `property`'s confirmation snapshot flags the value a
/// launch binds for it, so the launch must set the property true.
pub fn confirmation_needed<'a>(
    schema: &'a serde_json::Value,
    property: &'a serde_json::Value,
    params: &'a std::collections::BTreeMap<String, String>,
) -> Option<(&'a str, &'a str)> {
    let flags = property.get(CONFIRM_KEY)?.get("value")?;
    let key = property.pointer("/x-confirm-from/keyed_by")?.as_str()?;
    let value = key_value(schema, property, "x-confirm-from", params)?;
    (flags.get(value) == Some(&serde_json::Value::Bool(true))).then_some((key, value))
}

/// The supplied value is outside a closed snapshot. A keyed snapshot offers the list for `key`,
/// the value of the parameter it is keyed by, and none when there is no list for it. Open choices
/// (`x-allow-other`) and properties with no snapshot accept any value their type and pattern
/// accept.
pub fn outside_snapshot(property: &serde_json::Value, value: &str, key: Option<&str>) -> bool {
    if property.get("x-allow-other") == Some(&serde_json::Value::Bool(true)) {
        return false;
    }
    let Some(offered) = property.get(SNAPSHOT_KEY).and_then(|s| s.get("value")) else {
        return false;
    };
    let list = if property.pointer("/x-choices-from/keyed_by").is_some() {
        key.and_then(|k| offered.get(k))
    } else {
        Some(offered)
    };
    !list
        .and_then(serde_json::Value::as_array)
        .is_some_and(|choices| choices.iter().any(|c| c.as_str() == Some(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_keeps_strings_in_order_and_refuses_anything_else() {
        let ok = serde_json::json!(["3.7 Fast-1", "3.6.0 GA", "3.7 Fast-1"]);
        assert_eq!(
            bounded(&ok),
            Some(vec!["3.7 Fast-1".to_owned(), "3.6.0 GA".to_owned()])
        );
        assert_eq!(bounded(&serde_json::json!(["a", 1])), None);
        assert_eq!(bounded(&serde_json::json!("a")), None);
        assert_eq!(
            bounded(&serde_json::json!(vec!["x"; MAX_CHOICES + 1])),
            None
        );
        assert_eq!(
            bounded(&serde_json::json!(["y".repeat(MAX_CHOICE_CHARS + 1)])),
            None
        );
    }

    async fn seed(
        pool: &PgPool,
        playbook: &str,
        run: &str,
        at: &str,
        status: &str,
        output: serde_json::Value,
    ) {
        let key = format!("playbook:{playbook}:{run}");
        sqlx::query(
            "INSERT INTO issues (key, repo, tier, status, priority, input_kind, title, updated_at) \
             VALUES ($1, 'o/r', 0, 'done', 0, 'playbook', 't', $2)",
        )
        .bind(&key)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO playbook_launches (key, playbook, params, schema_digest, max_cost, max_time, \
                                            created_at) \
             VALUES ($1, $2, '{}'::jsonb, 'sha256:s', 1.0, '1h', $3)",
        )
        .bind(&key)
        .bind(playbook)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO runs (run_id, issue, status, created_at) VALUES ($1, $2, 'done', $3)",
        )
        .bind(run)
        .bind(&key)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO run_task_results (run_id, iter, task, status, output) \
             VALUES ($1, 0, 'options', $2, $3)",
        )
        .bind(run)
        .bind(status)
        .bind(output)
        .execute(pool)
        .await
        .unwrap();
    }

    /// The newest passing in-bounds output of this playbook's producer is the snapshot: a failed
    /// attempt, an out-of-bounds value, and another playbook's run never displace it.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_snapshot_is_the_newest_passing_in_bounds_output_of_this_playbook(pool: PgPool) {
        let releases = |r: &[&str]| serde_json::json!({"releases": r});
        seed(
            &pool,
            "backport",
            "r1",
            "2026-11-01T00:00:00Z",
            "pass",
            releases(&["3.6"]),
        )
        .await;
        seed(
            &pool,
            "backport",
            "r2",
            "2026-11-02T00:00:00Z",
            "pass",
            serde_json::json!({
                "releases": ["3.6", "3.7"],
                "branch": {"3.7": "sync-v0.33.0"},
                "tentative": {"3.6": false, "3.7": true},
            }),
        )
        .await;
        seed(
            &pool,
            "backport",
            "r3",
            "2026-11-03T00:00:00Z",
            "fail",
            releases(&["bogus"]),
        )
        .await;
        let huge = serde_json::json!({"releases": vec!["x"; MAX_CHOICES + 1]});
        seed(
            &pool,
            "backport",
            "r4",
            "2026-11-04T00:00:00Z",
            "pass",
            huge,
        )
        .await;
        seed(
            &pool,
            "other",
            "r5",
            "2026-11-05T00:00:00Z",
            "pass",
            releases(&["nope"]),
        )
        .await;

        let produced = outputs(&pool, "backport", "options").await.unwrap();
        let snapshot = latest(&produced, "releases", Shape::List).unwrap();
        assert_eq!(snapshot.value, serde_json::json!(["3.6", "3.7"]));
        assert_eq!(snapshot.run_id, "r2");

        let schema = serde_json::json!({"properties": {
            "release": {"type": "string", "x-choices-from": {"task": "options", "field": "releases"}},
            "pr": {"type": "string"},
            "branch": {"type": "string", "x-placeholder-from": {"task": "options", "field": "branch", "keyed_by": "release"}},
            "accept": {"type": "boolean", "default": false, "x-confirm-from": {"task": "options", "field": "tentative", "keyed_by": "release"}},
        }});
        let (served, record) = attach(&pool, "backport", &schema).await.unwrap();
        assert_eq!(
            served["properties"]["release"][SNAPSHOT_KEY]["value"],
            serde_json::json!(["3.6", "3.7"])
        );
        assert!(served["properties"]["pr"].get(SNAPSHOT_KEY).is_none());
        assert_eq!(
            served["properties"]["branch"][PLACEHOLDER_KEY]["value"],
            serde_json::json!({"3.7": "sync-v0.33.0"})
        );
        assert!(
            record.get("branch").is_none(),
            "a placeholder is display only"
        );
        assert_eq!(record["release"]["run_id"], "r2");
        assert_eq!(record["release"]["digest"], snapshot.digest);
        assert_eq!(record["accept"]["run_id"], "r2");
        let params = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<std::collections::BTreeMap<String, String>>()
        };
        let validate = |p| crate::playbooks::registry::validate_params(&served, &p);
        assert!(validate(params(&[("release", "3.6")])).is_ok());
        assert!(validate(params(&[("release", "3.9")])).is_err());
        let unconfirmed = validate(params(&[("release", "3.7")])).unwrap_err();
        assert_eq!(
            unconfirmed
                .iter()
                .map(|e| e.field.as_str())
                .collect::<Vec<_>>(),
            ["accept"],
            "a flagged release needs its confirmation"
        );
        assert!(validate(params(&[("release", "3.7"), ("accept", "false")])).is_err());
        assert!(validate(params(&[("release", "3.7"), ("accept", "true")])).is_ok());

        let (unseeded, record) = attach(&pool, "fresh", &schema).await.unwrap();
        assert_eq!(unseeded, schema);
        assert_eq!(record, serde_json::json!({}));
    }

    /// A refresh skips the declared params' schema, so a required param need not be supplied,
    /// but it must be the only value and the playbook must declare something to refresh. Its
    /// admission compiles the producer closure alone.
    #[test]
    fn a_refresh_launch_is_its_switch_alone_and_compiles_the_producer_closure() {
        use crate::playbooks::registry::{compiled_plan, validate_params};
        let schema = serde_json::json!({
            "type": "object", "additionalProperties": false, "required": ["pr"],
            "properties": {
                "pr": {"type": "integer"},
                "release": {"type": "string", "x-choices-from": {"task": "options", "field": "releases"}},
            },
        });
        let params = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<std::collections::BTreeMap<String, String>>()
        };
        let refresh = params(&[("choices.refresh", "true")]);
        assert_eq!(
            validate_params(&schema, &refresh).unwrap(),
            serde_json::json!({"choices.refresh": "true"})
        );
        for refused in [
            params(&[("choices.refresh", "true"), ("pr", "1")]),
            params(&[("choices.refresh", "yes")]),
        ] {
            assert!(validate_params(&schema, &refused).is_err(), "{refused:?}");
        }
        let mut plain = schema.clone();
        plain["properties"]["release"]
            .as_object_mut()
            .unwrap()
            .remove("x-choices-from");
        assert!(validate_params(&plain, &refresh).is_err());
        plain["properties"]["release"]["x-placeholder-from"] =
            serde_json::json!({"task": "options", "field": "branch", "keyed_by": "pr"});
        assert!(validate_params(&plain, &refresh).is_ok());
        let mut confirming = schema.clone();
        confirming["properties"]["release"] = serde_json::json!({
            "type": "boolean",
            "x-confirm-from": {"task": "options", "field": "tentative", "keyed_by": "pr"},
        });
        assert!(validate_params(&confirming, &refresh).is_ok());

        let dir = tempfile::tempdir().unwrap();
        let root = crate::testing::fixtures::write_playbook_pack(
            dir.path(),
            r#"params = {
    "pr": {"type": "int", "required": True},
    "release": {"type": "string", "default": "", "choices_from": {"task": "options", "field": "releases"}},
}
options = command(name = "options", run = "./o.sh", emits = {"releases": "list"})
work = agent(name = "work", prompt = "Backport " + str(param("pr")), depends_on = [options])
workflow(type = "playbook", tasks = [options, work])
"#,
        );
        let plan = compiled_plan(&root, &refresh, None).unwrap();
        let names: Vec<&str> = plan.tasks_topo().map(|t| t.name.0.as_str()).collect();
        assert_eq!(names, ["options"]);
    }

    #[test]
    fn only_a_closed_snapshot_refuses_a_value() {
        let closed = serde_json::json!({"x-choices-snapshot": {"value": ["a", "b"]}});
        let open =
            serde_json::json!({"x-allow-other": true, "x-choices-snapshot": {"value": ["a"]}});
        assert!(outside_snapshot(&closed, "c", None));
        assert!(!outside_snapshot(&closed, "a", None));
        assert!(!outside_snapshot(&open, "c", None));
        assert!(!outside_snapshot(
            &serde_json::json!({"x-choices-from": {}}),
            "c",
            None
        ));
    }

    /// A keyed snapshot offers the list for the key's supplied value, and nothing for a key it
    /// has no list for.
    #[test]
    fn a_keyed_snapshot_offers_the_list_for_the_supplied_key() {
        let keyed = serde_json::json!({
            "x-choices-from": {"task": "o", "field": "c", "keyed_by": "release"},
            "x-choices-snapshot": {"value": {"3.7": ["vllm", "flashinfer"], "3.6": ["vllm"]}},
        });
        assert!(!outside_snapshot(&keyed, "flashinfer", Some("3.7")));
        assert!(outside_snapshot(&keyed, "flashinfer", Some("3.6")));
        assert!(outside_snapshot(&keyed, "vllm", Some("3.9")));
        assert!(outside_snapshot(&keyed, "vllm", None));

        let schema = serde_json::json!({"properties": {
            "release": {"type": "string", "default": "3.6"},
            "component": keyed,
        }});
        let component = &schema["properties"]["component"];
        let supplied = std::collections::BTreeMap::from([("release".to_owned(), "3.7".to_owned())]);
        let from = "x-choices-from";
        assert_eq!(key_value(&schema, component, from, &supplied), Some("3.7"));
        let defaulted = std::collections::BTreeMap::new();
        assert_eq!(key_value(&schema, component, from, &defaulted), Some("3.6"));
        assert_eq!(
            key_value(&schema, &schema["properties"]["release"], from, &supplied),
            None
        );
    }

    #[test]
    fn a_keyed_value_is_bounded_per_key() {
        let lists = serde_json::json!({"3.7": ["a", "a", "b"], "3.6": []});
        assert_eq!(
            Shape::KeyedLists.bounded(&lists),
            Some(serde_json::json!({"3.6": [], "3.7": ["a", "b"]}))
        );
        assert_eq!(
            Shape::KeyedLists.bounded(&serde_json::json!({"3.7": "a"})),
            None
        );
        assert_eq!(
            Shape::KeyedText.bounded(&serde_json::json!({"3.7": "sync-v0.33.0"})),
            Some(serde_json::json!({"3.7": "sync-v0.33.0"}))
        );
        assert_eq!(
            Shape::KeyedText.bounded(&serde_json::json!({"3.7": ["a"]})),
            None
        );
        assert_eq!(
            Shape::KeyedText.bounded(&serde_json::json!({"3.7": "x".repeat(MAX_CHOICE_CHARS + 1)})),
            None
        );
        let wide: serde_json::Map<String, serde_json::Value> = (0..=MAX_CHOICES)
            .map(|i| (i.to_string(), serde_json::json!("b")))
            .collect();
        assert_eq!(
            Shape::KeyedText.bounded(&serde_json::Value::Object(wide)),
            None
        );
        assert_eq!(
            Shape::KeyedFlags.bounded(&serde_json::json!({"3.7": true, "3.6": false})),
            Some(serde_json::json!({"3.6": false, "3.7": true}))
        );
        assert_eq!(
            Shape::KeyedFlags.bounded(&serde_json::json!({"3.7": "true"})),
            None
        );
    }
}
