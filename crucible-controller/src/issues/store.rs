//! Raw SQL over the issue, scope, comment and adoption tables.

use crate::issues::model::{
    AwaitingApproval, InputKind, Issue, IssueComment, IssueQuery, NewIssue, NewScope, RerankScope,
    Scope, SortKey, UpstreamState,
};

use crate::model::{ParkReason, ParkedBy, SortDir, Status};

use anyhow::{Context, Result};

use crucible_contract::Tier;

use sqlx::{PgExecutor, PgPool, Row};

use std::collections::HashMap;

/// Serialize a label set for the `issues.labels` TEXT column: a serde_json array string, or NULL
/// for an empty set (indistinguishable from "not yet backfilled", which is fine — both render and
/// filter as "no labels").
fn labels_to_json(labels: &[String]) -> Result<Option<String>> {
    if labels.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        serde_json::to_string(labels).context("serializing issue labels")?,
    ))
}

/// Decode the `issues.labels` column back into the label set; NULL reads as empty.
pub(crate) fn labels_from_json(raw: Option<String>) -> Result<Vec<String>> {
    match raw {
        Some(s) => serde_json::from_str(&s).context("decoding issue labels"),
        None => Ok(Vec::new()),
    }
}

/// Insert an issue (entering at `new`, tier `NULL`) or, if already tracked, refresh its discovery
/// metadata — repo, evidence, title, author, body, labels, `updated_at`, `upstream_updated_at`.
/// The `ON CONFLICT` deliberately omits `status`, the park fields, `tier`, and `priority`: triage
/// is pure discovery — it never resurrects or freezes a row, never guesses or clobbers a tier the
/// ranker already set, and never resets a priority a human bumped.
#[tracing::instrument(name = "db.upsert_issue", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %iss.key), err)]
pub async fn upsert_issue(ex: impl PgExecutor<'_>, iss: &NewIssue) -> Result<()> {
    let updated_at = crate::clock::now_rfc3339();
    let labels = labels_to_json(&iss.labels)?;
    let upstream_updated_at = iss.upstream_updated_at.as_deref();
    sqlx::query!(
        r#"
        INSERT INTO issues (key, repo, status, priority, evidence_url, title, author, body, labels, updated_at, upstream_updated_at)
        VALUES ($1, $2, 'new', $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT(key) DO UPDATE SET
            repo                 = excluded.repo,
            evidence_url         = excluded.evidence_url,
            title                = excluded.title,
            author               = excluded.author,
            body                 = excluded.body,
            labels               = excluded.labels,
            updated_at           = excluded.updated_at,
            upstream_updated_at  = excluded.upstream_updated_at
        "#,
        iss.key,
        iss.repo,
        iss.priority,
        iss.evidence_url,
        iss.title,
        iss.author,
        iss.body,
        labels,
        updated_at,
        upstream_updated_at,
    )
    .execute(ex)
    .await
    .context("upsert_issue")?;
    Ok(())
}

/// Atomic compare-and-set on `status`: flip `from` → `to` for `key`, only if the row is still at
/// `from`. Returns whether this call won the claim. One UPDATE, so two racers cannot both succeed
/// (a single UPDATE is atomic under Postgres row locking).
#[tracing::instrument(name = "db.claim_issue", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub async fn claim_issue(
    ex: impl PgExecutor<'_>,
    key: &str,
    from: Status,
    to: Status,
) -> Result<bool> {
    let (to_s, from_s) = (to.as_str(), from.as_str());
    let res = sqlx::query!(
        "UPDATE issues SET status = $1 WHERE key = $2 AND status = $3",
        to_s,
        key,
        from_s,
    )
    .execute(ex)
    .await
    .context("claim_issue")?;
    Ok(res.rows_affected() == 1)
}

/// Park an issue: set `status = 'parked'` with a reason and the parking authority (machine|human).
/// Stamps `pre_park_status` with the status being parked from (the UPDATE's RHS reads the old row)
/// so [`unpark_issue`] restores the issue's pipeline position instead of demoting it to `new`.
/// A re-park of an already-parked row keeps the original stamp (never overwrites it with `parked`).
#[tracing::instrument(name = "db.park_issue", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn park_issue(
    ex: impl PgExecutor<'_>,
    key: &str,
    reason: &str,
    parked_by: ParkedBy,
    updated_at: &str,
) -> Result<()> {
    let by = parked_by.as_str();
    sqlx::query!(
        "UPDATE issues SET \
         pre_park_status = CASE WHEN status = 'parked' THEN pre_park_status ELSE status END, \
         status = 'parked', parked_reason = $1, parked_by = $2, updated_at = $3 WHERE key = $4",
        reason,
        by,
        updated_at,
        key,
    )
    .execute(ex)
    .await
    .context("park_issue")?;
    Ok(())
}

/// Fetch one issue by key, decoded into strong types, or `None` if untracked.
#[tracing::instrument(name = "db.get_issue", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub async fn get_issue(ex: impl PgExecutor<'_>, key: &str) -> Result<Option<Issue>> {
    let sql = format!("SELECT {ISSUE_COLS} FROM issues WHERE key = $1");
    sqlx::query(&sql)
        .bind(key)
        .fetch_optional(ex)
        .await
        .context("get_issue")?
        .as_ref()
        .map(issue_from_row)
        .transpose()
}

/// List issues under the full `status=`/`tier=`/`repo=`/`label=`/`upstream=`/`upstream_since=`
/// filter + `sort=`/`dir=` set
/// (shared by the `/` UI page and `GET /api/issues` via one query builder so the two never
/// drift). The sort column comes from a [`SortKey`] match, never string-interpolated, so this
/// stays injection-safe however the query params are threaded through; every filter value is a
/// bound `?` parameter.
#[tracing::instrument(name = "db.list_issues_filtered", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_issues_filtered(
    ex: impl PgExecutor<'_>,
    q: &IssueQuery,
) -> Result<Vec<Issue>> {
    let order_col = match q.sort {
        SortKey::Updated => "updated_at",
        SortKey::Upstream => "upstream_updated_at",
        SortKey::Tier => "tier",
        SortKey::Priority => "priority",
        SortKey::Title => "title",
    };
    // NULLS placement pinned to the SQLite-era behavior the UI expects (NULLs sort as
    // smallest: first ASC, last DESC); Postgres's default is the opposite on DESC.
    let order_dir = match q.dir {
        SortDir::Asc => "ASC NULLS FIRST",
        SortDir::Desc => "DESC NULLS LAST",
    };
    // Upstream open/closed is derived, not stored: a closed upstream issue is always retired to
    // the `(parked, "upstream closed")` shape (and untracked closed issues never get a row), so
    // that shape IS the closed set and its complement is the open set.
    let upstream_clause = match q.upstream {
        None => "",
        Some(UpstreamState::Closed) => "AND (status = 'parked' AND parked_reason = $9)",
        Some(UpstreamState::Open) => "AND NOT (status = 'parked' AND parked_reason = $9)",
    };
    // `upstream_since` is an *upstream* activity window, and only kinds with an upstream ever get
    // an `upstream_updated_at` — an adopted scenario/jira row is NULL there forever. Applying the
    // window to those rows hid every adopted item behind the page's default `recency=1y`, which is
    // what made the kind=scenario filter look like it matched nothing. Rows of an upstream-bearing
    // kind with no stamp yet (pre-backfill relics) stay excluded, as before.
    //
    // The tag list is a compile-time constant of our own spellings, so interpolating it is safe;
    // every caller-supplied value below is still a bound parameter.
    let upstream_tags = InputKind::UPSTREAM_TAGS
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(", ");
    // `key` as the tiebreak keeps the order fully deterministic (ties on the sort column are
    // otherwise implementation-defined). This is a plain `sqlx::query` (not the `query!`
    // macro, since the ORDER BY column is only known at runtime), so column aliases are plain SQL
    // — no `"col!"` not-null override syntax, that's a `query!`-macro-only annotation.
    //
    // The label filter is a LIKE substring match against the serde_json-serialized `labels` text
    // (the bound pattern is `%"<label>"%`, quotes included, so partial label names don't match).
    // Good enough for GitHub label names; a label containing `"`, `%`, `_`, or `\` would need
    // json_each instead.
    let sql = format!(
        r#"
        SELECT {ISSUE_COLS}
        FROM issues
        WHERE ($1 IS NULL OR status = $1)
          AND ($2 IS NULL OR tier = $2)
          AND ($3 IS NULL OR repo = $3)
          AND ($4 IS NULL OR labels LIKE $4)
          AND ($5 IS NULL OR input_kind = $5)
          AND ($6 IS NULL OR upstream_updated_at >= $6 OR input_kind NOT IN ({upstream_tags}))
          AND ($7 IS NULL OR affinity = $7)
          AND ($8::text IS NULL OR input_kind <> $8)
          {upstream_clause}
        ORDER BY {order_col} {order_dir}, key ASC
        "#
    );
    let status = q.status.map(|s| s.as_str());
    let label_pattern = q.label.as_ref().map(|l| format!("%\"{l}\"%"));
    let mut query = sqlx::query(&sql)
        .bind(status)
        .bind(&q.tier)
        .bind(&q.repo)
        .bind(&label_pattern)
        .bind(q.kind.map(|k| k.tag()))
        .bind(&q.upstream_since)
        .bind(&q.affinity)
        .bind(q.exclude_kind.map(|k| k.tag()));
    if q.upstream.is_some() {
        query = query.bind(ParkReason::UpstreamClosed.to_string());
    }
    let rows = query.fetch_all(ex).await.context("list_issues_filtered")?;
    rows.iter().map(issue_from_row).collect()
}

/// The `issues` column list, in decode order. Every `issues` read that yields an [`Issue`] builds
/// its SELECT from this so a new column is a one-line edit here plus [`issue_from_row`].
pub(crate) const ISSUE_COLS: &str = "key, repo, input_kind, tier, status, priority, evidence_url, \
     parked_reason, parked_by, updated_at, ranked_content_hash, grounded_content_hash, title, \
     author, body, labels, upstream_updated_at, scope_now_justification, scope_now_max_cost, \
     redispatch_justification, git_ref, codegen_contract, affinity, dispatch_target, \
     agent_provider, agent_model";

/// Decode one `issues` row into strong types. Shared by every `issues` read; these use
/// `sqlx::query` (not the `query!` macro), so a new column needs no `.sqlx` cache entry, at the
/// cost of compile-time column checks on these static reads.
pub(crate) fn issue_from_row(row: &sqlx::postgres::PgRow) -> Result<Issue> {
    let key: String = row.try_get("key")?;
    let input_kind: String = row.try_get("input_kind")?;
    Ok(Issue {
        kind: InputKind::from_parts(&input_kind, &key),
        key,
        repo: row.try_get("repo")?,
        tier: row.try_get("tier")?,
        status: row.try_get("status")?,
        priority: row.try_get("priority")?,
        evidence_url: row.try_get("evidence_url")?,
        parked_reason: row.try_get("parked_reason")?,
        parked_by: row.try_get("parked_by")?,
        updated_at: row.try_get("updated_at")?,
        ranked_content_hash: row.try_get("ranked_content_hash")?,
        grounded_content_hash: row.try_get("grounded_content_hash")?,
        title: row.try_get("title")?,
        author: row.try_get("author")?,
        body: row.try_get("body")?,
        labels: labels_from_json(row.try_get("labels")?)?,
        upstream_updated_at: row.try_get("upstream_updated_at")?,
        scope_now_justification: row.try_get("scope_now_justification")?,
        scope_now_max_cost: row.try_get("scope_now_max_cost")?,
        redispatch_justification: row.try_get("redispatch_justification")?,
        git_ref: row.try_get("git_ref")?,
        codegen_contract: row.try_get("codegen_contract")?,
        affinity: row.try_get("affinity")?,
        dispatch_target: row.try_get("dispatch_target")?,
        agent_provider: row.try_get("agent_provider")?,
        agent_model: row.try_get("agent_model")?,
    })
}

