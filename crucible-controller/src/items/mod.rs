//! A playbook's item store (`playbook_items`): labels its runs keep per item, and which run holds
//! each item. A hold is live only while the holding run has an unrevoked, unexpired pod token, so
//! every way a run ends frees its items without a release.

pub(crate) mod api;

use anyhow::{Context, Result};
use crucible_contract::items::{
    ClaimRequest, Conflict, Item, ItemKey, ItemKeys, ItemsError, Labels, ReleaseRequest,
    SetRequest, StoreName,
};
use sqlx::PgPool;
use sqlx::types::Json;

/// One playbook's store: every run of the playbook, registered or draft, shares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scope {
    pub(crate) playbook: String,
    pub(crate) store: StoreName,
}

/// `store` of the playbook `run_id` was launched from; `None` when the run is not a playbook run.
pub(crate) async fn scope_of_run(
    pool: &PgPool,
    run_id: &str,
    store: StoreName,
) -> Result<Option<Scope>> {
    let playbook = sqlx::query_scalar!(
        "SELECT l.playbook FROM runs r JOIN playbook_launches l ON l.key = r.issue
         WHERE r.run_id = $1",
        run_id,
    )
    .fetch_optional(pool)
    .await
    .context("reading the run's playbook")?;
    Ok(playbook.map(|playbook| Scope { playbook, store }))
}

struct Row {
    key: String,
    labels: Json<Labels>,
    held_by: Option<String>,
    live: bool,
    updated_at: Option<String>,
    updated_by: Option<String>,
}

impl Row {
    /// The run holding this item, if its hold is live.
    fn holder(&self) -> Option<&str> {
        self.held_by.as_deref().filter(|_| self.live)
    }

    fn key(&self) -> Result<ItemKey> {
        ItemKey::new(self.key.clone()).with_context(|| format!("stored item key {:?}", self.key))
    }

    fn into_item(self) -> Result<Item> {
        let key = self.key()?;
        let held_by = self.holder().map(str::to_string);
        Ok(Item {
            key,
            labels: self.labels.0,
            held_by,
            updated_at: self.updated_at,
            updated_by: self.updated_by,
        })
    }
}

async fn rows(
    conn: &mut sqlx::PgConnection,
    scope: &Scope,
    keys: &ItemKeys,
    lock: bool,
) -> Result<Vec<Row>> {
    let now = crate::clock::now_rfc3339();
    let keys = keys.sorted();
    let store = scope.store.as_str();
    let rows = match lock {
        true => {
            sqlx::query_as!(
                Row,
                r#"SELECT i.key, i.labels AS "labels: Json<Labels>", i.held_by, i.updated_at,
                      i.updated_by,
                      EXISTS (SELECT 1 FROM pod_tokens t
                              WHERE t.run_id = i.held_by AND t.revoked_at IS NULL
                                AND t.expires_at > $4) AS "live!"
               FROM playbook_items i
               WHERE i.playbook = $1 AND i.store = $2 AND i.key = ANY($3)
               ORDER BY i.key
               FOR UPDATE OF i"#,
                scope.playbook,
                store,
                &keys,
                now,
            )
            .fetch_all(&mut *conn)
            .await
        }
        false => {
            sqlx::query_as!(
                Row,
                r#"SELECT i.key, i.labels AS "labels: Json<Labels>", i.held_by, i.updated_at,
                      i.updated_by,
                      EXISTS (SELECT 1 FROM pod_tokens t
                              WHERE t.run_id = i.held_by AND t.revoked_at IS NULL
                                AND t.expires_at > $4) AS "live!"
               FROM playbook_items i
               WHERE i.playbook = $1 AND i.store = $2 AND i.key = ANY($3)
               ORDER BY i.key"#,
                scope.playbook,
                store,
                &keys,
                now,
            )
            .fetch_all(&mut *conn)
            .await
        }
    };
    rows.context("reading items")
}

