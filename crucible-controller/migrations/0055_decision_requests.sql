-- Human-decided routes (RFC-0002 C-HUMAN-DECISION): one request per run and route task. The run
-- opens it and polls it; a person allowed `run:approve` answers it against its evidence digest.
CREATE TABLE decision_requests (
    id              TEXT PRIMARY KEY,
    run_id          TEXT NOT NULL,
    task            TEXT NOT NULL,
    -- The launch the run belongs to, for the live event key and the approver set.
    launch_key      TEXT,
    questions       JSONB NOT NULL,
    evidence        JSONB NOT NULL,
    evidence_digest TEXT NOT NULL,
    opened_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ NOT NULL,
    state           TEXT NOT NULL DEFAULT 'open'
        CHECK (state IN ('open', 'answered', 'expired', 'withdrawn')),
    labels          JSONB,
    decided_by      TEXT,
    decided_at      TIMESTAMPTZ,
    note            TEXT,
    UNIQUE (run_id, task),
    CHECK ((state = 'answered') = (labels IS NOT NULL AND decided_by IS NOT NULL AND decided_at IS NOT NULL))
);
CREATE INDEX decision_requests_open ON decision_requests (state, expires_at) WHERE state = 'open';

-- The ingest credential a local run's controller mints at spawn: the run presents it as the bearer
-- for its `local-<run>` pod name. Only the digest is kept.
CREATE TABLE local_run_tokens (
    pod          TEXT PRIMARY KEY,
    run_id       TEXT NOT NULL,
    token_sha256 TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