/// Record the cluster this issue's work dispatches onto. Written by the launch that chose it,
/// from the set that launcher was authorized for; NULL means the controller's configured default.
#[tracing::instrument(name = "db.set_dispatch_target", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue_key = %issue_key), err)]
pub(crate) async fn set_dispatch_target(
    ex: impl PgExecutor<'_>,
    issue_key: &str,
    target: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE issues SET dispatch_target = $2 WHERE key = $1")
        .bind(issue_key)
        .bind(target)
        .execute(ex)
        .await
        .context("set_dispatch_target")?;
    Ok(())
}

/// The cluster an issue's work was pinned to at launch, or `None` for the controller's configured
/// default. Read at each dispatch rather than carried on the spec: every dispatch site already has
/// the issue key, and late resolution is what lets an operator repoint the default.
#[tracing::instrument(name = "db.dispatch_target", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue_key = %issue_key), err)]
pub(crate) async fn dispatch_target(
    ex: impl PgExecutor<'_>,
    issue_key: &str,
) -> Result<DispatchRouting> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT dispatch_target, codegen_contract FROM issues WHERE key = $1")
            .bind(issue_key)
            .fetch_optional(ex)
            .await
            .context("dispatch_target")?;
    let (target, contract) = row.unwrap_or((None, None));
    Ok(DispatchRouting { target, contract })
}

/// Record the model provider and model this issue's dispatches run against. Written by the launch
/// that chose them; NULL is "resolve the defaults at dispatch time", which an empty provider
/// registry answers with nothing at all. A model without a provider is refused at the API, so the
/// pair is written together.
#[tracing::instrument(name = "db.set_agent_dispatch", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue_key = %issue_key), err)]
pub(crate) async fn set_agent_dispatch(
    ex: impl PgExecutor<'_>,
    issue_key: &str,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE issues SET agent_provider = $2, agent_model = $3 WHERE key = $1")
        .bind(issue_key)
        .bind(provider)
        .bind(model)
        .execute(ex)
        .await
        .context("set_agent_dispatch")?;
    Ok(())
}

/// What an issue offers the dispatch-cluster decision: the target its launch pinned, and the
/// contract it is measured under. Both `None` is an issue that never chose and is not GPU-measured,
/// which takes the controller's configured default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchRouting {
    pub(crate) target: Option<String>,
    pub(crate) contract: Option<String>,
}

/// Mirror one issue's upstream comment set: upsert each comment by GitHub id and delete rows
/// whose comment vanished upstream, in one transaction (a reader never sees a half-replaced
/// set). Takes the pool (not a generic executor) because it owns the transaction.
#[tracing::instrument(name = "db.replace_issue_comments", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue_key = %issue_key), err)]
pub(crate) async fn replace_issue_comments(
    pool: &sqlx::PgPool,
    issue_key: &str,
    comments: &[IssueComment],
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("replace_issue_comments: begin")?;
    for c in comments {
        sqlx::query!(
            r#"
            INSERT INTO issue_comments (id, issue_key, author, created_at, updated_at, body)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT(id) DO UPDATE SET
                issue_key  = excluded.issue_key,
                author     = excluded.author,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at,
                body       = excluded.body
            "#,
            c.id,
            issue_key,
            c.author,
            c.created_at,
            c.updated_at,
            c.body,
        )
        .execute(&mut *tx)
        .await
        .context("replace_issue_comments: upsert")?;
    }
    // The vanished set: everything not in the surviving ids (bound as one array parameter).
    let keep_ids: Vec<i64> = comments.iter().map(|c| c.id).collect();
    sqlx::query!(
        "DELETE FROM issue_comments
         WHERE issue_key = $1 AND id <> ALL($2)",
        issue_key,
        &keep_ids,
    )
    .execute(&mut *tx)
    .await
    .context("replace_issue_comments: delete vanished")?;
    tx.commit().await.context("replace_issue_comments: commit")
}

/// One issue's mirrored comments, oldest first (GitHub creation order; id breaks the rare
/// same-second tie).
#[tracing::instrument(name = "db.list_issue_comments", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue_key = %issue_key), err)]
pub(crate) async fn list_issue_comments(
    ex: impl PgExecutor<'_>,
    issue_key: &str,
) -> Result<Vec<IssueComment>> {
    let rows = sqlx::query!(
        r#"
        SELECT id AS "id!", issue_key AS "issue_key!", author, created_at, updated_at, body
        FROM issue_comments WHERE issue_key = $1 ORDER BY created_at, id
        "#,
        issue_key,
    )
    .fetch_all(ex)
    .await
    .context("list_issue_comments")?;
    Ok(rows
        .into_iter()
        .map(|r| IssueComment {
            id: r.id,
            issue_key: r.issue_key,
            author: r.author,
            created_at: r.created_at,
            updated_at: r.updated_at,
            body: r.body,
        })
        .collect())
}

/// Every scope proposed for one issue, oldest first (the provenance chain the issue-detail view
/// walks: issue → scopes → runs → candidates).
#[tracing::instrument(name = "db.list_scopes_for_issue", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue = %issue), err)]
pub(crate) async fn list_scopes_for_issue(
    ex: impl PgExecutor<'_>,
    issue: &str,
) -> Result<Vec<Scope>> {
    let sql = format!("SELECT {SCOPE_COLS} FROM scopes WHERE issue = $1 ORDER BY id");
    let rows = sqlx::query(&sql)
        .bind(issue)
        .fetch_all(ex)
        .await
        .context("list_scopes_for_issue")?;
    rows.iter().map(decode_scope).collect()
}

/// The `scopes` column list, in decode order. Every `scopes` read builds its SELECT from this so a
/// new column is a one-line edit here plus [`decode_scope`].
const SCOPE_COLS: &str = "id, issue, pack_digest, check_outcome, stale, approval_pr, approved_by, \
     approved_at, frozen_issue_hash, stale_comment_id, exposure, exposure_digest, \
     approved_exposure_digest";

/// Decode one `scopes` row. Shared by every `scopes` read; these use `sqlx::query` (not the
/// `query!` macro), so a new column needs no `.sqlx` cache entry, at the cost of compile-time
/// column checks on these static reads.
fn decode_scope(r: &sqlx::postgres::PgRow) -> Result<Scope> {
    Ok(Scope {
        id: r.try_get("id")?,
        issue: r.try_get("issue")?,
        pack_digest: r.try_get("pack_digest")?,
        check_outcome: r.try_get("check_outcome")?,
        stale: r.try_get("stale")?,
        approval_pr: r.try_get("approval_pr")?,
        approved_by: r.try_get("approved_by")?,
        approved_at: r.try_get("approved_at")?,
        frozen_issue_hash: r.try_get("frozen_issue_hash")?,
        stale_comment_id: r.try_get("stale_comment_id")?,
        exposure: r
            .try_get::<Option<serde_json::Value>, _>("exposure")?
            .map(crate::playbooks::exposure::Exposure::from_value)
            .transpose()?,
        exposure_digest: r.try_get("exposure_digest")?,
        approved_exposure_digest: r.try_get("approved_exposure_digest")?,
    })
}

/// One `scopes` row by id, or `None` if unknown — the approval evidence endpoint's lookup
/// (`GET /api/approvals/{scope_id}/evidence`) needs the owning issue key to reconstruct the pack path.
#[tracing::instrument(name = "db.get_scope_by_id", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", id = id), err)]
pub(crate) async fn get_scope_by_id(ex: impl PgExecutor<'_>, id: i64) -> Result<Option<Scope>> {
    let sql = format!("SELECT {SCOPE_COLS} FROM scopes WHERE id = $1");
    sqlx::query(&sql)
        .bind(id)
        .fetch_optional(ex)
        .await
        .context("get_scope_by_id")?
        .as_ref()
        .map(decode_scope)
        .transpose()
}

/// Apply a ranking verdict's tier: a compare-and-set on the *ranking* state (not
/// `status`), so two reconciles racing the same issue can't both win — it only applies if the
/// row's cached hash is stale relative to `ranked_content_hash` (never-ranked, or content
/// changed since the last verdict). Returns rows changed (0/1): only the winner should log the
/// rationale as evidence and ledger the call's cost.
#[tracing::instrument(name = "db.apply_rank_result", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key, tier = %tier), err)]
pub(crate) async fn apply_rank_result(
    ex: impl PgExecutor<'_>,
    key: &str,
    tier: &str,
    affinity: &str,
    ranked_content_hash: &str,
) -> Result<bool> {
    let updated_at = crate::clock::now_rfc3339();
    let res = sqlx::query!(
        r#"
        UPDATE issues SET tier = $1, affinity = $2, ranked_content_hash = $3, updated_at = $4
        WHERE key = $5 AND (ranked_content_hash IS NULL OR ranked_content_hash <> $6)
        "#,
        tier,
        affinity,
        ranked_content_hash,
        updated_at,
        key,
        ranked_content_hash,
    )
    .execute(ex)
    .await
    .context("apply_rank_result")?;
    Ok(res.rows_affected() == 1)
}

/// Stamp the tier/ranking-cache columns unconditionally. The follow-up half of an N-verdict park
/// ([`crate::issues::reconcile`]): the caller already won the `status` CAS via `claim_park`, so nothing
/// else can be racing this row's tier fields at this point.
#[tracing::instrument(name = "db.set_ranked_tier", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key, tier = %tier), err)]
pub(crate) async fn set_ranked_tier(
    ex: impl PgExecutor<'_>,
    key: &str,
    tier: &str,
    affinity: &str,
    ranked_content_hash: &str,
) -> Result<()> {
    let updated_at = crate::clock::now_rfc3339();
    sqlx::query!(
        "UPDATE issues SET tier = $1, affinity = $2, ranked_content_hash = $3, updated_at = $4          WHERE key = $5",
        tier,
        affinity,
        ranked_content_hash,
        updated_at,
        key,
    )
    .execute(ex)
    .await
    .context("set_ranked_tier")?;
    Ok(())
}

/// Force a re-rank of one issue: NULL the rank cache (`ranked_content_hash`) so the next sweep's
/// `confirm_tier` misses it and re-ranks. The standing `tier` is deliberately left in place — the
/// tier gate keeps working off the old verdict until the fresh one lands. Returns rows changed
/// (0 = unknown key).
#[tracing::instrument(name = "db.clear_rank", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn clear_rank(ex: impl PgExecutor<'_>, key: &str) -> Result<bool> {
    let updated_at = crate::clock::now_rfc3339();
    let res = sqlx::query!(
        "UPDATE issues SET ranked_content_hash = NULL, updated_at = $1 WHERE key = $2",
        updated_at,
        key,
    )
    .execute(ex)
    .await
    .context("clear_rank")?;
    Ok(res.rows_affected() == 1)
}

