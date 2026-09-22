//! The outer loop's history: the append-only `events` table. The ledger rows are what *is*; the
//! events table is what *happened*. One row per state transition, preserving the frozen NDJSON
//! line shape (`from`/`to` are Postgres reserved words, so the columns are `from_status`/
//! `to_status`; [`export_ndjson`] maps them back):
//!
//! ```json
//! {"v":1,"ts":"2026-07-02T12:34:56Z","key":"owner/repo#7","from":"new","to":"scoped","reason":null,"evidence":null,"actor":null}
//! ```
//!
//! Beyond the table, every append is also published on an in-process broadcast channel
//! ([`EventLog::subscribe`]) — the live feed behind `GET /api/events/stream` and the `/activity`
//! page. The channel is best-effort by design: no subscribers means the send is dropped, a slow
//! subscriber is lagged past, and the table stays the only durable record.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::{PgExecutor, PgPool, Row};
use tokio::sync::broadcast;

/// One transition line. `reason`/`evidence`/`actor` are always present (null when absent) so the
/// shape is stable for a reader; `ts` is RFC3339 (jiff), matching the session log's stamp format.
/// `actor` (an additive v1 field — older lines simply lack the key) is the human a transition
/// traces back to: the oauth2-proxy identity on an override, null for the machine's own moves.
#[derive(Debug, Serialize)]
pub struct Event<'a> {
    pub(crate) v: i64,
    pub(crate) ts: String,
    pub(crate) key: &'a str,
    pub(crate) from: &'a str,
    pub(crate) to: &'a str,
    pub(crate) reason: Option<&'a str>,
    pub(crate) evidence: Option<&'a str>,
    pub(crate) actor: Option<&'a str>,
}

impl<'a> Event<'a> {
    /// Build a v1 event, stamping `ts` with the current RFC3339 UTC instant.
    pub fn now(
        key: &'a str,
        from: &'a str,
        to: &'a str,
        reason: Option<&'a str>,
        evidence: Option<&'a str>,
    ) -> Self {
        Event {
            v: 1,
            ts: crate::clock::now_rfc3339(),
            key,
            from,
            to,
            reason,
            evidence,
            actor: None,
        }
    }

    /// Attribute this event to a human actor (builder-style, so the many machine-side
    /// [`Event::now`] call sites stay untouched).
    pub(crate) fn by(mut self, actor: Option<&'a str>) -> Self {
        self.actor = actor;
        self
    }
}

/// One decoded row, for readers (the issue-detail event history) and for the live broadcast.
/// The owned counterpart of [`Event`], which borrows for writing. `Serialize` because the SSE
/// stream and the NDJSON export re-emit it as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    v: i64,
    pub(crate) ts: String,
    pub(crate) key: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) reason: Option<String>,
    pub(crate) evidence: Option<String>,
    /// Absent on lines written before the field existed — `default` keeps them parseable.
    #[serde(default)]
    pub(crate) actor: Option<String>,
}

/// How many not-yet-received events the live channel buffers per subscriber before it starts
/// lagging (skipping) — enough to ride out a render, small enough to bound memory.
const LIVE_CAPACITY: usize = 256;

/// How long a live tail sleeps between table polls when no local append wakes it — the worst-case
/// latency for seeing another process's writes (a standby tailing the leader's events).
const TAIL_POLL: std::time::Duration = std::time::Duration::from_millis(1500);

/// How old (by its `ts` stamp) a fetched row must be before the tail's floor advances past its id.
/// Identity ids are allocated at INSERT time, mid-transaction, so a poll can see id N+1 while N's
/// transaction is still committing; rows above the floor are re-read and deduped every poll, and
/// the floor only passes ids whose neighbors can no longer appear. An event whose transaction
/// stays open longer than this between insert and commit could still be skipped — the writers are
/// short few-statement transactions, nowhere near the bound.
const TAIL_FLOOR_GRACE_SECS: i64 = 15;