/// The items among `keys` that exist and match `selector`.
pub(crate) async fn query(
    pool: &PgPool,
    scope: &Scope,
    request: &crucible_contract::items::QueryRequest,
) -> Result<Vec<Item>> {
    let mut conn = pool.acquire().await.context("acquiring a connection")?;
    rows(&mut conn, scope, &request.keys, false)
        .await?
        .into_iter()
        .filter(|row| {
            request
                .selector
                .as_ref()
                .is_none_or(|s| s.matches(&row.labels.0))
        })
        .map(Row::into_item)
        .collect()
}

/// What a claim or a set did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Done(Vec<Item>),
    Refused(Vec<Conflict>),
}

/// Hold every key in `request` for `run_id`, or none of them.
pub(crate) async fn claim(
    pool: &PgPool,
    scope: &Scope,
    run_id: &str,
    request: &ClaimRequest,
) -> Result<Outcome> {
    let now = crate::clock::now_rfc3339();
    let keys = request.keys.sorted();
    let mut tx = pool.begin().await.context("opening the claim")?;
    sqlx::query!(
        "INSERT INTO playbook_items (playbook, store, key, created_at)
         SELECT $1, $2, k, $4 FROM unnest($3::text[]) AS k ORDER BY k
         ON CONFLICT DO NOTHING",
        scope.playbook,
        scope.store.as_str(),
        &keys,
        now,
    )
    .execute(&mut *tx)
    .await
    .context("recording claimed items")?;
    let rows = rows(&mut tx, scope, &request.keys, true).await?;
    let mut conflicts = Vec::new();
    for row in &rows {
        if let Some(holder) = row.holder().filter(|h| *h != run_id) {
            conflicts.push(Conflict::Held {
                key: row.key()?,
                run: holder.to_string(),
            });
        } else if !request.force
            && request
                .unless
                .as_ref()
                .is_some_and(|s| s.matches(&row.labels.0))
        {
            conflicts.push(Conflict::Matched {
                key: row.key()?,
                labels: row.labels.0.clone(),
            });
        }
    }
    if !conflicts.is_empty() {
        return Ok(Outcome::Refused(conflicts));
    }
    sqlx::query!(
        "UPDATE playbook_items SET held_by = $4
         WHERE playbook = $1 AND store = $2 AND key = ANY($3)",
        scope.playbook,
        scope.store.as_str(),
        &keys,
        run_id,
    )
    .execute(&mut *tx)
    .await
    .context("holding claimed items")?;
    tx.commit().await.context("committing the claim")?;
    rows.into_iter()
        .map(|row| {
            Ok(Item {
                held_by: Some(run_id.to_string()),
                ..row.into_item()?
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Outcome::Done)
}

/// A set refused for something other than a key the run does not hold; it changed nothing.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SetError {
    #[error("item {key}: {source}")]
    Invalid { key: String, source: ItemsError },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// Apply `request`'s edit to every key, all held by `run_id`, or to none.
pub(crate) async fn set(
    pool: &PgPool,
    scope: &Scope,
    run_id: &str,
    request: &SetRequest,
) -> Result<Outcome, SetError> {
    let now = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await.context("opening the set")?;
    let rows = rows(&mut tx, scope, &request.keys, true).await?;
    let held: std::collections::BTreeSet<&str> = rows
        .iter()
        .filter(|row| row.held_by.as_deref() == Some(run_id))
        .map(|row| row.key.as_str())
        .collect();
    let refused: Vec<Conflict> = request
        .keys
        .as_slice()
        .iter()
        .filter(|key| !held.contains(key.as_str()))
        .map(|key| Conflict::NotHeld { key: key.clone() })
        .collect();
    if !refused.is_empty() {
        return Ok(Outcome::Refused(refused));
    }
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let labels = row
            .labels
            .0
            .apply(&request.labels)
            .map_err(|source| SetError::Invalid {
                key: row.key.clone(),
                source,
            })?;
        sqlx::query!(
            "UPDATE playbook_items SET labels = $4, updated_by = $5, updated_at = $6
             WHERE playbook = $1 AND store = $2 AND key = $3",
            scope.playbook,
            scope.store.as_str(),
            row.key,
            Json(&labels) as _,
            run_id,
            now,
        )
        .execute(&mut *tx)
        .await
        .context("writing item labels")?;
        items.push(Item {
            key: row.key()?,
            labels,
            held_by: Some(run_id.to_string()),
            updated_at: Some(now.clone()),
            updated_by: Some(run_id.to_string()),
        });
    }
    tx.commit().await.context("committing the set")?;
    Ok(Outcome::Done(items))
}

/// Drop `run_id`'s holds on `request`'s keys, answering the items it released.
pub(crate) async fn release(
    pool: &PgPool,
    scope: &Scope,
    run_id: &str,
    request: &ReleaseRequest,
) -> Result<Vec<Item>> {
    let keys = request.keys.sorted();
    let rows = sqlx::query!(
        r#"UPDATE playbook_items SET held_by = NULL
           WHERE playbook = $1 AND store = $2 AND key = ANY($3) AND held_by = $4
           RETURNING key, labels AS "labels: Json<Labels>", updated_at, updated_by"#,
        scope.playbook,
        scope.store.as_str(),
        &keys,
        run_id,
    )
    .fetch_all(pool)
    .await
    .context("releasing items")?;
    let mut items: Vec<Item> = rows
        .into_iter()
        .map(|row| {
            Ok(Item {
                key: ItemKey::new(row.key.clone())
                    .with_context(|| format!("stored item key {:?}", row.key))?,
                labels: row.labels.0,
                held_by: None,
                updated_at: row.updated_at,
                updated_by: row.updated_by,
            })
        })
        .collect::<Result<_>>()?;
    items.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(items)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::items::*;
    use crate::runs::pod_token::{Subject, mint, revoke};
    use crucible_contract::items::{LabelEdit, QueryRequest, Selector};
    use std::time::Duration;

    const HOUR: Duration = Duration::from_secs(3600);

    /// A running run of `playbook` with a live pod token; answers the token.
    pub(crate) async fn playbook_run(pool: &PgPool, playbook: &str, run_id: &str) -> String {
        let key = format!("playbook:{playbook}:{run_id}");
        sqlx::query(
            "INSERT INTO issues (key, repo, tier, status, priority, input_kind, title, updated_at)
             VALUES ($1, 'owner/repo', 'T1', 'running', 0, 'playbook', 'items',
                     '2026-10-28T00:00:00Z')",
        )
        .bind(&key)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO playbook_launches (key, playbook, params, schema_digest, max_cost,
                                            max_time, created_at)
             VALUES ($1, $2, '{}'::jsonb, 'sha256:x', 1.0, '5m', '2026-10-28T00:00:00Z')",
        )
        .bind(&key)
        .bind(playbook)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO runs (run_id, status, issue) VALUES ($1, 'running', $2)")
            .bind(run_id)
            .bind(&key)
            .execute(pool)
            .await
            .unwrap();
        mint(pool, &Subject::Run(run_id.to_string()), None, HOUR)
            .await
            .unwrap()
    }

    fn scope(playbook: &str, store: &str) -> Scope {
        Scope {
            playbook: playbook.into(),
            store: store.parse().unwrap(),
        }
    }

    fn keys(raw: &[&str]) -> ItemKeys {
        ItemKeys::new(raw.iter().map(|k| k.parse().unwrap()).collect()).unwrap()
    }

    fn key(raw: &str) -> ItemKey {
        raw.parse().unwrap()
    }

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        Labels::new(
            pairs
                .iter()
                .map(|(n, v)| (n.parse().unwrap(), v.parse().unwrap()))
                .collect(),
        )
        .unwrap()
    }

    fn edit(json: &str) -> LabelEdit {
        serde_json::from_str(json).unwrap()
    }

    fn claim_of(raw: &[&str], unless: Option<&str>, force: bool) -> ClaimRequest {
        ClaimRequest {
            keys: keys(raw),
            unless: unless.map(|s| s.parse::<Selector>().unwrap()),
            force,
        }
    }

    async fn claim_ok(pool: &PgPool, scope: &Scope, run: &str, raw: &[&str]) -> Vec<Item> {
        match claim(pool, scope, run, &claim_of(raw, None, false))
            .await
            .unwrap()
        {
            Outcome::Done(items) => items,
            Outcome::Refused(c) => panic!("{run} could not claim {raw:?}: {c:?}"),
        }
    }

    async fn set_ok(pool: &PgPool, scope: &Scope, run: &str, raw: &[&str], json: &str) {
        let request = SetRequest {
            keys: keys(raw),
            labels: edit(json),
        };
        match set(pool, scope, run, &request).await.unwrap() {
            Outcome::Done(_) => {}
            Outcome::Refused(c) => panic!("{run} could not set {raw:?}: {c:?}"),
        }
    }

    async fn read(pool: &PgPool, scope: &Scope, raw: &[&str]) -> Vec<Item> {
        query(
            pool,
            scope,
            &QueryRequest {
                keys: keys(raw),
                selector: None,
            },
        )
        .await
        .unwrap()
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_run_resolves_to_the_store_of_the_playbook_it_was_launched_from(pool: PgPool) {
        playbook_run(&pool, "triage", "run-a").await;
        assert_eq!(
            scope_of_run(&pool, "run-a", "tickets".parse().unwrap())
                .await
                .unwrap(),
            Some(scope("triage", "tickets"))
        );
        sqlx::query("INSERT INTO runs (run_id, status) VALUES ('loose', 'running')")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            scope_of_run(&pool, "loose", "tickets".parse().unwrap())
                .await
                .unwrap(),
            None,
            "a run with no playbook launch has no store"
        );
        assert_eq!(
            scope_of_run(&pool, "nope", "tickets".parse().unwrap())
                .await
                .unwrap(),
            None
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_claim_takes_every_key_or_none(pool: PgPool) {
        let s = scope("triage", "tickets");
        playbook_run(&pool, "triage", "run-a").await;
        playbook_run(&pool, "triage", "run-b").await;
        let held = claim_ok(&pool, &s, "run-a", &["K-1"]).await;
        assert_eq!(held[0].held_by.as_deref(), Some("run-a"));
        assert_eq!(
            claim(&pool, &s, "run-b", &claim_of(&["K-1", "K-2"], None, false))
                .await
                .unwrap(),
            Outcome::Refused(vec![Conflict::Held {
                key: key("K-1"),
                run: "run-a".into()
            }])
        );
        assert!(
            read(&pool, &s, &["K-2"]).await.is_empty(),
            "a refused claim records no item"
        );
        let taken = claim_ok(&pool, &s, "run-b", &["K-2"]).await;
        assert_eq!(taken[0].held_by.as_deref(), Some("run-b"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn reclaiming_a_held_key_is_a_no_op_for_its_holder(pool: PgPool) {
        let s = scope("triage", "tickets");
        playbook_run(&pool, "triage", "run-a").await;
        claim_ok(&pool, &s, "run-a", &["K-1"]).await;
        let again = claim_ok(&pool, &s, "run-a", &["K-1", "K-2"]).await;
        assert_eq!(
            again
                .iter()
                .map(|i| (i.key.as_str(), i.held_by.as_deref()))
                .collect::<Vec<_>>(),
            [("K-1", Some("run-a")), ("K-2", Some("run-a"))]
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn unless_refuses_matching_keys_and_force_skips_it_but_not_a_live_hold(pool: PgPool) {
        let s = scope("triage", "tickets");
        playbook_run(&pool, "triage", "run-a").await;
        playbook_run(&pool, "triage", "run-b").await;
        claim_ok(&pool, &s, "run-a", &["K-1", "K-2"]).await;
        set_ok(&pool, &s, "run-a", &["K-1"], r#"{"triaged": ""}"#).await;
        release(
            &pool,
            &s,
            "run-a",
            &ReleaseRequest {
                keys: keys(&["K-1"]),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            claim(
                &pool,
                &s,
                "run-b",
                &claim_of(&["K-1"], Some("triaged"), false)
            )
            .await
            .unwrap(),
            Outcome::Refused(vec![Conflict::Matched {
                key: key("K-1"),
                labels: labels(&[("triaged", "")])
            }])
        );
        assert_eq!(
            claim(
                &pool,
                &s,
                "run-b",
                &claim_of(&["K-2"], Some("triaged"), true)
            )
            .await
            .unwrap(),
            Outcome::Refused(vec![Conflict::Held {
                key: key("K-2"),
                run: "run-a".into()
            }]),
            "force never takes a live run's hold"
        );
        let forced = match claim(
            &pool,
            &s,
            "run-b",
            &claim_of(&["K-1"], Some("triaged"), true),
        )
        .await
        .unwrap()
        {
            Outcome::Done(items) => items,
            refused => panic!("{refused:?}"),
        };
        assert_eq!(forced[0].labels, labels(&[("triaged", "")]));
        assert_eq!(forced[0].held_by.as_deref(), Some("run-b"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_hold_ends_when_its_runs_token_is_revoked_or_expires(pool: PgPool) {
        let s = scope("triage", "tickets");
        playbook_run(&pool, "triage", "run-a").await;
        playbook_run(&pool, "triage", "run-b").await;
        playbook_run(&pool, "triage", "run-c").await;
        claim_ok(&pool, &s, "run-a", &["K-1"]).await;
        revoke(&pool, &Subject::Run("run-a".into())).await.unwrap();
        assert_eq!(read(&pool, &s, &["K-1"]).await[0].held_by, None);
        claim_ok(&pool, &s, "run-b", &["K-1"]).await;

        sqlx::query(
            "UPDATE pod_tokens SET expires_at = '2026-01-01T00:00:00Z' WHERE run_id = 'run-b'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(read(&pool, &s, &["K-1"]).await[0].held_by, None);
        let taken = claim_ok(&pool, &s, "run-c", &["K-1"]).await;
        assert_eq!(taken[0].held_by.as_deref(), Some("run-c"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_set_needs_a_hold_on_every_key_and_changes_none_otherwise(pool: PgPool) {
        let s = scope("triage", "tickets");
        playbook_run(&pool, "triage", "run-a").await;
        playbook_run(&pool, "triage", "run-b").await;
        claim_ok(&pool, &s, "run-a", &["K-1"]).await;
        claim_ok(&pool, &s, "run-b", &["K-2"]).await;
        let request = SetRequest {
            keys: keys(&["K-1", "K-2", "K-3"]),
            labels: edit(r#"{"triaged": ""}"#),
        };
        assert_eq!(
            set(&pool, &s, "run-a", &request).await.unwrap(),
            Outcome::Refused(vec![
                Conflict::NotHeld { key: key("K-2") },
                Conflict::NotHeld { key: key("K-3") },
            ])
        );
        assert_eq!(read(&pool, &s, &["K-1"]).await[0].labels, Labels::default());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn labels_merge_across_sets_and_outlive_the_run_that_wrote_them(pool: PgPool) {
        let s = scope("triage", "tickets");
        playbook_run(&pool, "triage", "run-a").await;
        claim_ok(&pool, &s, "run-a", &["K-1"]).await;
        set_ok(
            &pool,
            &s,
            "run-a",
            &["K-1"],
            r#"{"pending": "", "verdict": "error"}"#,
        )
        .await;
        set_ok(
            &pool,
            &s,
            "run-a",
            &["K-1"],
            r#"{"pending": null, "verdict": "not_affected", "triaged": ""}"#,
        )
        .await;
        revoke(&pool, &Subject::Run("run-a".into())).await.unwrap();
        let item = read(&pool, &s, &["K-1"]).await.remove(0);
        assert_eq!(
            item.labels,
            labels(&[("triaged", ""), ("verdict", "not_affected")])
        );
        assert_eq!(item.held_by, None);
        assert_eq!(item.updated_by.as_deref(), Some("run-a"));
        assert!(item.updated_at.is_some());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_set_past_the_label_cap_changes_nothing(pool: PgPool) {
        let s = scope("triage", "tickets");
        playbook_run(&pool, "triage", "run-a").await;
        claim_ok(&pool, &s, "run-a", &["K-1", "K-2"]).await;
        let full: String = (0..crucible_contract::items::MAX_LABELS)
            .map(|i| format!(r#""l{i}": """#))
            .collect::<Vec<_>>()
            .join(",");
        set_ok(&pool, &s, "run-a", &["K-2"], &format!("{{{full}}}")).await;
        let request = SetRequest {
            keys: keys(&["K-1", "K-2"]),
            labels: edit(r#"{"extra": "x"}"#),
        };
        let err = set(&pool, &s, "run-a", &request).await.unwrap_err();
        assert!(
            matches!(err, SetError::Invalid { ref key, .. } if key == "K-2"),
            "{err}"
        );
        assert_eq!(read(&pool, &s, &["K-1"]).await[0].labels, Labels::default());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn release_drops_only_the_callers_own_holds(pool: PgPool) {
        let s = scope("triage", "tickets");
        playbook_run(&pool, "triage", "run-a").await;
        playbook_run(&pool, "triage", "run-b").await;
        claim_ok(&pool, &s, "run-a", &["K-1"]).await;
        claim_ok(&pool, &s, "run-b", &["K-2"]).await;
        let released = release(
            &pool,
            &s,
            "run-a",
            &ReleaseRequest {
                keys: keys(&["K-1", "K-2", "K-3"]),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            released.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["K-1"]
        );
        let after = read(&pool, &s, &["K-1", "K-2"]).await;
        assert_eq!(after[0].held_by, None);
        assert_eq!(after[1].held_by.as_deref(), Some("run-b"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_query_omits_unwritten_keys_and_filters_by_selector(pool: PgPool) {
        let s = scope("triage", "tickets");
        playbook_run(&pool, "triage", "run-a").await;
        claim_ok(&pool, &s, "run-a", &["K-1", "K-2"]).await;
        set_ok(
            &pool,
            &s,
            "run-a",
            &["K-1"],
            r#"{"triaged": "", "verdict": "affected"}"#,
        )
        .await;
        let all = read(&pool, &s, &["K-1", "K-2", "K-9"]).await;
        assert_eq!(
            all.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["K-1", "K-2"]
        );
        let untriaged = query(
            &pool,
            &s,
            &QueryRequest {
                keys: keys(&["K-1", "K-2", "K-9"]),
                selector: Some("!triaged".parse().unwrap()),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            untriaged.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["K-2"]
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn stores_and_playbooks_do_not_share_items(pool: PgPool) {
        playbook_run(&pool, "triage", "run-a").await;
        playbook_run(&pool, "audit", "run-b").await;
        let tickets = scope("triage", "tickets");
        claim_ok(&pool, &tickets, "run-a", &["K-1"]).await;
        set_ok(&pool, &tickets, "run-a", &["K-1"], r#"{"triaged": ""}"#).await;
        for other in [scope("triage", "shadow"), scope("audit", "tickets")] {
            assert!(read(&pool, &other, &["K-1"]).await.is_empty(), "{other:?}");
        }
        let audit = claim_ok(&pool, &scope("audit", "tickets"), "run-b", &["K-1"]).await;
        assert_eq!(audit[0].labels, Labels::default());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn concurrent_overlapping_claims_admit_exactly_one(pool: PgPool) {
        let s = scope("triage", "tickets");
        let runs: Vec<String> = (0..8).map(|i| format!("run-{i}")).collect();
        for run in &runs {
            playbook_run(&pool, "triage", run).await;
        }
        let mut tasks = Vec::new();
        for (i, run) in runs.iter().enumerate() {
            let (pool, s, run) = (pool.clone(), s.clone(), run.clone());
            let mut order = vec!["K-1", "K-2", "K-3", "K-4"];
            order.rotate_left(i % 4);
            tasks.push(tokio::spawn(async move {
                claim(&pool, &s, &run, &claim_of(&order, None, false))
                    .await
                    .unwrap()
            }));
        }
        let mut won = 0;
        for task in tasks {
            if matches!(task.await.unwrap(), Outcome::Done(_)) {
                won += 1;
            }
        }
        assert_eq!(won, 1);
        let holders: std::collections::BTreeSet<Option<String>> =
            read(&pool, &s, &["K-1", "K-2", "K-3", "K-4"])
                .await
                .into_iter()
                .map(|i| i.held_by)
                .collect();
        assert_eq!(holders.len(), 1, "one run holds all four: {holders:?}");
    }
}
