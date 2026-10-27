-- 0071_commit_provenance.sql — signed provenance for the commits runs push.
--
-- `runs.dispatch_provenance` is the document the controller stamped on the run at dispatch (the
-- cluster, the provider, the sandbox image), kept so a statement reads back exactly what the run
-- was handed.
--
-- `commit_provenance` holds one DSSE envelope per (run, task attempt, repository, commit), as the
-- exact bytes that were signed and served: the digest is over those bytes, so they are never
-- re-serialized. A statement outlives the run row it describes, so run_id carries no foreign key.
ALTER TABLE runs ADD COLUMN dispatch_provenance JSONB;

CREATE TABLE commit_provenance (
    envelope_digest TEXT PRIMARY KEY,
    commit_sha      TEXT NOT NULL,
    repository      TEXT NOT NULL,
    run_id          TEXT NOT NULL,
    iter            BIGINT NOT NULL,
    task            TEXT NOT NULL,
    keyid           TEXT NOT NULL,
    envelope        BYTEA NOT NULL,
    created_at      TEXT NOT NULL,
    UNIQUE (run_id, iter, task, repository, commit_sha)
);
CREATE INDEX commit_provenance_commit ON commit_provenance (commit_sha);
CREATE INDEX commit_provenance_run ON commit_provenance (run_id);
