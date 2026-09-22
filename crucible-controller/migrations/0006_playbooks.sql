-- 0006_playbooks.sql — the playbook registry: a git-pinned pack plus the launch form the pinned
-- engine extracted from it. Same conventions as the baseline: TEXT RFC3339 UTC timestamps,
-- BIGINT integers.
--
-- The tarball lives here rather than in `pack_tarballs`: that table is keyed by sanitized issue
-- key and holds scope packs, while a playbook pack is keyed by registry id and materialized by id.
CREATE TABLE playbooks (
    -- Operator-chosen slug, validated at registration (`playbooks::validate_id`): the launch key
    -- `playbook:{id}:{uuid}` is built from it.
    id            TEXT PRIMARY KEY,
    description   TEXT NOT NULL,
    -- `owner/repo` slug or a full clone URL, normalized the way scenarios normalize theirs.
    repo          TEXT NOT NULL,
    -- The branch/tag asked for; NULL = the repo's default branch.
    git_ref       TEXT,
    -- The resolved commit the pack was taken at: the pin.
    rev           TEXT NOT NULL,
    -- Pack directory inside the repo; '' = the repo root.
    path          TEXT NOT NULL,
    tar_gz        BYTEA NOT NULL,
    tar_digest    TEXT NOT NULL,
    tar_bytes     BIGINT NOT NULL,
    -- `crucible plan params` output for the pack's workflow source, verbatim.
    params_schema JSONB NOT NULL,
    schema_digest TEXT NOT NULL,
    -- The engine pin the schema was extracted with; a mismatch with the running binary's pin is
    -- what the startup re-derivation claims.
    core_rev      TEXT NOT NULL,
    created_by    TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX playbooks_core_rev ON playbooks (core_rev);
