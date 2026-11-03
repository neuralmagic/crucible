//! `decision_requests` rows. Expiry is evaluated in SQL against `expires_at`, so the database clock
//! is the one every reader and the answer path agree on.

use anyhow::{Context, Result};
use crucible_contract::decision::{Question, QuestionId};
use crucible_contract::decision_request::{
    AnswerRecord, Evidence, OpenRequest, RequestState, RequestStatus,
};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;

/// One request as stored.
#[derive(Debug, Clone)]
pub(crate) struct Decision {
    pub(crate) id: String,
    pub(crate) run_id: String,
    pub(crate) task: String,
    pub(crate) launch_key: Option<String>,
    pub(crate) questions: BTreeMap<QuestionId, Question>,
    pub(crate) evidence: Evidence,
    pub(crate) evidence_digest: String,
    pub(crate) opened_at: String,
    pub(crate) expires_at: String,
    pub(crate) status: RequestStatus,
}

impl Decision {
    pub(crate) fn state(&self) -> RequestState {
        RequestState {
            id: self.id.clone(),
            evidence_digest: self.evidence_digest.clone(),
            expires_at: self.expires_at.clone(),
            status: self.status.clone(),
        }
    }
}

/// Every column, with timestamps as RFC 3339 text and an open request past its expiry read as
/// expired.
macro_rules! select {
    () => {
        "SELECT id, run_id, task, launch_key, questions, evidence, evidence_digest, \
         to_char(opened_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS opened_at, \
         to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS expires_at, \
         CASE WHEN state = 'open' AND expires_at <= now() THEN 'expired' ELSE state END AS state, \
         labels, decided_by, \
         to_char(decided_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS decided_at, note \
         FROM decision_requests"
    };
}

#[derive(Debug, thiserror::Error)]
enum StoreError {
    #[error("decision request in unknown state {0:?}")]
    UnknownState(String),
    #[error("decision request {0} vanished")]
    Vanished(String),
}

fn decode(row: &sqlx::postgres::PgRow) -> Result<Decision> {
    let state: String = row.try_get("state")?;
    let evidence_digest: String = row.try_get("evidence_digest")?;
    let status = match state.as_str() {
        "open" => RequestStatus::Open,
        "expired" => RequestStatus::Expired,
        "withdrawn" => RequestStatus::Withdrawn,
        "answered" => {
            let labels: serde_json::Value = row.try_get("labels")?;
            RequestStatus::Answered {
                answer: AnswerRecord {
                    labels: serde_json::from_value(labels).context("decoding answer labels")?,
                    decided_by: row.try_get("decided_by")?,
                    decided_at: row.try_get("decided_at")?,
                    evidence_digest: evidence_digest.clone(),
                    note: row.try_get("note")?,
                },
            }
        }
        other => return Err(StoreError::UnknownState(other.to_owned()).into()),
    };
    let questions: serde_json::Value = row.try_get("questions")?;
    let evidence: serde_json::Value = row.try_get("evidence")?;
    Ok(Decision {
        id: row.try_get("id")?,
        run_id: row.try_get("run_id")?,
        task: row.try_get("task")?,
        launch_key: row.try_get("launch_key")?,
        questions: serde_json::from_value(questions).context("decoding questions")?,
        evidence: serde_json::from_value(evidence).context("decoding evidence")?,
        evidence_digest,
        opened_at: row.try_get("opened_at")?,
        expires_at: row.try_get("expires_at")?,
        status,
    })
}

/// Open the run's request for `task`, or return the one already there: a reopen never resets the
/// expiry or replaces the evidence.
pub(crate) async fn open(
    pool: &PgPool,
    run_id: &str,
    launch_key: Option<&str>,
    request: &OpenRequest,
) -> Result<(Decision, bool)> {
    let id = uuid::Uuid::now_v7().to_string();
    let inserted = sqlx::query(
        "INSERT INTO decision_requests \
         (id, run_id, task, launch_key, questions, evidence, evidence_digest, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(secs => $8)) \
         ON CONFLICT (run_id, task) DO NOTHING",
    )
    .bind(&id)
    .bind(run_id)
    .bind(&request.task)
    .bind(launch_key)
    .bind(serde_json::to_value(&request.questions)?)
    .bind(serde_json::to_value(&request.evidence)?)
    .bind(&request.evidence_digest)
    .bind(request.timeout_secs as f64)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    let row = sqlx::query(concat!(select!(), " WHERE run_id = $1 AND task = $2"))
        .bind(run_id)
        .bind(&request.task)
        .fetch_one(pool)
        .await?;
    Ok((decode(&row)?, inserted))
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<Decision>> {
    let row = sqlx::query(concat!(select!(), " WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(decode).transpose()
}

/// Open requests, newest first.
pub(crate) async fn list_open(pool: &PgPool) -> Result<Vec<Decision>> {
    let rows = sqlx::query(concat!(
        select!(),
        " WHERE state = 'open' AND expires_at > now() ORDER BY opened_at DESC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(decode).collect()
}

/// Why an answer was not recorded.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Answered, expired, or withdrawn already; carries the state.
    Closed(&'static str),
    /// The answer names evidence other than the request's.
    StaleEvidence,
}

/// Record the first answer. A repeat of the recorded answer by the same principal is acknowledged
/// as `Ok(false)`; anything else on a closed request is refused.
pub(crate) async fn answer(
    conn: &mut sqlx::PgConnection,
    id: &str,
    labels: &BTreeMap<QuestionId, Vec<String>>,
    evidence_digest: &str,
    decided_by: &str,
    note: Option<&str>,
) -> Result<Result<bool, Refused>> {
    let labels_json = serde_json::to_value(labels)?;
    let updated = sqlx::query(
        "UPDATE decision_requests SET state = 'answered', labels = $2, decided_by = $3, \
         decided_at = now(), note = $4 \
         WHERE id = $1 AND state = 'open' AND expires_at > now() AND evidence_digest = $5",
    )
    .bind(id)
    .bind(&labels_json)
    .bind(decided_by)
    .bind(note)
    .bind(evidence_digest)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 1 {
        return Ok(Ok(true));
    }
    let row = sqlx::query(concat!(select!(), " WHERE id = $1"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(current) = row.as_ref().map(decode).transpose()? else {
        return Err(StoreError::Vanished(id.to_owned()).into());
    };
    Ok(Err(match &current.status {
        RequestStatus::Open if current.evidence_digest != evidence_digest => Refused::StaleEvidence,
        RequestStatus::Open => Refused::Closed("open"),
        RequestStatus::Answered { answer }
            if answer.decided_by == decided_by
                && &answer.labels == labels
                && answer.evidence_digest == evidence_digest =>
        {
            return Ok(Ok(false));
        }
        RequestStatus::Answered { .. } => Refused::Closed("answered"),
        RequestStatus::Expired => Refused::Closed("expired"),
        RequestStatus::Withdrawn => Refused::Closed("withdrawn"),
    }))
}

/// Withdraw every open request of a run that ended, returning them as withdrawn.
pub(crate) async fn withdraw_run(pool: &PgPool, run_id: &str) -> Result<Vec<Decision>> {
    let ids: Vec<String> = sqlx::query_scalar(
        "UPDATE decision_requests SET state = 'withdrawn' \
         WHERE run_id = $1 AND state = 'open' AND expires_at > now() RETURNING id",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    let mut withdrawn = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(d) = get(pool, &id).await? {
            withdrawn.push(d);
        }
    }
    Ok(withdrawn)
}