/// The bulk [`clear_rank`]: clear the rank cache for every `new` issue a [`RerankScope`] matches
/// (only `new` rows ever reach `confirm_tier`, so anything else would be a silent no-op). Returns
/// how many rows will rank fresh on the next sweep — for [`RerankScope::Unranked`] that's rows
/// that were already due (their cache is NULL after a rank failure), so the count is a match
/// count, not a cache-invalidation count.
#[tracing::instrument(name = "db.clear_rank_bulk", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn clear_rank_bulk(ex: impl PgExecutor<'_>, scope: RerankScope) -> Result<u64> {
    let updated_at = crate::clock::now_rfc3339();
    let res = match scope {
        RerankScope::All => {
            sqlx::query!(
                "UPDATE issues SET ranked_content_hash = NULL, updated_at = $1 WHERE status = 'new'",
                updated_at,
            )
            .execute(ex)
            .await
        }
        RerankScope::Tier(t) => {
            let tier = t.as_str();
            sqlx::query!(
                "UPDATE issues SET ranked_content_hash = NULL, updated_at = $1 WHERE status = 'new' AND tier = $2",
                updated_at,
                tier,
            )
            .execute(ex)
            .await
        }
        RerankScope::Unranked => {
            sqlx::query!(
                "UPDATE issues SET ranked_content_hash = NULL, updated_at = $1 WHERE status = 'new' AND tier IS NULL",
                updated_at,
            )
            .execute(ex)
            .await
        }
    }
    .context("clear_rank_bulk")?;
    Ok(res.rows_affected())
}

/// Apply the pre-scope grounded gate's verdict: a compare-and-set on `grounded_content_hash` (the
/// `apply_rank_result` shape, one cache key over) — only applies if the row's cached grounded hash
/// is stale relative to `grounded_content_hash` (never grounded, or content changed since the last
/// grounded verdict). `tier` is `Some` for a confirming/demoting [`crucible_contract::Disposition::Tier`]
/// (overwrites `issues.tier`) or `None` for [`crucible_contract::Disposition::Stale`] (the tier column
/// is untouched — `stale` is not a tier). Returns rows changed (0/1): only the winner should log
/// the rationale as evidence and ledger the grounded turn's cost.
///
/// It ALSO stamps `ranked_content_hash` to the same hash: a grounded verdict is the AUTHORITATIVE
/// rank (it overrides the text verdict), so it establishes the ranking cache over the exact content
/// it judged. This matters now that grounded dispatch is non-blocking and this collection is the
/// primary landing site for a LOW-CONFIDENCE ESCALATION turn — whose text rank was never persisted
/// (`apply_verdict` deferred at launch). Without it, `confirm_tier` would find `ranked_content_hash`
/// NULL on the next sweep, re-rank, and re-escalate a fresh (paid) grounded turn every pass. In the
/// pre-scope path `ranked_content_hash` already equals this hash (confirm_tier set it), so the extra
/// column write is a harmless idempotent no-op.
#[tracing::instrument(name = "db.apply_grounded_result", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn apply_grounded_result(
    ex: impl PgExecutor<'_>,
    key: &str,
    tier: Option<&str>,
    grounded_content_hash: &str,
) -> Result<bool> {
    let updated_at = crate::clock::now_rfc3339();
    let res = sqlx::query!(
        r#"
        UPDATE issues
        SET tier = COALESCE($1, tier),
            grounded_content_hash = $2,
            ranked_content_hash = $3,
            updated_at = $4
        WHERE key = $5 AND (grounded_content_hash IS NULL OR grounded_content_hash <> $6)
        "#,
        tier,
        grounded_content_hash,
        grounded_content_hash,
        updated_at,
        key,
        grounded_content_hash,
    )
    .execute(ex)
    .await
    .context("apply_grounded_result")?;
    Ok(res.rows_affected() == 1)
}

/// Every non-terminal issue key (`status <> 'done'`), for the daemon's startup re-enqueue.
#[tracing::instrument(name = "db.non_terminal_keys", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub async fn non_terminal_keys(ex: impl PgExecutor<'_>) -> Result<Vec<String>> {
    let rows =
        sqlx::query!(r#"SELECT key AS "key!" FROM issues WHERE status <> 'done' ORDER BY key"#)
            .fetch_all(ex)
            .await
            .context("non_terminal_keys")?;
    Ok(rows.into_iter().map(|r| r.key).collect())
}

/// The upstream-poll watermark for `repo`: the `updated_at` of the most-recently-seen
/// issue, or `None` if the repo has never been triaged.
#[tracing::instrument(name = "db.get_watermark", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repo = %repo), err)]
pub(crate) async fn get_watermark(ex: impl PgExecutor<'_>, repo: &str) -> Result<Option<String>> {
    let row = sqlx::query!(
        "SELECT last_seen_updated_at FROM repos WHERE repo = $1",
        repo,
    )
    .fetch_optional(ex)
    .await
    .context("get_watermark")?;
    Ok(row.and_then(|r| r.last_seen_updated_at))
}

/// Advance (or create) `repo`'s watermark to `updated_at`.
#[tracing::instrument(name = "db.set_watermark", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repo = %repo), err)]
pub(crate) async fn set_watermark(
    ex: impl PgExecutor<'_>,
    repo: &str,
    updated_at: &str,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO repos (repo, last_seen_updated_at) VALUES ($1, $2)
        ON CONFLICT(repo) DO UPDATE SET last_seen_updated_at = excluded.last_seen_updated_at
        "#,
        repo,
        updated_at,
    )
    .execute(ex)
    .await
    .context("set_watermark")?;
    Ok(())
}

/// Count issues in `repo` currently at `status` — the triage summary's `parked` column.
#[tracing::instrument(name = "db.count_issues_by_status", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repo = %repo), err)]
pub(crate) async fn count_issues_by_status(
    ex: impl PgExecutor<'_>,
    repo: &str,
    status: Status,
) -> Result<i64> {
    let status = status.as_str();
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM issues WHERE repo = $1 AND status = $2"#,
        repo,
        status,
    )
    .fetch_one(ex)
    .await
    .context("count_issues_by_status")?;
    Ok(row.n)
}

/// Count issues in `repo` whose `upstream_updated_at` was never stamped (rows ingested before
/// migration 0007) — the backfill pass's gate: zero means the pass makes no GitHub calls at all.
#[tracing::instrument(name = "db.count_null_upstream_updated_at", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repo = %repo), err)]
pub(crate) async fn count_null_upstream_updated_at(
    ex: impl PgExecutor<'_>,
    repo: &str,
) -> Result<i64> {
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM issues WHERE repo = $1 AND upstream_updated_at IS NULL"#,
        repo,
    )
    .fetch_one(ex)
    .await
    .context("count_null_upstream_updated_at")?;
    Ok(row.n)
}

/// Stamp `upstream_updated_at` onto still-NULL rows only, per key, in one transaction. A non-NULL
/// stamp is never overwritten (the watermark sweep owns those), and a key absent from the table
/// is skipped, not an error. Returns how many rows took a stamp. Deliberately leaves `updated_at`
/// alone — this is metadata repair, not row activity. Takes the pool (not a generic executor)
/// because it owns the transaction.
#[tracing::instrument(name = "db.stamp_null_upstream_updated_at", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn stamp_null_upstream_updated_at(
    pool: &sqlx::PgPool,
    stamps: &[(String, String)],
) -> Result<u64> {
    let mut tx = pool
        .begin()
        .await
        .context("stamp_null_upstream_updated_at: begin")?;
    let mut stamped = 0u64;
    for (key, upstream_updated_at) in stamps {
        let res = sqlx::query!(
            "UPDATE issues SET upstream_updated_at = $1 WHERE key = $2 AND upstream_updated_at IS NULL",
            upstream_updated_at,
            key,
        )
        .execute(&mut *tx)
        .await
        .context("stamp_null_upstream_updated_at: update")?;
        stamped += res.rows_affected();
    }
    tx.commit()
        .await
        .context("stamp_null_upstream_updated_at: commit")?;
    Ok(stamped)
}

/// Claim `from` → `parked` and set the reason + authority in one atomic UPDATE (the claim-and-park
/// primitive `reconcile` uses when a proposal dies in check/selftest). Returns whether this call
/// won the claim — only the winning racer parks, and only if the row is still at `from`. Stamps `pre_park_status`
/// with `from` so [`unpark_issue`] restores the issue's pipeline position.
#[tracing::instrument(name = "db.claim_park", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn claim_park(
    ex: impl PgExecutor<'_>,
    key: &str,
    from: Status,
    reason: &str,
    parked_by: ParkedBy,
    updated_at: &str,
) -> Result<bool> {
    let (from_s, by) = (from.as_str(), parked_by.as_str());
    let res = sqlx::query!(
        "UPDATE issues SET pre_park_status = status, status = 'parked', parked_reason = $1, \
         parked_by = $2, updated_at = $3 \
         WHERE key = $4 AND status = $5",
        reason,
        by,
        updated_at,
        key,
        from_s,
    )
    .execute(ex)
    .await
    .context("claim_park")?;
    Ok(res.rows_affected() == 1)
}

/// Insert a surviving pack's `scopes` row and return its autoincrement id (the run row references
/// it, closing the issue → scope → run provenance chain).
#[tracing::instrument(name = "db.insert_scope", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue = %s.issue), err)]
pub async fn insert_scope(ex: impl PgExecutor<'_>, s: &NewScope) -> Result<i64> {
    let row = sqlx::query!(
        r#"INSERT INTO scopes (issue, pack_digest, check_outcome) VALUES ($1, $2, $3)
           RETURNING id AS "id!""#,
        s.issue,
        s.pack_digest,
        s.check_outcome,
    )
    .fetch_one(ex)
    .await
    .context("insert_scope")?;
    Ok(row.id)
}

/// Record the exposure the engine computed from a frozen scope pack.
#[tracing::instrument(name = "db.set_scope_exposure", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn set_scope_exposure(
    ex: impl PgExecutor<'_>,
    scope_id: i64,
    extraction: &crate::playbooks::exposure::Extraction,
) -> Result<()> {
    let (exposure, digest) = extraction.stored()?;
    sqlx::query!(
        "UPDATE scopes SET exposure = $1, exposure_digest = $2 WHERE id = $3",
        exposure,
        digest,
        scope_id,
    )
    .execute(ex)
    .await
    .context("set_scope_exposure")?;
    Ok(())
}

/// The most recent `scopes` row for an issue (the pack currently at the approval), or `None` if the
/// issue was never scoped.
#[tracing::instrument(name = "db.latest_scope_for_issue", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue = %issue), err)]
pub async fn latest_scope_for_issue(ex: impl PgExecutor<'_>, issue: &str) -> Result<Option<Scope>> {
    let sql = format!("SELECT {SCOPE_COLS} FROM scopes WHERE issue = $1 ORDER BY id DESC LIMIT 1");
    sqlx::query(&sql)
        .bind(issue)
        .fetch_optional(ex)
        .await
        .context("latest_scope_for_issue")?
        .as_ref()
        .map(decode_scope)
        .transpose()
}

/// The full `scenarios` sidecar row for the SPA's detail panel (title/body/affected_repos/adopter,
/// none of which live on `issues` — the GitHub-flavored `title`/`body` columns keep their upstream
/// semantics and are never repurposed for a scenario). `None` for a key with no sidecar row.
pub(crate) struct ScenarioRow {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) affected_repos: Vec<String>,
    /// An authoritative brief reaches the scope agent verbatim, prescriptions intact, instead of
    /// being de-prescribed into a neutral problem framing.
    pub(crate) authoritative: bool,
    /// A pack the repo already carries, relative to the checkout root. `Some` makes the scope turn
    /// validate that pack instead of drafting one, so no agent runs.
    pub(crate) pack_path: Option<String>,
    pub(crate) created_by: String,
    pub(crate) created_at: String,
}

#[tracing::instrument(name = "db.get_scenario", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn get_scenario(pool: &PgPool, key: &str) -> Result<Option<ScenarioRow>> {
    let row = sqlx::query!(
        "SELECT title, body, authoritative, pack_path, created_by, created_at FROM scenarios WHERE key = $1",
        key,
    )
    .fetch_optional(pool)
    .await
    .context("get_scenario")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let affected_repos = get_scenario_repos(pool, key).await?;
    Ok(Some(ScenarioRow {
        title: row.title,
        body: row.body,
        affected_repos,
        authoritative: row.authoritative,
        pack_path: row.pack_path,
        created_by: row.created_by,
        created_at: row.created_at,
    }))
}

