//! Launch-form choices a producer task supplies ([ADR-0073]): the latest passing value of the
//! producer's field among this playbook's runs, attached to the schema the form and admission read.
//!
//! [ADR-0073]: ../../../docs/adr/ADR-0073.md

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

pub const MAX_CHOICES: usize = 256;
pub const MAX_CHOICE_CHARS: usize = 256;

/// The snapshot key a served schema carries on each property that declares `x-choices-from`.
pub const SNAPSHOT_KEY: &str = "x-choices-snapshot";

/// How many recent passing outputs to try when the newest is out of bounds, so one bad refresh
/// keeps the previous list instead of emptying the form.
const CANDIDATES: i64 = 8;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Snapshot {
    pub choices: Vec<String>,
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
        let text = item.as_str()?;
        if text.chars().count() > MAX_CHOICE_CHARS {
            return None;
        }
        if !choices.iter().any(|seen| seen == text) {
            choices.push(text.to_owned());
        }
    }
    Some(choices)
}

fn digest(choices: &[String]) -> String {
    let bytes = serde_json::to_vec(choices).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// The newest in-bounds value `task` emitted as `field` in a passing attempt of one of this
/// playbook's runs.
pub async fn latest(
    pool: &PgPool,
    playbook: &str,
    task: &str,
    field: &str,
) -> Result<Option<Snapshot>> {
    let rows = sqlx::query(
        "SELECT t.output -> $3 AS choices, t.run_id, r.created_at \
         FROM run_task_results t \
         JOIN runs r ON r.run_id = t.run_id \
         JOIN playbook_launches l ON l.key = r.issue \
         WHERE l.playbook = $1 AND t.task = $2 AND t.status = 'pass' \
           AND jsonb_typeof(t.output -> $3) = 'array' \
         ORDER BY r.created_at DESC NULLS LAST, r.seq DESC, t.iter DESC \
         LIMIT $4",
    )
    .bind(playbook)
    .bind(task)
    .bind(field)
    .bind(CANDIDATES)
    .fetch_all(pool)
    .await
    .context("reading a choice producer's outputs")?;
    for row in rows {
        let value: serde_json::Value = row.try_get("choices")?;
        if let Some(choices) = bounded(&value) {
            return Ok(Some(Snapshot {
                digest: digest(&choices),
                choices,
                run_id: row.try_get("run_id")?,
                captured_at: row.try_get("created_at")?,
            }));
        }
    }
    Ok(None)
}

/// `schema` with each `x-choices-from` property carrying its snapshot, and the `{param: {run_id,
/// digest}}` record a launch stores. A property with no snapshot yet is left as declared.
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
    for (name, property) in properties.iter_mut() {
        let from = property.get("x-choices-from");
        let (Some(task), Some(field)) = (
            from.and_then(|f| f.get("task"))
                .and_then(serde_json::Value::as_str),
            from.and_then(|f| f.get("field"))
                .and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        if let Some(snapshot) = latest(pool, playbook, task, field).await? {
            record.insert(
                name.clone(),
                serde_json::json!({"run_id": snapshot.run_id, "digest": snapshot.digest}),
            );
            if let Some(object) = property.as_object_mut() {
                object.insert(SNAPSHOT_KEY.into(), serde_json::to_value(&snapshot)?);
            }
        }
    }
    Ok((served, serde_json::Value::Object(record)))
}

/// The supplied value is outside a closed snapshot. Open choices (`x-allow-other`) and properties
/// with no snapshot accept any value their type and pattern accept.
pub fn outside_snapshot(property: &serde_json::Value, value: &str) -> bool {
    if property.get("x-allow-other") == Some(&serde_json::Value::Bool(true)) {
        return false;
    }
    property
        .get(SNAPSHOT_KEY)
        .and_then(|s| s.get("choices"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|choices| !choices.iter().any(|c| c.as_str() == Some(value)))
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
            releases(&["3.6", "3.7"]),
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

        let snapshot = latest(&pool, "backport", "options", "releases")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.choices, ["3.6", "3.7"]);
        assert_eq!(snapshot.run_id, "r2");

        let schema = serde_json::json!({"properties": {
            "release": {"type": "string", "x-choices-from": {"task": "options", "field": "releases"}},
            "pr": {"type": "string"},
        }});
        let (served, record) = attach(&pool, "backport", &schema).await.unwrap();
        assert_eq!(
            served["properties"]["release"][SNAPSHOT_KEY]["choices"],
            serde_json::json!(["3.6", "3.7"])
        );
        assert!(served["properties"]["pr"].get(SNAPSHOT_KEY).is_none());
        assert_eq!(record["release"]["run_id"], "r2");
        assert_eq!(record["release"]["digest"], snapshot.digest);
        let params =
            |v: &str| std::collections::BTreeMap::from([("release".to_owned(), v.to_owned())]);
        assert!(crate::playbooks::registry::validate_params(&served, &params("3.7")).is_ok());
        assert!(crate::playbooks::registry::validate_params(&served, &params("3.9")).is_err());

        let (unseeded, record) = attach(&pool, "fresh", &schema).await.unwrap();
        assert_eq!(unseeded, schema);
        assert_eq!(record, serde_json::json!({}));
    }

    #[test]
    fn only_a_closed_snapshot_refuses_a_value() {
        let closed = serde_json::json!({"x-choices-snapshot": {"choices": ["a", "b"]}});
        let open =
            serde_json::json!({"x-allow-other": true, "x-choices-snapshot": {"choices": ["a"]}});
        assert!(outside_snapshot(&closed, "c"));
        assert!(!outside_snapshot(&closed, "a"));
        assert!(!outside_snapshot(&open, "c"));
        assert!(!outside_snapshot(
            &serde_json::json!({"x-choices-from": {}}),
            "c"
        ));
    }
}
