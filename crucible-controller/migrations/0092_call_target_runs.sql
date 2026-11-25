-- 0092_call_target_runs.sql — playbook calls (RFC-0008): one row per target run a call
-- dispatched, and the parent linkage of a playbook launch a call created.
--
-- A target run is identified by the calling run, the call task, the mapped instance and the
-- attempt; the unique index is what keeps a resumed caller from dispatching a second one.
-- `bound` is the audit of what left the calling run: every bound value, and each bound file as
-- its digest and size, never its bytes. `handle` is NULL while the dispatch is in flight.
CREATE TABLE call_target_runs (
    id           TEXT PRIMARY KEY,
    parent_run   TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    task         TEXT NOT NULL,
    instance     TEXT,
    attempt      INTEGER NOT NULL CHECK (attempt >= 0),
    alias        TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('playbook', 'github_workflow')),
    target       JSONB NOT NULL,
    interface    JSONB NOT NULL,
    handle       JSONB,
    detail       JSONB NOT NULL DEFAULT '{}'::jsonb,
    bound        JSONB NOT NULL,
    budget       JSONB,
    status       TEXT NOT NULL
        CHECK (status IN ('pending', 'running', 'passed', 'failed', 'cancelled')),
    note         TEXT NOT NULL DEFAULT '',
    result       JSONB NOT NULL DEFAULT 'null'::jsonb,
    result_files JSONB NOT NULL DEFAULT '[]'::jsonb,
    progress     JSONB NOT NULL DEFAULT '{"nodes": [], "edges": [], "omitted": 0}'::jsonb,
    link         JSONB,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    ended_at     TEXT
);
CREATE UNIQUE INDEX call_target_runs_identity
    ON call_target_runs (parent_run, task, (COALESCE(instance, '')), attempt);
CREATE INDEX call_target_runs_open ON call_target_runs (parent_run) WHERE ended_at IS NULL;

-- The bytes of a file a call binds, uploaded by the calling run before the dispatch that names it.
CREATE TABLE call_input_files (
    run_id     TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    digest     TEXT NOT NULL,
    size       BIGINT NOT NULL,
    bytes      BYTEA NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (run_id, digest)
);

-- A launch a call created: the run and call task that dispatched it, how deep in a chain of
-- calling runs it is (0 for a launch no call made), and the run id its first dispatch takes, so
-- the call's handle names the run before it exists.
ALTER TABLE playbook_launches
    ADD COLUMN parent_run TEXT,
    ADD COLUMN parent_task TEXT,
    ADD COLUMN parent_instance TEXT,
    ADD COLUMN call_depth INTEGER NOT NULL DEFAULT 0 CHECK (call_depth >= 0),
    ADD COLUMN reserved_run TEXT;
CREATE INDEX playbook_launches_parent_run ON playbook_launches (parent_run)
    WHERE parent_run IS NOT NULL;

ALTER TABLE playbook_launches DROP CONSTRAINT playbook_launches_origin_check;
ALTER TABLE playbook_launches
    ADD CONSTRAINT playbook_launches_origin_check
    CHECK (origin IN ('manual', 'deferred', 'schedule', 'draft', 'watch', 'webhook', 'call'));