/// The ordered affected-repos hint list for a scenario (position 0 first). `issues.repo` — seeded
/// from position 0 at adoption — is the actual clone target; this list is the human's problem
/// framing, not a binding constraint.
#[tracing::instrument(name = "db.get_scenario_repos", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
async fn get_scenario_repos(ex: impl PgExecutor<'_>, key: &str) -> Result<Vec<String>> {
    let rows = sqlx::query!(
        "SELECT repo FROM scenario_repos WHERE key = $1 ORDER BY position",
        key,
    )
    .fetch_all(ex)
    .await
    .context("get_scenario_repos")?;
    Ok(rows.into_iter().map(|r| r.repo).collect())
}

/// Mint a new scenario-kind issue + its `scenarios` sidecar in one transaction. Adoption creates
/// a fresh key rather than mutating an existing row, so it runs directly against the pool instead
/// of through the `OverrideKind` queue (that queue exists to compose mutations with reconcile on
/// an *existing* key; a brand-new key races with nothing). Lands straight in `new` with a preset
/// tier — the human adoption IS the tier/priority authorization, since a scenario never reaches
/// the ranker. `Tier::T1` (the full propose+refine+adversary pipeline) is the preset: an adopted
/// scenario has no existing failing test for a `T0` judge to point at. Returns the minted
/// `scenario:{uuidv7}` key.
/// `affected_repos` is a non-empty hint list; `affected_repos[0]` is the actual clone target
/// (written into `issues.repo`), the full list rides along as the scope pod's problem framing.
/// `authoritative` marks a measurement-derived brief that reaches the scope agent verbatim,
/// prescriptions intact, instead of being de-prescribed. `pins` carries the clone ref and the
/// codegen-contract name; the API layer has already validated both.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "db.adopt_scenario", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn adopt_scenario(
    pool: &PgPool,
    title: &str,
    body: &str,
    affected_repos: &[String],
    authoritative: bool,
    pins: AdoptPins<'_>,
    created_by: &str,
) -> Result<String> {
    let key = format!("scenario:{}", uuid::Uuid::now_v7());
    adopt_body_issue(
        pool,
        &key,
        "scenario",
        title,
        body,
        affected_repos,
        authoritative,
        pins,
        created_by,
    )
    .await?;
    Ok(key)
}

/// Mint a scenario-shaped issue whose pack was supplied directly by an administrator. The launch
/// POST itself is both authorship and approval, so the row starts at `awaiting-approval` with an
/// already-approved scope. Reconcile can therefore reuse the ordinary build + loop-run pipeline
/// without spending on, or pretending to have performed, a scope turn.
pub(crate) async fn adopt_direct_pack(
    pool: &PgPool,
    launch: &crate::issues::model::NewDirectPack<'_>,
) -> Result<i64> {
    let now = crate::clock::now_rfc3339();
    let tier = Tier::T1.as_str();
    let mut tx = pool.begin().await.context("adopt_direct_pack: begin")?;
    sqlx::query(
        "INSERT INTO issues (key, repo, tier, status, priority, input_kind, git_ref, updated_at) \
         VALUES ($1, $2, $3, 'awaiting-approval', 0, 'scenario', $4, $5)",
    )
    .bind(launch.key)
    .bind(launch.repo)
    .bind(tier)
    .bind(launch.git_ref)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .context("adopt_direct_pack: insert issue")?;
    sqlx::query(
        "INSERT INTO scenarios (key, title, body, authoritative, created_by) \
         VALUES ($1, $2, $3, TRUE, $4)",
    )
    .bind(launch.key)
    .bind(launch.title)
    .bind(launch.body)
    .bind(launch.created_by)
    .execute(&mut *tx)
    .await
    .context("adopt_direct_pack: insert scenario")?;
    sqlx::query("INSERT INTO scenario_repos (key, repo, position) VALUES ($1, $2, 0)")
        .bind(launch.key)
        .bind(launch.repo)
        .execute(&mut *tx)
        .await
        .context("adopt_direct_pack: insert scenario repo")?;
    let scope_id = insert_scope(
        &mut *tx,
        &NewScope {
            issue: launch.key.to_string(),
            pack_digest: Some(launch.pack_digest.to_string()),
            check_outcome: Some("DIRECT".to_string()),
        },
    )
    .await?;
    record_approval(&mut *tx, scope_id, launch.created_by, &now).await?;
    tx.commit()
        .await
        .context("adopt_direct_pack: commit launch")?;
    Ok(scope_id)
}

/// The per-adoption pins a scenario may set on its own `issues` row. Grouped rather than passed as
/// two adjacent `Option<&str>`: a transposed pair would compile fine and silently clone a branch
/// named after a contract.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AdoptPins<'a> {
    /// Branch or tag to clone the target repo at; `None` = the repo's default branch.
    pub git_ref: Option<&'a str>,
    /// Name of the configured broker codegen contract this item is measured under; `None` = the
    /// pack measures locally on the loop pod.
    pub codegen_contract: Option<&'a str>,
    /// A pack the repo already carries, relative to the checkout root; `None` = the scope turn
    /// drafts one.
    pub pack_path: Option<&'a str>,
}

/// Mint a Jira-kind issue from a title/body the controller already fetched from Jira Cloud. Shares
/// the scenario sidecar + the caps-bypass semantics: a Jira row is `has_upstream() == false`, so it
/// flows the same non-upstream scope path (the stored body IS the goal, no live re-fetch), and the
/// human adopt is the tier/priority authorization. `key` is a pre-validated `jira:{site}:{PROJ-N}`
/// ([`crate::launches::jira::JiraRef::storage_key`]); the caller has already fetched `title`/`body`.
#[tracing::instrument(name = "db.adopt_jira", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn adopt_jira(
    pool: &PgPool,
    key: &str,
    title: &str,
    body: &str,
    affected_repos: &[String],
    authoritative: bool,
    created_by: &str,
) -> Result<String> {
    adopt_body_issue(
        pool,
        key,
        "jira",
        title,
        body,
        affected_repos,
        authoritative,
        // Jira adoption pins nothing: default branch, local measure.
        AdoptPins::default(),
        created_by,
    )
    .await?;
    Ok(key.to_string())
}

/// Shared insert for a human-adopted, non-upstream work item (scenario or Jira): the `issues` row
/// plus its `scenarios` body sidecar + ordered `scenario_repos`, all in one transaction. Adoption
/// mints a fresh key rather than mutating an existing row, so it runs directly against the pool (no
/// `OverrideKind` queue — that queue composes mutations with reconcile on an *existing* key; a
/// brand-new key races with nothing). Lands at `new` with a preset [`Tier::T1`] (the full
/// propose+refine+adversary pipeline): an adopted item has no existing failing test for a `T0`
/// judge, and it never reaches the ranker. `affected_repos[0]` is the clone target (`issues.repo`);
/// the full list rides the scope pod's goal framing as a hint the pack agent may override. `pins`
/// pins that clone to a branch/tag and/or names the broker codegen contract the item is measured
/// under; only scenario adoption ever sets either.
#[allow(clippy::too_many_arguments)]
async fn adopt_body_issue(
    pool: &PgPool,
    key: &str,
    input_kind: &str,
    title: &str,
    body: &str,
    affected_repos: &[String],
    authoritative: bool,
    pins: AdoptPins<'_>,
    created_by: &str,
) -> Result<()> {
    let updated_at = crate::clock::now_rfc3339();
    let tier = Tier::T1.as_str();
    let clone_target = affected_repos
        .first()
        .context("adopt_body_issue: affected_repos must be non-empty")?;
    let (git_ref, codegen_contract, pack_path) =
        (pins.git_ref, pins.codegen_contract, pins.pack_path);
    let mut tx = pool.begin().await.context("adopt_body_issue: begin")?;
    sqlx::query!(
        "INSERT INTO issues (key, repo, tier, status, priority, input_kind, git_ref, \
         codegen_contract, updated_at) VALUES ($1, $2, $3, 'new', 0, $4, $5, $6, $7)",
        key,
        clone_target,
        tier,
        input_kind,
        git_ref,
        codegen_contract,
        updated_at,
    )
    .execute(&mut *tx)
    .await
    .context("adopt_body_issue: insert issue")?;
    sqlx::query!(
        "INSERT INTO scenarios (key, title, body, authoritative, pack_path, created_by) VALUES ($1, $2, $3, $4, $5, $6)",
        key,
        title,
        body,
        authoritative,
        pack_path,
        created_by,
    )
    .execute(&mut *tx)
    .await
    .context("adopt_body_issue: insert scenario")?;
    for (position, repo) in affected_repos.iter().enumerate() {
        let position =
            i64::try_from(position).context("adopt_body_issue: too many affected repos")?;
        sqlx::query!(
            "INSERT INTO scenario_repos (key, repo, position) VALUES ($1, $2, $3)",
            key,
            repo,
            position,
        )
        .execute(&mut *tx)
        .await
        .context("adopt_body_issue: insert scenario_repos")?;
    }
    tx.commit().await.context("adopt_body_issue: commit")?;
    Ok(())
}

/// Persist a scope turn's structured report verbatim (`scope_reports`), success or failure.
#[tracing::instrument(name = "db.insert_scope_report", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn insert_scope_report(
    ex: impl PgExecutor<'_>,
    r: &crate::issues::model::NewScopeReport,
) -> Result<i64> {
    let now = crate::clock::now_rfc3339();
    let res = sqlx::query!(
        r#"
        INSERT INTO scope_reports (issue_key, pod_name, survived, report_json, created_at)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id AS "id!"
        "#,
        r.issue_key,
        r.pod_name,
        r.survived,
        r.report_json,
        now,
    )
    .fetch_one(ex)
    .await
    .context("insert_scope_report")?;
    Ok(res.id)
}

/// The most recent structured scope report for an issue, or `None` if no scope turn ever
/// finished with a report (dispatch failures and timeouts leave nothing here).
#[tracing::instrument(name = "db.latest_scope_report", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue_key = %issue_key), err)]
pub(crate) async fn latest_scope_report(
    ex: impl PgExecutor<'_>,
    issue_key: &str,
) -> Result<Option<crate::issues::model::ScopeReportRow>> {
    let row = sqlx::query!(
        r#"
        SELECT id AS "id!", issue_key AS "issue_key!", pod_name, survived AS "survived!",
               report_json AS "report_json!", created_at AS "created_at!"
        FROM scope_reports WHERE issue_key = $1 ORDER BY id DESC LIMIT 1
        "#,
        issue_key,
    )
    .fetch_optional(ex)
    .await
    .context("latest_scope_report")?;
    Ok(row.map(|r| crate::issues::model::ScopeReportRow {
        id: r.id,
        issue_key: r.issue_key,
        pod_name: r.pod_name,
        survived: r.survived,
        report_json: r.report_json,
        created_at: r.created_at,
    }))
}

/// Transcripts are the biggest rows in the DB, so retention is enforced on every insert: only the
/// newest N per issue survive ([`insert_scope_transcript`] prunes past this).
const SCOPE_TRANSCRIPT_KEEP: i64 = 3;

