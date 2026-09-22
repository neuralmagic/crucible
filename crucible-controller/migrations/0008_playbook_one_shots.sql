-- 0008_playbook_one_shots.sql — the one-shot surface: where a launch came from, whether it may
-- advance the schedule dedupe state, and the deferred launches waiting for their fire_at.
-- Same conventions as the baseline: TEXT RFC3339 UTC timestamps, DOUBLE PRECISION money, and a
-- DEFAULT on every added NOT NULL column.

-- 'manual' = a form POST, 'deferred' = a one-shot's fire_at came due, 'schedule' = a cron sweep.
-- Downstream a launch is a launch; the runs surface reads this only to keep scheduled firings off
-- the one-shot section.
ALTER TABLE playbook_launches
    ADD COLUMN origin TEXT NOT NULL DEFAULT 'manual'
        CHECK (origin IN ('manual', 'deferred', 'schedule')),
    -- Opted in at launch. FALSE leaves the cursor and the seen-set exactly where the last
    -- scheduled firing left them.
    ADD COLUMN advance_dedupe BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX playbook_launches_origin ON playbook_launches (origin);

-- A run-once-at launch: the frozen authorization plus the instant it becomes due. It fires once
-- (the sweep's CAS claim is what makes that true) and completes; a recurring shape is a schedule,
-- not this.
CREATE TABLE playbook_one_shots (
    id             TEXT PRIMARY KEY,
    playbook       TEXT NOT NULL REFERENCES playbooks(id),
    -- The validated `{name: value}` object, frozen at creation: the firing launches these values,
    -- not whatever the form would produce today.
    params         JSONB NOT NULL,
    schema_digest  TEXT NOT NULL,
    max_cost       DOUBLE PRECISION NOT NULL,
    max_time       TEXT NOT NULL,
    advance_dedupe BOOLEAN NOT NULL DEFAULT FALSE,
    -- Normalized to `%Y-%m-%dT%H:%M:%SZ` on write, so the sweep's `fire_at <= now` compares
    -- lexicographically the way every other stamp in this schema does.
    fire_at        TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'fired', 'canceled')),
    -- The launch the firing minted, NULL until then.
    fired_key      TEXT REFERENCES issues(key) ON DELETE SET NULL,
    fired_at       TEXT,
    created_by     TEXT,
    created_at     TEXT NOT NULL
);

CREATE INDEX playbook_one_shots_due ON playbook_one_shots (fire_at) WHERE status = 'pending';
CREATE INDEX playbook_one_shots_playbook ON playbook_one_shots (playbook);
