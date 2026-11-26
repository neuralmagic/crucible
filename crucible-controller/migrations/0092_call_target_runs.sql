CREATE TABLE call_target_runs (
    id           TEXT PRIMARY KEY,
    parent_run   TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    task         TEXT NOT NULL,
    instance     TEXT,
    attempt      INTEGER NOT NULL CHECK (attempt >= 0),
    alias        TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('playbook', 'github_workflow', 'tekton')),
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
    ended_at     TEXT,
    cancel_owed  BOOLEAN NOT NULL DEFAULT false
);
CREATE UNIQUE INDEX call_target_runs_identity
    ON call_target_runs (parent_run, task, (COALESCE(instance, '')), attempt);
CREATE INDEX call_target_runs_open ON call_target_runs (parent_run) WHERE ended_at IS NULL;
CREATE INDEX call_target_runs_due ON call_target_runs (updated_at)
    WHERE handle IS NOT NULL AND (ended_at IS NULL OR cancel_owed);

CREATE TABLE call_input_files (
    run_id     TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    digest     TEXT NOT NULL,
    size       BIGINT NOT NULL,
    bytes      BYTEA NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (run_id, digest)
);

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

CREATE TABLE launch_param_files (
    launch     TEXT NOT NULL REFERENCES playbook_launches(key) ON DELETE CASCADE,
    digest     TEXT NOT NULL,
    size       BIGINT NOT NULL CHECK (size >= 0),
    media_type TEXT,
    bytes      BYTEA NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (launch, digest)
);

CREATE TABLE param_file_uploads (
    uploader   TEXT NOT NULL,
    digest     TEXT NOT NULL,
    size       BIGINT NOT NULL CHECK (size >= 0),
    bytes      BYTEA NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (uploader, digest)
);

CREATE TABLE call_result_files (
    call_id    TEXT NOT NULL REFERENCES call_target_runs(id) ON DELETE CASCADE,
    path       TEXT NOT NULL,
    digest     TEXT NOT NULL,
    size       BIGINT NOT NULL CHECK (size >= 0),
    bytes      BYTEA NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (call_id, path)
);