/// Persist a scope turn's preserved agent transcript (`scope_transcripts`), then prune the
/// issue's older attempts past [`SCOPE_TRANSCRIPT_KEEP`] — the structured `scope_reports` rows
/// keep the full history, the heavyweight transcripts keep only the recent tail.
#[tracing::instrument(name = "db.insert_scope_transcript", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn insert_scope_transcript(
    pool: &PgPool,
    t: &crate::issues::model::NewScopeTranscript,
) -> Result<i64> {
    let now = crate::clock::now_rfc3339();
    let res = sqlx::query!(
        r#"
        INSERT INTO scope_transcripts (scope_report_id, issue_key, transcript_gz, created_at)
        VALUES ($1, $2, $3, $4)
        RETURNING id AS "id!"
        "#,
        t.scope_report_id,
        t.issue_key,
        t.transcript_gz,
        now,
    )
    .fetch_one(pool)
    .await
    .context("insert_scope_transcript")?;
    sqlx::query!(
        r#"
        DELETE FROM scope_transcripts
        WHERE issue_key = $1
          AND id NOT IN (
              SELECT id FROM scope_transcripts WHERE issue_key = $2 ORDER BY id DESC LIMIT $3
          )
        "#,
        t.issue_key,
        t.issue_key,
        SCOPE_TRANSCRIPT_KEEP,
    )
    .execute(pool)
    .await
    .context("prune scope_transcripts")?;
    Ok(res.id)
}

/// The most recent preserved transcript for an issue, or `None` if no scope turn ever delivered
/// one (pre-feature turns, dispatch failures, timeouts).
#[tracing::instrument(name = "db.latest_scope_transcript", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue_key = %issue_key), err)]
pub(crate) async fn latest_scope_transcript(
    ex: impl PgExecutor<'_>,
    issue_key: &str,
) -> Result<Option<crate::issues::model::ScopeTranscriptRow>> {
    let row = sqlx::query!(
        r#"
        SELECT id AS "id!", scope_report_id AS "scope_report_id!", issue_key AS "issue_key!",
               transcript_gz AS "transcript_gz!", created_at AS "created_at!"
        FROM scope_transcripts WHERE issue_key = $1 ORDER BY id DESC LIMIT 1
        "#,
        issue_key,
    )
    .fetch_optional(ex)
    .await
    .context("latest_scope_transcript")?;
    Ok(row.map(|r| crate::issues::model::ScopeTranscriptRow {
        id: r.id,
        scope_report_id: r.scope_report_id,
        issue_key: r.issue_key,
        transcript_gz: r.transcript_gz,
        created_at: r.created_at,
    }))
}

/// Record a human approval on a scope — the signal that flips its approval gate open. Only the
/// FIRST approval takes — `WHERE approved_at IS NULL` — so a re-poll seeing the same approval is an
/// idempotent no-op. Returns whether this poll was the one that flipped the approval open.
#[tracing::instrument(name = "db.record_approval", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn record_approval(
    ex: impl PgExecutor<'_>,
    scope_id: i64,
    approved_by: &str,
    approved_at: &str,
) -> Result<bool> {
    let res = sqlx::query!(
        "UPDATE scopes SET approved_by = $1, approved_at = $2, \
         approved_exposure_digest = exposure_digest \
         WHERE id = $3 AND approved_at IS NULL",
        approved_by,
        approved_at,
        scope_id,
    )
    .execute(ex)
    .await
    .context("record_approval")?;
    Ok(res.rows_affected() == 1)
}

/// Baseline (or update) the frozen upstream-content hash on a scope — the reference a later approval
/// reconcile compares against to detect goal drift.
#[tracing::instrument(name = "db.set_scope_frozen_hash", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn set_scope_frozen_hash(
    ex: impl PgExecutor<'_>,
    scope_id: i64,
    hash: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE scopes SET frozen_issue_hash = $1 WHERE id = $2",
        hash,
        scope_id,
    )
    .execute(ex)
    .await
    .context("set_scope_frozen_hash")?;
    Ok(())
}

/// Mark a scope stale and record the id of the "upstream changed" comment on the approval PR,
/// updating the comment in place. Set together so a stale row always carries its comment id.
#[tracing::instrument(name = "db.set_scope_stale", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn set_scope_stale(
    ex: impl PgExecutor<'_>,
    scope_id: i64,
    comment_id: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE scopes SET stale = TRUE, stale_comment_id = $1 WHERE id = $2",
        comment_id,
        scope_id,
    )
    .execute(ex)
    .await
    .context("set_scope_stale")?;
    Ok(())
}

/// Every `awaiting-approval` issue whose latest scope carries an open, not-yet-approved approval PR
/// — the working set the approval poll checks each PR against for the approval signal.
#[tracing::instrument(name = "db.awaiting_approval_scopes", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn awaiting_approval_scopes(
    ex: impl PgExecutor<'_>,
) -> Result<Vec<AwaitingApproval>> {
    let rows = sqlx::query!(
        r#"
        SELECT i.key AS "key!", i.repo AS "repo!", s.id AS "scope_id!",
               s.approval_pr AS "approval_pr!", s.stale AS "stale!",
               s.exposure_digest
        FROM issues i
        JOIN scopes s ON s.id = (
            SELECT id FROM scopes WHERE issue = i.key ORDER BY id DESC LIMIT 1
        )
        WHERE i.status = 'awaiting-approval'
          AND s.approval_pr IS NOT NULL
          AND s.approved_at IS NULL
        ORDER BY i.key
        "#,
    )
    .fetch_all(ex)
    .await
    .context("awaiting_approval_scopes")?;
    Ok(rows
        .into_iter()
        .map(|r| AwaitingApproval {
            key: r.key,
            repo: r.repo,
            scope_id: r.scope_id,
            approval_pr: r.approval_pr,
            stale: r.stale,
            exposure_digest: r.exposure_digest,
        })
        .collect())
}

/// The latest kept-candidate PR per issue (issue key → pr_url), for the issue surfaces' PR chips.
/// Rows come back ordered by run_id ascending (lexical = chronological, the run-id stamp), so the
/// map fold leaves each issue holding the PR from its newest run.
#[tracing::instrument(name = "db.latest_kept_pr_urls", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn latest_kept_pr_urls(
    ex: impl PgExecutor<'_>,
) -> Result<HashMap<String, String>> {
    let rows = sqlx::query!(
        r#"
        SELECT s.issue AS "issue!", c.pr_url AS "pr_url!"
        FROM candidates c
        JOIN runs r ON r.run_id = c.run_id
        JOIN scopes s ON s.id = r.scope
        WHERE c.pr_url IS NOT NULL AND c.decision = 'keep'
        ORDER BY c.run_id ASC
        "#,
    )
    .fetch_all(ex)
    .await
    .context("latest_kept_pr_urls")?;
    Ok(rows.into_iter().map(|r| (r.issue, r.pr_url)).collect())
}

/// Unpark an issue (human override or machine stale-revival): `parked` → its pre-park status
/// (`pre_park_status`, stamped by the park writers; a legacy NULL row falls back to `new`),
/// clearing the park reason + authority + stamp. Guarded by `status = 'parked'` so it never
/// resurrects a live row. Returns the restored status when the unpark took (`None` = no-op), so
/// the caller's event line reports the real transition — reconcile re-converges from wherever the
/// issue actually was (a restored `awaiting-approval` re-reads its standing approval, a restored
/// `running` re-adopts or re-drives; that's the level-triggered contract).
#[tracing::instrument(name = "db.unpark_issue", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn unpark_issue(
    ex: impl PgExecutor<'_>,
    key: &str,
    updated_at: &str,
) -> Result<Option<String>> {
    let row = sqlx::query!(
        r#"UPDATE issues SET status = COALESCE(pre_park_status, 'new'), pre_park_status = NULL,
         parked_reason = NULL, parked_by = NULL, updated_at = $1
         WHERE key = $2 AND status = 'parked'
         RETURNING status AS "status!""#,
        updated_at,
        key,
    )
    .fetch_optional(ex)
    .await
    .context("unpark_issue")?;
    Ok(row.map(|r| r.status))
}

/// Set an issue's scheduling priority (a human bump). Priority is orthogonal to status,
/// so this touches no lifecycle column — it just re-weights the `ORDER BY tier, priority` pick.
#[tracing::instrument(name = "db.set_priority", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn set_priority(
    ex: impl PgExecutor<'_>,
    key: &str,
    priority: i64,
) -> Result<bool> {
    let updated_at = crate::clock::now_rfc3339();
    let res = sqlx::query!(
        "UPDATE issues SET priority = $1, updated_at = $2 WHERE key = $3",
        priority,
        updated_at,
        key,
    )
    .execute(ex)
    .await
    .context("set_priority")?;
    Ok(res.rows_affected() >= 1)
}

/// The pod name of the most recent run launched for `issue` (via its scopes), or `None`. The
/// upstream-close path uses it to stop a live run. Newest by insertion order (`rowid`).
#[tracing::instrument(name = "db.latest_run_pod_for_issue", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue = %issue), err)]
pub(crate) async fn latest_run_pod_for_issue(
    ex: impl PgExecutor<'_>,
    issue: &str,
) -> Result<Option<String>> {
    let row = sqlx::query!(
        r#"
        SELECT r.pod
        FROM runs r
        WHERE r.issue = $1
        ORDER BY r.seq DESC LIMIT 1
        "#,
        issue,
    )
    .fetch_optional(ex)
    .await
    .context("latest_run_pod_for_issue")?;
    Ok(row.and_then(|r| r.pod))
}

/// Record the approval PR opened for a pack (the approval gate's evidence pointer). The
/// approval watch later stamps `approved_by`/`approved_at` on the same row.
#[tracing::instrument(name = "db.set_scope_approval_pr", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub async fn set_scope_approval_pr(
    ex: impl PgExecutor<'_>,
    scope_id: i64,
    pr_url: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE scopes SET approval_pr = $1 WHERE id = $2",
        pr_url,
        scope_id,
    )
    .execute(ex)
    .await
    .context("set_scope_approval_pr")?;
    Ok(())
}

/// Count scope turns ledgered on a UTC day (`YYYY-MM-DD`) — the `max_scopes_per_day` cap. Every
/// scope appends one `kind = 'scope'` ledger row, so the cap is a COUNT over the ledger.
#[tracing::instrument(name = "db.count_scopes_on_day", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", day = %day), err)]
pub(crate) async fn count_scopes_on_day(ex: impl PgExecutor<'_>, day: &str) -> Result<i64> {
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind = 'scope' AND substr(ts, 1, 10) = $1"#,
        day,
    )
    .fetch_one(ex)
    .await
    .context("count_scopes_on_day")?;
    Ok(row.n)
}

/// Count issues by status, returned as (status_str, count) pairs.
#[tracing::instrument(name = "db.status_counts", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn status_counts(ex: impl PgExecutor<'_>) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query("SELECT status, COUNT(*) AS n FROM issues GROUP BY status")
        .fetch_all(ex)
        .await
        .context("status_counts")?;
    rows.into_iter()
        .map(|r| {
            let status: String = r.try_get("status")?;
            let count: i64 = r.try_get("n")?;
            Ok((status, count))
        })
        .collect()
}

/// Count non-terminal issues by tier (NULL tier → "unranked"), returned as (tier, count) pairs.
#[tracing::instrument(name = "db.tier_counts", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn tier_counts(ex: impl PgExecutor<'_>) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query(
        "SELECT COALESCE(tier, 'unranked') AS tier, COUNT(*) AS n \
         FROM issues WHERE status NOT IN ('done') GROUP BY tier",
    )
    .fetch_all(ex)
    .await
    .context("tier_counts")?;
    rows.into_iter()
        .map(|r| {
            let tier: String = r.try_get("tier")?;
            let count: i64 = r.try_get("n")?;
            Ok((tier, count))
        })
        .collect()
}