/// Insert one event row through `ex` — a pool for a standalone append, or a transaction so the
/// row commits atomically with the state transition it records ([`crate::issues::transitions::transition`]).
pub(crate) async fn insert(ex: impl PgExecutor<'_>, ev: &Event<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO events (v, ts, key, from_status, to_status, reason, evidence, actor)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(ev.v)
    .bind(&ev.ts)
    .bind(ev.key)
    .bind(ev.from)
    .bind(ev.to)
    .bind(ev.reason)
    .bind(ev.evidence)
    .bind(ev.actor)
    .execute(ex)
    .await
    .context("inserting controller event")?;
    Ok(())
}

fn record_of_row(row: &sqlx::postgres::PgRow) -> EventRecord {
    EventRecord {
        v: row.get("v"),
        ts: row.get("ts"),
        key: row.get("key"),
        from: row.get("from_status"),
        to: row.get("to_status"),
        reason: row.get("reason"),
        evidence: row.get("evidence"),
        actor: row.get("actor"),
    }
}

const SELECT_COLUMNS: &str = "v, ts, key, from_status, to_status, reason, evidence, actor";

/// Write every event, oldest first, as NDJSON in the frozen line shape — the disaster-case export
/// (`crucible-controller db export-events`) and the offline-analysis format. Returns the line
/// count.
pub async fn export_ndjson(pool: &PgPool, out: &mut dyn std::io::Write) -> Result<usize> {
    let records = EventLog::new(pool.clone()).read_all().await?;
    for rec in &records {
        let mut line =
            serde_json::to_string(rec).context("serialize controller event for export")?;
        line.push('\n');
        out.write_all(line.as_bytes())
            .context("writing the NDJSON export")?;
    }
    Ok(records.len())
}

/// The whole table as one NDJSON string — the test-side stand-in for reading the old file.
#[cfg(test)]
pub(crate) async fn export_string(pool: &PgPool) -> Result<String> {
    let mut buf = Vec::new();
    export_ndjson(pool, &mut buf).await?;
    String::from_utf8(buf).context("export is utf-8")
}

/// The events-table handle: the ledger pool plus the live broadcast channel. Cheap to clone
/// (clones share the channel).
#[derive(Clone)]
pub struct EventLog {
    pool: PgPool,
    live: broadcast::Sender<EventRecord>,
}

impl std::fmt::Debug for EventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventLog").finish_non_exhaustive()
    }
}

impl EventLog {
    pub(crate) fn new(pool: PgPool) -> Self {
        EventLog {
            pool,
            live: broadcast::channel(LIVE_CAPACITY).0,
        }
    }

