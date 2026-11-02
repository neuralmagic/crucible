-- 0056_run_tokens.sql — the bearer a run pod holds for its evidence (RFC-0004:C-RUN-EVIDENCE).
--
-- A token reads `crr_<id>_<secret>`, verified like an api key: one indexed lookup on `id`, then a
-- constant-time compare against the SHA-256 of the secret half. It is minted at dispatch, before
-- the run row exists, so `run_id` carries no foreign key.
CREATE TABLE run_tokens (
    id          TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL,
    secret_hash TEXT NOT NULL,
    -- The run whose evidence this token may read: the parent the run's launch reruns.
    reads_run   TEXT,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    revoked_at  TEXT
);

CREATE INDEX run_tokens_run ON run_tokens (run_id);