/// Per-repository issue counts by status plus the poll watermark and the runtime watch state
/// (Lane O3). The row set is every repo name known to either table — `issues.repo` (so a repo
/// with tracked issues always shows, even if its `repos` row hasn't landed yet) UNIONed with
/// `repos.repo` (so a freshly-watched repo with zero issues yet still shows; every issue-count
/// column is simply 0 until the first triage sweep lands one).
#[tracing::instrument(name = "db.repo_health", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn repo_health(
    ex: impl PgExecutor<'_>,
) -> Result<Vec<crate::issues::model::RepoHealth>> {
    let rows = sqlx::query(
        r#"
        WITH known_repos AS (
            SELECT repo FROM issues
            UNION
            SELECT repo FROM repos
        )
        SELECT k.repo,
               SUM(CASE WHEN i.status = 'new' THEN 1 ELSE 0 END) AS new,
               SUM(CASE WHEN i.status = 'scoped' THEN 1 ELSE 0 END) AS scoped,
               SUM(CASE WHEN i.status = 'awaiting-approval' THEN 1 ELSE 0 END) AS awaiting_approval,
               SUM(CASE WHEN i.status = 'running' THEN 1 ELSE 0 END) AS running,
               SUM(CASE WHEN i.status = 'pr-open' THEN 1 ELSE 0 END) AS pr_open,
               SUM(CASE WHEN i.status = 'parked' THEN 1 ELSE 0 END) AS parked,
               SUM(CASE WHEN i.status = 'done' THEN 1 ELSE 0 END) AS done,
               COUNT(i.key) AS total,
               r.last_seen_updated_at AS watermark,
               COALESCE(r.watched, TRUE) AS watched,
               COALESCE(r.paused, FALSE) AS paused,
               r.added_by AS added_by,
               r.added_at AS added_at
        FROM known_repos k
        LEFT JOIN issues i ON i.repo = k.repo
        LEFT JOIN repos r ON r.repo = k.repo
        GROUP BY k.repo, r.last_seen_updated_at, r.watched, r.paused, r.added_by, r.added_at
        ORDER BY total DESC, k.repo
        "#,
    )
    .fetch_all(ex)
    .await
    .context("repo_health")?;
    rows.into_iter()
        .map(|r| {
            Ok(crate::issues::model::RepoHealth {
                repo: r.try_get("repo")?,
                new: r.try_get("new")?,
                scoped: r.try_get("scoped")?,
                awaiting_approval: r.try_get("awaiting_approval")?,
                running: r.try_get("running")?,
                pr_open: r.try_get("pr_open")?,
                parked: r.try_get("parked")?,
                done: r.try_get("done")?,
                total: r.try_get("total")?,
                watermark: r.try_get("watermark")?,
                watched: r.try_get("watched")?,
                paused: r.try_get("paused")?,
                added_by: r.try_get("added_by")?,
                added_at: r.try_get("added_at")?,
            })
        })
        .collect()
}

/// Raw stage counts for the fleet-wide pipeline funnel (`GET /api/funnel`), one row for the
/// whole fleet (no `GROUP BY`). Every issue lands in exactly one of the six in-band buckets
/// (`discovered`..`done`); `parked`/`stale` are an out-of-band call-out over the same `parked`
/// status issues (`stale` is the subset of `parked` whose `parked_reason` starts with `stale`,
/// per the grounded ranker's "already implemented" disposition — see `journey.rs`'s `Ranked`
/// step doc). `scoped` has no separate DTO stage: a `scoped` row is definitionally already
/// tiered, so it folds into `ranked` alongside tiered `new` rows rather than vanishing from the
/// funnel.
pub(crate) struct FunnelCounts {
    pub(crate) discovered: i64,
    pub(crate) ranked: i64,
    pub(crate) awaiting_approval: i64,
    pub(crate) running: i64,
    pub(crate) pr_open: i64,
    pub(crate) done: i64,
    pub(crate) parked: i64,
    pub(crate) stale: i64,
}

