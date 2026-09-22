-- 0013_pack_imports.sql — a proposed pack import, frozen at the commit it was fetched at. The
-- wizard's preview stopped being ephemeral: the fetch and the compile happen once, server-side,
-- and the row is what a shared link, the approvals rail and the registration all read. Same
-- conventions as 0006/0012: TEXT RFC3339 UTC timestamps, BYTEA tarballs, BIGINT integers.
CREATE TABLE pack_imports (
    -- uuidv7, so the id sorts by proposal time and rides a URL.
    id            TEXT PRIMARY KEY,
    repo          TEXT NOT NULL,
    -- The branch/tag asked for; NULL = the repo's default branch.
    git_ref       TEXT,
    -- Pack directory inside the repo; '' = the repo root.
    path          TEXT NOT NULL,
    -- The commit the fetch resolved to. Everything below was taken at it.
    rev           TEXT NOT NULL,
    tar_gz        BYTEA NOT NULL,
    tar_digest    TEXT NOT NULL,
    tar_bytes     BIGINT NOT NULL,
    -- NULL when the pinned engine refused the source: the diagnostics are the preview then.
    params_schema JSONB,
    schema_digest TEXT,
    -- The compiled plan as `plan_graph::WorkflowGraphDto`; NULL when there is none.
    graph         JSONB,
    -- The engine's own text, verbatim, as a JSON array of strings.
    diagnostics   JSONB NOT NULL,
    core_rev      TEXT NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('pending', 'registered', 'discarded')),
    -- The registry id this import registered as, once it did.
    playbook      TEXT,
    -- The draft opened from this import's frozen tarball, once one was.
    draft_id      TEXT,
    proposed_by   TEXT,
    created_at    TEXT NOT NULL,
    resolved_by   TEXT,
    resolved_at   TEXT
);
CREATE INDEX pack_imports_pending ON pack_imports (status, created_at DESC);
