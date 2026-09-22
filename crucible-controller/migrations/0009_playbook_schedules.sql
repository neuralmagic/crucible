-- 0009_playbook_schedules.sql — recurring playbook launches: a cron expression, the timezone it is
-- read in, and the frozen authorization each firing launches. A firing inserts an ordinary
-- `playbook_launches` row, so downstream a scheduled run is indistinguishable from a manual one.
-- Same conventions as the baseline: TEXT RFC3339 UTC timestamps, DOUBLE PRECISION money, BIGINT
-- integers, and a DEFAULT on every NOT NULL column.
CREATE TABLE playbook_schedules (
    id                   TEXT PRIMARY KEY,
    playbook             TEXT NOT NULL REFERENCES playbooks(id),
    -- The validated `{name: value}` object every firing launches. JSONB because the dedupe cursor
    -- rewrites individual fields between firings.
    params               JSONB NOT NULL,
    schema_digest        TEXT NOT NULL,
    max_cost             DOUBLE PRECISION NOT NULL,
    max_time             TEXT NOT NULL,
    -- A schedule is the surface dedupe state belongs to, so its firings advance it by default.
    advance_dedupe       BOOLEAN NOT NULL DEFAULT TRUE,
    -- A five-field cron expression, parsed at write time and re-parsed on read.
    cron_expr            TEXT NOT NULL,
    -- The IANA zone the expression is read in (`UTC`, `America/New_York`).
    tz                   TEXT NOT NULL,
    enabled              BOOLEAN NOT NULL DEFAULT TRUE,
    -- The next firing, `%Y-%m-%dT%H:%M:%SZ` so the sweep's `next_due_at <= now` compares
    -- lexicographically. NULL when the schedule is disabled or has no further occurrence.
    next_due_at          TEXT,
    last_fired_at        TEXT,
    -- Reset by a firing that launches, raised by one that does not; at the configured threshold
    -- the schedule disables itself.
    consecutive_failures BIGINT NOT NULL DEFAULT 0,
    created_by           TEXT,
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL
);

CREATE INDEX playbook_schedules_due ON playbook_schedules (next_due_at) WHERE enabled;
CREATE INDEX playbook_schedules_playbook ON playbook_schedules (playbook);