/// Compute [`FunnelCounts`] over the whole `issues` table in one aggregate query. The `stale`
/// bucket's `parked_reason LIKE 'stale%'` counts exactly [`crate::model::ParkReason::StaleRankHorizon`]
/// and [`crate::model::ParkReason::StaleAlreadyImplemented`] rows — both variants' `Display`
/// renderings start with `"stale"` (guarded by a unit test in `model.rs`, since a whole-table
/// aggregate isn't worth a Rust-side parse loop over every row for one COUNT).
#[tracing::instrument(name = "db.funnel_counts", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn funnel_counts(ex: impl PgExecutor<'_>) -> Result<FunnelCounts> {
    let row = sqlx::query(
        r#"
        SELECT
            COALESCE(SUM(CASE WHEN status = 'new' AND tier IS NULL THEN 1 ELSE 0 END), 0)
                AS discovered,
            COALESCE(SUM(CASE WHEN (status = 'new' AND tier IS NOT NULL) OR status = 'scoped'
                     THEN 1 ELSE 0 END), 0) AS ranked,
            COALESCE(SUM(CASE WHEN status = 'awaiting-approval' THEN 1 ELSE 0 END), 0)
                AS awaiting_approval,
            COALESCE(SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END), 0) AS running,
            COALESCE(SUM(CASE WHEN status = 'pr-open' THEN 1 ELSE 0 END), 0) AS pr_open,
            COALESCE(SUM(CASE WHEN status = 'done' THEN 1 ELSE 0 END), 0) AS done,
            COALESCE(SUM(CASE WHEN status = 'parked' THEN 1 ELSE 0 END), 0) AS parked,
            COALESCE(SUM(CASE WHEN status = 'parked' AND parked_reason LIKE 'stale%'
                     THEN 1 ELSE 0 END), 0) AS stale
        FROM issues
        "#,
    )
    .fetch_one(ex)
    .await
    .context("funnel_counts")?;
    Ok(FunnelCounts {
        discovered: row.try_get("discovered")?,
        ranked: row.try_get("ranked")?,
        awaiting_approval: row.try_get("awaiting_approval")?,
        running: row.try_get("running")?,
        pr_open: row.try_get("pr_open")?,
        done: row.try_get("done")?,
        parked: row.try_get("parked")?,
        stale: row.try_get("stale")?,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::launches::model::NewPlaybookLaunch;
    use crate::launches::store::{AdoptPlaybookOutcome, adopt_playbook_launch};

    use crate::issues::model::IssueKind;
    use crate::issues::repo_watch::*;

    use anyhow::Result;
    use sqlx::PgPool;

    /// The target is pinned on the issue, so every pod its lifecycle dispatches reads one answer.
    /// NULL is the compatibility path: it means the controller's configured default, resolved late.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_issues_dispatch_target_is_pinned_and_read_back(pool: PgPool) -> anyhow::Result<()> {
        upsert_issue(&pool, &seed("owner/repo#1", "owner/repo", 5)).await?;
        assert_eq!(
            dispatch_target(&pool, "owner/repo#1").await?.target,
            None,
            "an issue that never chose reads as the configured default"
        );

        set_dispatch_target(&pool, "owner/repo#1", Some("wharf")).await?;
        assert_eq!(
            dispatch_target(&pool, "owner/repo#1")
                .await?
                .target
                .as_deref(),
            Some("wharf")
        );
        assert_eq!(
            get_issue(&pool, "owner/repo#1")
                .await?
                .expect("row")
                .dispatch_target
                .as_deref(),
            Some("wharf"),
            "the issue read projects it too"
        );

        // A scheduled firing pins NULL explicitly when its schedule named no target.
        set_dispatch_target(&pool, "owner/repo#1", None).await?;
        assert_eq!(dispatch_target(&pool, "owner/repo#1").await?.target, None);

        assert_eq!(
            dispatch_target(&pool, "owner/repo#404").await?,
            DispatchRouting {
                target: None,
                contract: None
            },
            "an unknown issue is not an error"
        );
        Ok(())
    }

    fn seed(key: &str, repo: &str, priority: i64) -> NewIssue {
        NewIssue {
            key: key.to_string(),
            repo: repo.to_string(),
            priority,
            evidence_url: None,
            title: Some(format!("title for {key}")),
            author: Some("octocat".to_string()),
            body: None,
            labels: Vec::new(),
            upstream_updated_at: None,
        }
    }

    /// `updated_at` is stamped internally by `upsert_issue`/`set_ranked_tier` (wall-clock "now"),
    /// so the sort/upstream-filter tests that need distinct, deterministic values pin `updated_at`
    /// directly with a raw UPDATE after seeding — same effect as a caller-supplied stamp, without
    /// reintroducing a timestamp parameter every production call site would have to thread through.
    async fn pin_updated_at(pool: &PgPool, key: &str, updated_at: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE issues SET updated_at = $1 WHERE key = $2",
            updated_at,
            key
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn seed_rows(pool: &PgPool) -> Result<()> {
        // Tier is never part of an upsert (triage is pure discovery) — seed it separately via
        // `set_ranked_tier`, the way the ranker actually sets it.
        for (key, repo, tier, priority, updated_at, labels) in [
            (
                "owner/repo#1",
                "owner/repo",
                "T0",
                5,
                "2026-01-01T00:00:00Z",
                &["bug", "area/scheduler"][..],
            ),
            (
                "owner/repo#2",
                "owner/repo",
                "T1",
                9,
                "2026-01-03T00:00:00Z",
                &["perf"][..],
            ),
            (
                "other/repo#1",
                "other/repo",
                "T0",
                1,
                "2026-01-02T00:00:00Z",
                &["bug"][..],
            ),
        ] {
            let mut iss = seed(key, repo, priority);
            iss.labels = labels.iter().map(|l| l.to_string()).collect();
            iss.upstream_updated_at = Some(updated_at.to_string());
            upsert_issue(pool, &iss).await?;
            set_ranked_tier(pool, key, tier, "perf", &format!("hash-{key}")).await?;
            pin_updated_at(pool, key, updated_at).await?;
        }
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn upsert_does_not_clobber_human_bumped_priority(pool: PgPool) -> Result<()> {
        let iss = seed("owner/repo#99", "owner/repo", 0);
        upsert_issue(&pool, &iss).await?;

        let row = get_issue(&pool, "owner/repo#99").await?.expect("inserted");
        assert_eq!(row.priority, 0, "initial insert sets priority 0");

        set_priority(&pool, "owner/repo#99", 9).await?;
        let row = get_issue(&pool, "owner/repo#99").await?.expect("bumped");
        assert_eq!(row.priority, 9, "human bump took effect");

        let iss_again = seed("owner/repo#99", "owner/repo", 0);
        upsert_issue(&pool, &iss_again).await?;
        let row = get_issue(&pool, "owner/repo#99")
            .await?
            .expect("re-upserted");
        assert_eq!(
            row.priority, 9,
            "triage re-upsert must NOT clobber the human-bumped priority"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn preexisting_row_decodes_as_github_via_default_backfill(pool: PgPool) -> Result<()> {
        let iss = seed("neuralmagic/crucible#7", "neuralmagic/crucible", 3);
        upsert_issue(&pool, &iss).await?;

        let row = get_issue(&pool, "neuralmagic/crucible#7")
            .await?
            .expect("inserted");
        assert_eq!(
            row.kind,
            crate::issues::model::InputKind::GitHub {
                owner: "neuralmagic".to_string(),
                repo: "crucible".to_string(),
                number: 7,
            }
        );
        assert!(row.kind.has_upstream());
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn insert_scope_transcript_prunes_past_the_per_issue_keep(pool: PgPool) -> Result<()> {
        // One report row per attempt; each carries a transcript. Another issue's transcript must
        // survive the pruning untouched.
        let other_report = insert_scope_report(
            &pool,
            &crate::issues::model::NewScopeReport {
                issue_key: "owner/repo#2".to_string(),
                pod_name: None,
                survived: true,
                report_json: "{}".to_string(),
            },
        )
        .await?;
        insert_scope_transcript(
            &pool,
            &crate::issues::model::NewScopeTranscript {
                scope_report_id: other_report,
                issue_key: "owner/repo#2".to_string(),
                transcript_gz: b"other".to_vec(),
            },
        )
        .await?;

        for attempt in 0..SCOPE_TRANSCRIPT_KEEP + 2 {
            let report_id = insert_scope_report(
                &pool,
                &crate::issues::model::NewScopeReport {
                    issue_key: "owner/repo#1".to_string(),
                    pod_name: None,
                    survived: false,
                    report_json: "{}".to_string(),
                },
            )
            .await?;
            insert_scope_transcript(
                &pool,
                &crate::issues::model::NewScopeTranscript {
                    scope_report_id: report_id,
                    issue_key: "owner/repo#1".to_string(),
                    transcript_gz: format!("attempt-{attempt}").into_bytes(),
                },
            )
            .await?;
        }

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM scope_transcripts WHERE issue_key = $1")
                .bind("owner/repo#1")
                .fetch_one(&pool)
                .await?;
        assert_eq!(count, SCOPE_TRANSCRIPT_KEEP, "older attempts pruned");

        let latest = latest_scope_transcript(&pool, "owner/repo#1")
            .await?
            .expect("latest survives");
        assert_eq!(
            latest.transcript_gz,
            format!("attempt-{}", SCOPE_TRANSCRIPT_KEEP + 1).into_bytes(),
            "the newest attempt is the one served"
        );
        let other = latest_scope_transcript(&pool, "owner/repo#2")
            .await?
            .expect("other issue untouched");
        assert_eq!(other.transcript_gz, b"other".to_vec());
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn list_issues_filtered_sorts_by_priority_desc(pool: PgPool) -> Result<()> {
        seed_rows(&pool).await?;
        let q = IssueQuery {
            sort: SortKey::Priority,
            dir: crate::model::SortDir::Desc,
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["owner/repo#2", "owner/repo#1", "other/repo#1"]
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn list_issues_filtered_sorts_by_updated_asc(pool: PgPool) -> Result<()> {
        seed_rows(&pool).await?;
        let q = IssueQuery {
            sort: SortKey::Updated,
            dir: SortDir::Asc,
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["owner/repo#1", "other/repo#1", "owner/repo#2"]
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn list_issues_filtered_combines_repo_and_tier_filters(pool: PgPool) -> Result<()> {
        seed_rows(&pool).await?;
        let q = IssueQuery {
            repo: Some("owner/repo".to_string()),
            tier: Some("T0".to_string()),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["owner/repo#1"],
            "only the T0 row in owner/repo survives both filters"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn list_issues_filtered_with_no_filters_returns_everything(pool: PgPool) -> Result<()> {
        seed_rows(&pool).await?;
        let rows = list_issues_filtered(&pool, &IssueQuery::default()).await?;
        assert_eq!(rows.len(), 3);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn list_issues_filtered_by_label_matches_the_whole_label_only(
        pool: PgPool,
    ) -> Result<()> {
        seed_rows(&pool).await?;
        let q = IssueQuery {
            label: Some("bug".to_string()),
            sort: SortKey::Priority,
            dir: crate::model::SortDir::Desc,
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["owner/repo#1", "other/repo#1"],
            "only the rows labeled `bug`, not `perf`"
        );

        // The quoted LIKE pattern must not match a partial label name.
        let q = IssueQuery {
            label: Some("bu".to_string()),
            ..Default::default()
        };
        assert!(list_issues_filtered(&pool, &q).await?.is_empty());

        // Round trip: labels decode back off the row.
        let q = IssueQuery {
            label: Some("area/scheduler".to_string()),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].labels, vec!["bug", "area/scheduler"]);
        Ok(())
    }

    /// The adopt transaction re-reads the registry digest, so a re-register between the
    /// endpoint's validation and the insert is a refusal, not a launch carrying values
    /// validated against a schema the pack no longer declares.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn adopting_against_a_moved_schema_is_refused(pool: PgPool) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz,
                                      tar_digest, tar_bytes, params_schema, schema_digest,
                                      core_rev, created_by, created_at, updated_at)
               VALUES ('survey', 'reads a paper', 'owner/packs', 'main', 'abc123', '', $1,
                       'sha256:tar', 3, '{"type":"object"}'::jsonb, 'sha256:after', 'core1',
                       'wren', '2026-08-23T00:00:00Z', '2026-08-23T00:00:00Z')"#,
        )
        .bind(vec![1u8, 2, 3])
        .execute(&pool)
        .await?;
        let max_time = crate::model::MaxTime::parse("30m").expect("duration");
        let params = serde_json::json!({});
        let launch = NewPlaybookLaunch {
            playbook: "survey",
            repo: "owner/packs",
            title: "reads a paper",
            params: &params,
            schema_digest: "sha256:before",
            max_cost: 1.0,
            max_time: &max_time,
            advance_dedupe: false,
            dedupe_schedule: None,
            origin: crate::model::LaunchOrigin::Manual,
            draft_version: None,
            created_by: Some("wren"),
            launcher_groups: None,
        };
        let key = "playbook:survey:0199c0de-7c2c-71a5-8000-2";
        assert!(matches!(
            adopt_playbook_launch(&pool, key, &launch).await?,
            AdoptPlaybookOutcome::SchemaDrifted { ref current } if current == "sha256:after"
        ));
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_launches")
            .fetch_one(&pool)
            .await?;
        assert_eq!(rows, 0, "nothing was written");
        assert!(matches!(
            adopt_playbook_launch(&pool, "playbook:gone:0199c0de-7c2c-71a5-8000-3", &{
                NewPlaybookLaunch {
                    playbook: "gone",
                    ..launch
                }
            })
            .await?,
            AdoptPlaybookOutcome::UnknownPlaybook
        ));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn list_issues_filtered_by_kind_matches_the_input_kind_tag(pool: PgPool) -> Result<()> {
        // Three github rows from the shared seed plus one adopted scenario row and one adopted jira row.
        seed_rows(&pool).await?;
        let scenario_key = adopt_scenario(
            &pool,
            "title",
            "body",
            &["owner/repo".to_string()],
            true,
            AdoptPins::default(),
            "octocat",
        )
        .await?;
        let jira_key = adopt_jira(
            &pool,
            "jira:example:ACME-1234",
            "jira title",
            "jira body",
            &["owner/repo".to_string()],
            false,
            "octocat",
        )
        .await?;

        // `kind=github` keeps only the github rows, never the scenario or jira row.
        let q = IssueQuery {
            kind: Some(IssueKind::GitHub),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|r| matches!(r.kind, InputKind::GitHub { .. })),
            "kind=github excludes the adopted rows"
        );

        // `kind=scenario` keeps only the adopted scenario row.
        let q = IssueQuery {
            kind: Some(IssueKind::Scenario),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, scenario_key);
        assert!(matches!(rows[0].kind, InputKind::Scenario { .. }));

        // `kind=jira` keeps only the adopted jira row, decoded back into InputKind::Jira.
        let q = IssueQuery {
            kind: Some(IssueKind::Jira),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, jira_key);
        assert!(matches!(
            rows[0].kind,
            InputKind::Jira {
                ref site,
                ref project,
                number: 1234,
            } if site == "example" && project == "ACME"
        ));

        // An adopted playbook launch row answers `kind=playbook` and nothing else.
        sqlx::query(
            r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz,
                                      tar_digest, tar_bytes, params_schema, schema_digest,
                                      core_rev, created_by, created_at, updated_at)
               VALUES ('survey', 'reads a paper', 'owner/packs', 'main', 'abc123', '', $1,
                       'sha256:tar', 3, '{"type":"object"}'::jsonb, 'sha256:schema', 'core1',
                       'wren', '2026-08-23T00:00:00Z', '2026-08-23T00:00:00Z')"#,
        )
        .bind(vec![1u8, 2, 3])
        .execute(&pool)
        .await?;
        let playbook_key = "playbook:survey:0199c0de-7c2c-71a5-8000-1";
        let max_time = crate::model::MaxTime::parse("30m").expect("duration");
        let params = serde_json::json!({});
        assert!(matches!(
            adopt_playbook_launch(
                &pool,
                playbook_key,
                &NewPlaybookLaunch {
                    playbook: "survey",
                    repo: "owner/packs",
                    title: "reads a paper",
                    params: &params,
                    schema_digest: "sha256:schema",
                    max_cost: 1.0,
                    max_time: &max_time,
                    advance_dedupe: false,
                    dedupe_schedule: None,
                    origin: crate::model::LaunchOrigin::Manual,
                    draft_version: None,
                    created_by: Some("wren"),
                    launcher_groups: None,
                },
            )
            .await?,
            AdoptPlaybookOutcome::Adopted
        ));
        let q = IssueQuery {
            kind: Some(IssueKind::Playbook),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, playbook_key);

        // No kind filter returns everything (github + scenario + jira + playbook).
        let rows = list_issues_filtered(&pool, &IssueQuery::default()).await?;
        assert_eq!(rows.len(), 6);

        // The issues board's default: everything except the playbook rows.
        let q = IssueQuery {
            exclude_kind: Some(IssueKind::Playbook),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|r| r.key != playbook_key));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn label_filter_combines_with_repo(pool: PgPool) -> Result<()> {
        seed_rows(&pool).await?;
        let q = IssueQuery {
            label: Some("bug".to_string()),
            repo: Some("owner/repo".to_string()),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["owner/repo#1"]
        );
        Ok(())
    }

    /// The approval queue carries each revision's exposure digest, so an approver sees which rows
    /// disclose one before opening any of them. A revision frozen by an engine with no extraction
    /// reads NULL, not an empty disclosure.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_approval_queue_carries_each_revisions_exposure_digest(pool: PgPool) -> Result<()> {
        let disclosed = crate::playbooks::exposure::Extraction::Declared(
            crate::playbooks::exposure::Exposure {
                version: 1,
                outputs: Vec::new(),
                capabilities: Vec::new(),
            },
        );
        for (key, extraction) in [("owner/repo#1", Some(&disclosed)), ("owner/repo#2", None)] {
            upsert_issue(&pool, &seed(key, "owner/repo", 0)).await?;
            assert!(claim_issue(&pool, key, Status::New, Status::AwaitingApproval).await?);
            let scope_id = insert_scope(
                &pool,
                &NewScope {
                    issue: key.to_string(),
                    pack_digest: Some("v1:abc".into()),
                    check_outcome: Some("PASS".into()),
                },
            )
            .await?;
            if let Some(extraction) = extraction {
                set_scope_exposure(&pool, scope_id, extraction).await?;
            }
            set_scope_approval_pr(&pool, scope_id, "https://github.com/owner/repo/pull/9").await?;
        }

        let rows = awaiting_approval_scopes(&pool).await?;
        assert_eq!(
            rows.iter()
                .map(|r| (r.key.as_str(), r.exposure_digest.is_some()))
                .collect::<Vec<_>>(),
            vec![("owner/repo#1", true), ("owner/repo#2", false)]
        );
        assert_eq!(
            rows[0].exposure_digest,
            Some(disclosed.declared().expect("a document").digest()?),
            "the queue shows the digest the approval binds to"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn direct_pack_adoption_lands_as_an_approved_frozen_scope(pool: PgPool) -> Result<()> {
        let key = "scenario:direct-pack-test";
        let scope_id = adopt_direct_pack(
            &pool,
            &crate::issues::model::NewDirectPack {
                key,
                title: "direct pack",
                body: "operator supplied pack",
                repo: "owner/repo",
                git_ref: "0123456789abcdef",
                pack_digest: "sha256:pack",
                created_by: "admin",
            },
        )
        .await?;

        let issue = get_issue(&pool, key).await?.expect("direct launch issue");
        assert_eq!(issue.status, Status::AwaitingApproval);
        assert_eq!(issue.git_ref.as_deref(), Some("0123456789abcdef"));
        let scope = latest_scope_for_issue(&pool, key)
            .await?
            .expect("frozen scope");
        assert_eq!(scope.id, scope_id);
        assert_eq!(scope.pack_digest.as_deref(), Some("sha256:pack"));
        assert_eq!(scope.check_outcome.as_deref(), Some("DIRECT"));
        assert_eq!(scope.approved_by.as_deref(), Some("admin"));
        assert!(scope.approved_at.is_some());
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn list_issues_filtered_by_upstream_since_excludes_older_and_null(
        pool: PgPool,
    ) -> Result<()> {
        seed_rows(&pool).await?;
        // A pre-backfill relic: no upstream_updated_at at all.
        upsert_issue(&pool, &seed("owner/repo#3", "owner/repo", 0)).await?;

        let q = IssueQuery {
            upstream_since: Some("2026-01-02T00:00:00Z".to_string()),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["owner/repo#2", "other/repo#1"],
            "the older row and the NULL relic are both out"
        );

        let rows = list_issues_filtered(&pool, &IssueQuery::default()).await?;
        assert_eq!(rows.len(), 4, "no cutoff keeps everything, NULL included");
        Ok(())
    }

    /// The issues page ships `recency=1y` by default, so `kind=scenario` arrives with an
    /// `upstream_since` attached. Adopted rows never carry an `upstream_updated_at`, and the
    /// cutoff used to drop them — the filter matched nothing, whatever the window.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn upstream_since_spares_kinds_with_no_upstream(pool: PgPool) -> Result<()> {
        seed_rows(&pool).await?;
        // A pre-backfill github relic: upstream-bearing kind, no stamp yet.
        upsert_issue(&pool, &seed("owner/repo#3", "owner/repo", 0)).await?;
        let scenario_key = adopt_scenario(
            &pool,
            "title",
            "body",
            &["owner/repo".to_string()],
            true,
            AdoptPins::default(),
            "octocat",
        )
        .await?;
        let jira_key = adopt_jira(
            &pool,
            "jira:example:ACME-1234",
            "jira title",
            "jira body",
            &["owner/repo".to_string()],
            false,
            "octocat",
        )
        .await?;

        let since = Some("2026-01-02T00:00:00Z".to_string());
        let q = IssueQuery {
            kind: Some(IssueKind::Scenario),
            upstream_since: since.clone(),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec![scenario_key.as_str()],
            "an adopted scenario survives the upstream-recency window"
        );

        let q = IssueQuery {
            kind: Some(IssueKind::Jira),
            upstream_since: since.clone(),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec![jira_key.as_str()],
            "so does an adopted jira row"
        );

        // The relic exclusion is unchanged: a github row with no stamp is still out.
        let q = IssueQuery {
            upstream_since: since,
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert!(!keys.contains(&"owner/repo#3"), "got {keys:?}");
        assert!(keys.contains(&scenario_key.as_str()), "got {keys:?}");
        assert!(keys.contains(&jira_key.as_str()), "got {keys:?}");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn list_issues_filtered_by_upstream_state(pool: PgPool) -> Result<()> {
        seed_rows(&pool).await?;
        park_issue(
            &pool,
            "owner/repo#1",
            &ParkReason::UpstreamClosed.to_string(),
            ParkedBy::Machine,
            "2026-01-05T00:00:00Z",
        )
        .await?;
        // A machine park for any other reason is still upstream-open.
        park_issue(
            &pool,
            "owner/repo#2",
            "stale: no upstream activity",
            ParkedBy::Machine,
            "2026-01-05T00:00:00Z",
        )
        .await?;

        let q = IssueQuery {
            upstream: Some(UpstreamState::Closed),
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["owner/repo#1"],
            "closed = exactly the retired shape"
        );

        let q = IssueQuery {
            upstream: Some(UpstreamState::Open),
            sort: SortKey::Priority,
            dir: crate::model::SortDir::Desc,
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["owner/repo#2", "other/repo#1"],
            "open = everything else, other machine parks included"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn list_issues_filtered_sorts_by_upstream_desc_nulls_last(pool: PgPool) -> Result<()> {
        seed_rows(&pool).await?;
        upsert_issue(&pool, &seed("owner/repo#3", "owner/repo", 0)).await?;

        let q = IssueQuery {
            sort: SortKey::Upstream,
            dir: crate::model::SortDir::Desc,
            ..Default::default()
        };
        let rows = list_issues_filtered(&pool, &q).await?;
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec![
                "owner/repo#2",
                "other/repo#1",
                "owner/repo#1",
                "owner/repo#3"
            ],
            "freshest upstream activity first, the NULL relic last"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn funnel_counts_buckets_every_stage(pool: PgPool) -> Result<()> {
        // discovered: fresh triage, no tier yet.
        upsert_issue(&pool, &seed("r/a#1", "r", 0)).await?;
        // ranked: `new` with a tier.
        upsert_issue(&pool, &seed("r/a#2", "r", 0)).await?;
        set_ranked_tier(&pool, "r/a#2", "T1", "perf", "hash-2").await?;
        // ranked: `scoped` also counts (already tiered by definition).
        upsert_issue(&pool, &seed("r/a#3", "r", 0)).await?;
        set_ranked_tier(&pool, "r/a#3", "T2", "perf", "hash-3").await?;
        sqlx::query("UPDATE issues SET status = 'scoped' WHERE key = 'r/a#3'")
            .execute(&pool)
            .await?;
        // awaiting_approval.
        upsert_issue(&pool, &seed("r/a#4", "r", 0)).await?;
        sqlx::query("UPDATE issues SET status = 'awaiting-approval' WHERE key = 'r/a#4'")
            .execute(&pool)
            .await?;
        // running.
        upsert_issue(&pool, &seed("r/a#5", "r", 0)).await?;
        sqlx::query("UPDATE issues SET status = 'running' WHERE key = 'r/a#5'")
            .execute(&pool)
            .await?;
        // pr_open.
        upsert_issue(&pool, &seed("r/a#6", "r", 0)).await?;
        sqlx::query("UPDATE issues SET status = 'pr-open' WHERE key = 'r/a#6'")
            .execute(&pool)
            .await?;
        // done.
        upsert_issue(&pool, &seed("r/a#7", "r", 0)).await?;
        sqlx::query("UPDATE issues SET status = 'done' WHERE key = 'r/a#7'")
            .execute(&pool)
            .await?;
        // parked, non-stale.
        upsert_issue(&pool, &seed("r/a#8", "r", 0)).await?;
        park_issue(
            &pool,
            "r/a#8",
            "no repro",
            ParkedBy::Machine,
            "2026-01-01T00:00:00Z",
        )
        .await?;
        // parked, stale disposition.
        upsert_issue(&pool, &seed("r/a#9", "r", 0)).await?;
        park_issue(
            &pool,
            "r/a#9",
            &ParkReason::StaleAlreadyImplemented {
                rationale: "already implemented".to_string(),
            }
            .to_string(),
            ParkedBy::Machine,
            "2026-01-01T00:00:00Z",
        )
        .await?;

        let counts = funnel_counts(&pool).await?;
        assert_eq!(counts.discovered, 1);
        assert_eq!(counts.ranked, 2);
        assert_eq!(counts.awaiting_approval, 1);
        assert_eq!(counts.running, 1);
        assert_eq!(counts.pr_open, 1);
        assert_eq!(counts.done, 1);
        assert_eq!(counts.parked, 2);
        assert_eq!(counts.stale, 1);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn funnel_counts_is_zero_on_an_empty_fleet(pool: PgPool) -> Result<()> {
        let counts = funnel_counts(&pool).await?;
        assert_eq!(counts.discovered, 0);
        assert_eq!(counts.ranked, 0);
        assert_eq!(counts.awaiting_approval, 0);
        assert_eq!(counts.running, 0);
        assert_eq!(counts.pr_open, 0);
        assert_eq!(counts.done, 0);
        assert_eq!(counts.parked, 0);
        assert_eq!(counts.stale, 0);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn clear_rank_nulls_only_the_cache_and_reports_a_miss(pool: PgPool) -> Result<()> {
        seed_rows(&pool).await?;

        let before = get_issue(&pool, "owner/repo#1").await?.expect("issue");
        assert!(clear_rank(&pool, "owner/repo#1").await?);
        let iss = get_issue(&pool, "owner/repo#1").await?.expect("issue");
        assert!(iss.ranked_content_hash.is_none(), "cache cleared");
        assert_eq!(
            iss.tier.as_deref(),
            Some("T0"),
            "the standing tier keeps gating until a fresh verdict lands"
        );
        assert!(
            iss.updated_at >= before.updated_at,
            "clear_rank stamps updated_at forward"
        );

        // A neighbor is untouched, and an unknown key is a miss, not an error.
        let other = get_issue(&pool, "owner/repo#2").await?.expect("issue");
        assert!(other.ranked_content_hash.is_some());
        assert!(!clear_rank(&pool, "owner/repo#404").await?);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn clear_rank_bulk_filters_by_scope_and_only_touches_new_rows(
        pool: PgPool,
    ) -> Result<()> {
        // Three ranked `new` rows (T0, T1, T0) from the shared seed, plus one unranked `new` row
        // and one ranked-but-scoped row (the sweep never ranks non-`new` rows, so bulk must skip it).
        seed_rows(&pool).await?;
        upsert_issue(&pool, &seed("owner/repo#9", "owner/repo", 0)).await?;
        upsert_issue(&pool, &seed("owner/repo#8", "owner/repo", 0)).await?;
        set_ranked_tier(&pool, "owner/repo#8", "T0", "perf", "hash-8").await?;
        assert!(claim_issue(&pool, "owner/repo#8", Status::New, Status::Scoped).await?);

        assert_eq!(
            clear_rank_bulk(&pool, RerankScope::Tier(crucible_contract::Tier::T1)).await?,
            1,
            "one `new` T1 row"
        );
        assert_eq!(
            clear_rank_bulk(&pool, RerankScope::Unranked).await?,
            1,
            "only the never-ranked row has tier IS NULL (a cleared cache keeps its standing tier)"
        );
        assert_eq!(
            clear_rank_bulk(&pool, RerankScope::All).await?,
            4,
            "every `new` row, ranked or not; the scoped row is skipped"
        );
        let scoped = get_issue(&pool, "owner/repo#8").await?.expect("issue");
        assert!(
            scoped.ranked_content_hash.is_some(),
            "non-`new` rows keep their cache"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn repo_health_includes_zero_issue_watched_repos_and_unwatched_issue_repos(
        pool: PgPool,
    ) -> Result<()> {
        // A freshly-watched repo with no issues yet still shows, at 0 total.
        insert_watched_repo(&pool, "fresh/repo", Some("env")).await?;
        // A repo with issues but no `repos` row (pre-Lane-O3 data shape) still shows too.
        upsert_issue(
            &pool,
            &crate::issues::model::NewIssue {
                key: "legacy/repo#1".to_string(),
                repo: "legacy/repo".to_string(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;

        let health = repo_health(&pool).await?;
        let by_repo: std::collections::HashMap<_, _> =
            health.iter().map(|r| (r.repo.clone(), r)).collect();
        let fresh = by_repo.get("fresh/repo").expect("fresh repo present");
        assert_eq!(fresh.total, 0);
        assert!(fresh.watched);
        assert!(!fresh.paused);
        assert_eq!(fresh.added_by.as_deref(), Some("env"));

        let legacy = by_repo.get("legacy/repo").expect("legacy repo present");
        assert_eq!(legacy.total, 1);
        // No `repos` row at all ⇒ COALESCE defaults: watched, unpaused, no provenance.
        assert!(legacy.watched);
        assert!(!legacy.paused);
        assert!(legacy.added_by.is_none());
        Ok(())
    }
}