    /// Subscribe to the live feed: every event appended through *this log and its clones* from
    /// now on. History comes from [`EventLog::read_all`]/[`EventLog::read_recent`]; a subscriber
    /// that falls more than [`LIVE_CAPACITY`] events behind sees `Lagged` and should just resume.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<EventRecord> {
        self.live.subscribe()
    }

    /// Insert `ev` as one row, then publish it to the live feed.
    pub async fn append(&self, ev: &Event<'_>) -> Result<()> {
        insert(&self.pool, ev).await?;
        self.publish(ev);
        Ok(())
    }

    /// Publish to the live feed. Err = no subscribers, which is fine. Called after the durable
    /// write — by [`EventLog::append`], and by [`crate::issues::transitions::transition`]/`park`/`unpark`
    /// once their transaction commits.
    pub(crate) fn publish(&self, ev: &Event<'_>) {
        let _ = self.live.send(EventRecord {
            v: ev.v,
            ts: ev.ts.clone(),
            key: ev.key.to_string(),
            from: ev.from.to_string(),
            to: ev.to.to_string(),
            reason: ev.reason.map(str::to_string),
            evidence: ev.evidence.map(str::to_string),
            actor: ev.actor.map(str::to_string),
        });
    }

    /// Read every recorded event, oldest first.
    pub(crate) async fn read_all(&self) -> Result<Vec<EventRecord>> {
        let rows = sqlx::query(&format!("SELECT {SELECT_COLUMNS} FROM events ORDER BY id"))
            .fetch_all(&self.pool)
            .await
            .context("reading the events table")?;
        Ok(rows.iter().map(record_of_row).collect())
    }

    /// The `limit` most recent events, newest first — the activity feed's starting tail.
    pub(crate) async fn read_recent(&self, limit: usize) -> Result<Vec<EventRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {SELECT_COLUMNS} FROM events ORDER BY id DESC LIMIT $1"
        ))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .context("reading the events table tail")?;
        Ok(rows.iter().map(record_of_row).collect())
    }

    /// Every event recorded for one issue key, oldest first.
    pub(crate) async fn read_for_key(&self, key: &str) -> Result<Vec<EventRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {SELECT_COLUMNS} FROM events WHERE key = $1 ORDER BY id"
        ))
        .bind(key)
        .fetch_all(&self.pool)
        .await
        .context("reading the events table for one key")?;
        Ok(rows.iter().map(record_of_row).collect())
    }

    /// The newest event's id, 0 when the table is empty — where a live tail starts.
    pub(crate) async fn latest_id(&self) -> Result<i64> {
        let id: Option<i64> = sqlx::query_scalar("SELECT MAX(id) FROM events")
            .fetch_one(&self.pool)
            .await
            .context("reading the newest event id")?;
        Ok(id.unwrap_or(0))
    }

    /// Events with `id > after`, oldest first, each with its id.
    async fn read_after(&self, after: i64) -> Result<Vec<(i64, EventRecord)>> {
        let rows = sqlx::query(&format!(
            "SELECT id, {SELECT_COLUMNS} FROM events WHERE id > $1 ORDER BY id"
        ))
        .bind(after)
        .fetch_all(&self.pool)
        .await
        .context("reading the events table after an id")?;
        Ok(rows
            .iter()
            .map(|row| (row.get::<i64, _>("id"), record_of_row(row)))
            .collect())
    }

    /// A live tail over the table: every event with `id > after`, exactly once, in near-id order
    /// (a row whose transaction commits after a higher id was already seen is delivered when it
    /// becomes visible, not skipped — identity ids commit out of order). Rows are always read from
    /// the table, so writes from *other processes* (the active leader, while this replica stands
    /// by) appear too — the local broadcast channel only wakes the poll early, and [`TAIL_POLL`]
    /// bounds the cross-process latency. The query floor trails the newest id by
    /// [`TAIL_FLOOR_GRACE_SECS`] of `ts` age, with already-emitted ids deduped in between. Ends
    /// when the channel closes (every sender dropped: daemon shutdown).
    pub(crate) fn tail_after(
        &self,
        after: i64,
    ) -> impl futures_util::Stream<Item = (i64, EventRecord)> + Send + 'static + use<> {
        use std::collections::{BTreeSet, VecDeque};
        let state = (
            self.clone(),
            self.subscribe(),
            after,
            BTreeSet::<i64>::new(),
            VecDeque::<(i64, EventRecord)>::new(),
        );
        futures_util::stream::unfold(
            state,
            |(log, mut rx, mut floor, mut emitted, mut buf)| async move {
                loop {
                    if let Some(rec) = buf.pop_front() {
                        return Some((rec, (log, rx, floor, emitted, buf)));
                    }

                    if let Ok(rows) = log.read_after(floor).await {
                        let cutoff = crate::clock::stamp(
                            jiff::Timestamp::now()
                                - jiff::Span::new().seconds(TAIL_FLOOR_GRACE_SECS),
                        );
                        let mut new_floor = floor;
                        for (id, rec) in rows {
                            if rec.ts.as_str() <= cutoff.as_str() && id > new_floor {
                                new_floor = id;
                            }
                            if emitted.insert(id) {
                                buf.push_back((id, rec));
                            }
                        }
                        if new_floor > floor {
                            floor = new_floor;
                            emitted = emitted.split_off(&(floor + 1));
                        }
                        if !buf.is_empty() {
                            continue;
                        }
                    }
                    // Nothing new (or a transient read error): wake on a local append or the poll
                    // tick, then re-query. A burst of appends collapses into one query.
                    match tokio::time::timeout(TAIL_POLL, rx.recv()).await {
                        Ok(Err(broadcast::error::RecvError::Closed)) => return None,
                        _ => while rx.try_recv().is_ok() {},
                    }
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn export_emits_the_frozen_line_shape(pool: PgPool) -> Result<()> {
        let log = EventLog::new(pool.clone());
        log.append(&Event::now(
            "owner/repo#7",
            "new",
            "scoped",
            Some("proposed pack passed check"),
            Some("https://github.com/owner/repo/pull/9"),
        ))
        .await?;
        // A second line with the nullable fields absent — the shape must still carry the keys.
        log.append(&Event::now("owner/repo#8", "scoped", "parked", None, None))
            .await?;

        let mut buf = Vec::new();
        let n = export_ndjson(&pool, &mut buf).await?;
        assert_eq!(n, 2, "one line per append");
        let body = String::from_utf8(buf)?;
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0])?;
        assert_eq!(first["v"], 1);
        assert_eq!(first["key"], "owner/repo#7");
        assert_eq!(first["from"], "new");
        assert_eq!(first["to"], "scoped");
        assert_eq!(first["reason"], "proposed pack passed check");
        assert_eq!(first["evidence"], "https://github.com/owner/repo/pull/9");
        // ts is RFC3339 UTC (…Z), present on every line.
        let ts = first["ts"].as_str().unwrap_or_default();
        assert!(ts.ends_with('Z') && ts.contains('T'), "ts is RFC3339: {ts}");

        // Every key of the frozen shape is present, and the absent optionals are null (not dropped).
        let second: serde_json::Value = serde_json::from_str(lines[1])?;
        for k in [
            "v", "ts", "key", "from", "to", "reason", "evidence", "actor",
        ] {
            assert!(
                second.get(k).is_some(),
                "line must carry key `{k}`: {lines:?}"
            );
        }
        assert!(second["reason"].is_null());
        assert!(second["evidence"].is_null());
        assert!(second["actor"].is_null());
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn actor_attribution_round_trips(pool: PgPool) -> Result<()> {
        let log = EventLog::new(pool);
        log.append(
            &Event::now("owner/repo#7", "new", "parked", Some("no repro"), None).by(Some("wren")),
        )
        .await?;
        let events = log.read_all().await?;
        assert_eq!(events[0].actor.as_deref(), Some("wren"));

        // A pre-actor jsonl line (the field's key absent entirely) parses with actor = None —
        // the migration tool reads old files through this type.
        let old = r#"{"v":1,"ts":"2026-07-02T12:34:56Z","key":"a/b#1","from":"new","to":"scoped","reason":null,"evidence":null}"#;
        let rec: EventRecord = serde_json::from_str(old)?;
        assert!(rec.actor.is_none());
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn append_publishes_to_live_subscribers_and_clones_share_the_channel(
        pool: PgPool,
    ) -> Result<()> {
        let log = EventLog::new(pool);
        let clone = log.clone();
        let mut rx = log.subscribe();

        // An append through the *clone* reaches a subscriber on the original.
        clone
            .append(&Event::now(
                "owner/repo#7",
                "new",
                "scoped",
                Some("pack passed check"),
                None,
            ))
            .await?;
        let got = rx.try_recv().expect("event published");
        assert_eq!(got.key, "owner/repo#7");
        assert_eq!(got.to, "scoped");
        assert_eq!(got.reason.as_deref(), Some("pack passed check"));

        // No subscribers at append time is not an error, and a later subscriber starts fresh
        // (history comes from the table, not the channel).
        drop(rx);
        clone
            .append(&Event::now("owner/repo#8", "new", "parked", None, None))
            .await?;
        let mut late = log.subscribe();
        assert!(late.try_recv().is_err(), "live feed carries no history");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn tail_sees_rows_from_another_handle_and_local_appends_in_order(
        pool: PgPool,
    ) -> Result<()> {
        use futures_util::StreamExt;
        let local = EventLog::new(pool.clone());
        // A separate handle over the same pool shares no broadcast channel — the other-process
        // (standby-tails-the-leader) case; only the table connects them.
        let foreign = EventLog::new(pool);

        local
            .append(&Event::now("owner/repo#0", "new", "scoped", None, None))
            .await?;
        let mut tail = Box::pin(local.tail_after(local.latest_id().await?));

        local
            .append(&Event::now("owner/repo#1", "new", "scoped", None, None))
            .await?;
        foreign
            .append(&Event::now("owner/repo#2", "new", "parked", None, None))
            .await?;

        let deadline = std::time::Duration::from_secs(10);
        let first = tokio::time::timeout(deadline, tail.next())
            .await?
            .expect("tail stays open");
        let second = tokio::time::timeout(deadline, tail.next())
            .await?
            .expect("tail stays open");
        assert_eq!(first.1.key, "owner/repo#1", "pre-tail history is skipped");
        assert_eq!(second.1.key, "owner/repo#2", "another handle's row appears");
        Ok(())
    }

    /// Identity ids commit out of order: a transaction that allocated a lower id can commit after
    /// a higher id is already visible. The tail must deliver the late row when it lands instead of
    /// leaving it behind the cursor forever.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn tail_delivers_a_lower_id_row_that_commits_after_a_higher_one(
        pool: PgPool,
    ) -> Result<()> {
        use futures_util::StreamExt;
        let log = EventLog::new(pool.clone());
        log.append(&Event::now("owner/repo#0", "new", "scoped", None, None))
            .await?;
        let mut tail = Box::pin(log.tail_after(log.latest_id().await?));

        // Allocate the lower id inside an open transaction, then commit a higher-id row first.
        let mut tx = pool.begin().await?;
        insert(
            &mut *tx,
            &Event::now("owner/repo#1", "new", "parked", None, None),
        )
        .await?;
        log.append(&Event::now("owner/repo#2", "new", "scoped", None, None))
            .await?;

        let deadline = std::time::Duration::from_secs(10);
        let first = tokio::time::timeout(deadline, tail.next())
            .await?
            .expect("tail stays open");
        assert_eq!(
            first.1.key, "owner/repo#2",
            "the committed row appears first"
        );

        tx.commit().await?;
        let second = tokio::time::timeout(deadline, tail.next())
            .await?
            .expect("tail stays open");
        assert_eq!(
            second.1.key, "owner/repo#1",
            "the late-committing lower id is delivered, not skipped"
        );

        // And the already-emitted higher id is never re-delivered.
        log.append(&Event::now("owner/repo#3", "new", "scoped", None, None))
            .await?;
        let third = tokio::time::timeout(deadline, tail.next())
            .await?
            .expect("tail stays open");
        assert_eq!(third.1.key, "owner/repo#3", "no duplicate of owner/repo#2");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn read_recent_returns_the_tail_newest_first(pool: PgPool) -> Result<()> {
        let log = EventLog::new(pool);
        for i in 0..5 {
            log.append(&Event::now(
                &format!("owner/repo#{i}"),
                "new",
                "scoped",
                None,
                None,
            ))
            .await?;
        }
        let recent = log.read_recent(3).await?;
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].key, "owner/repo#4", "newest first");
        assert_eq!(recent[2].key, "owner/repo#2");
        assert_eq!(
            log.read_recent(99).await?.len(),
            5,
            "limit past the end is fine"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn read_all_is_empty_before_any_transition(pool: PgPool) -> Result<()> {
        let log = EventLog::new(pool);
        assert!(log.read_all().await?.is_empty());
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn read_for_key_filters_to_one_issue_in_insert_order(pool: PgPool) -> Result<()> {
        let log = EventLog::new(pool);
        log.append(&Event::now("owner/repo#1", "new", "scoped", None, None))
            .await?;
        log.append(&Event::now(
            "owner/repo#2",
            "new",
            "parked",
            Some("no repro"),
            None,
        ))
        .await?;
        log.append(&Event::now(
            "owner/repo#1",
            "scoped",
            "awaiting-approval",
            None,
            None,
        ))
        .await?;

        let all = log.read_all().await?;
        assert_eq!(all.len(), 3);

        let mine = log.read_for_key("owner/repo#1").await?;
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].to, "scoped");
        assert_eq!(mine[1].to, "awaiting-approval");
        assert!(log.read_for_key("owner/repo#404").await?.is_empty());
        Ok(())
    }
}
