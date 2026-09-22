-- 0012_playbook_drafts.sql — draft packs authored in the controller. A draft is a versioned
-- blob, not a git pin: every save tars the editor's file map and stores it beside whatever the
-- pinned engine made of it. Same conventions as 0006/0007: TEXT RFC3339 UTC timestamps, BYTEA
-- tarballs, BIGINT integers.
CREATE TABLE playbook_drafts (
    -- Validated by `playbooks::validate_id`, like a registry id: a draft launch is keyed
    -- `playbook:{id}:{uuid}` too.
    id                TEXT PRIMARY KEY,
    description       TEXT NOT NULL,
    -- The registered pack version 1 was seeded from; NULL = a skeleton.
    template_playbook TEXT,
    -- Where graduation opened the export PR, and the url it returned.
    graduation_repo   TEXT,
    graduation_path   TEXT,
    graduation_pr_url TEXT,
    -- Stamped when the graduated pack is registered from `graduation_repo`/`graduation_path`.
    retired_at        TEXT,
    created_by        TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);

-- One save. A version with a NULL `params_schema` is a snapshot that did not compile: the editor
-- keeps the bytes and shows `diagnostics`, which is why the compile result is stored, not enforced.
CREATE TABLE playbook_draft_versions (
    draft_id      TEXT NOT NULL REFERENCES playbook_drafts(id) ON DELETE CASCADE,
    version       BIGINT NOT NULL,
    tar_gz        BYTEA NOT NULL,
    tar_digest    TEXT NOT NULL,
    tar_bytes     BIGINT NOT NULL,
    params_schema JSONB,
    schema_digest TEXT,
    -- The compiled plan reduced to nodes and edges, as `plan_graph::WorkflowGraphDto`.
    graph         JSONB,
    -- `[{file, line, col, message}]`, the engine's own text with its anchor parsed out.
    diagnostics   JSONB NOT NULL,
    core_rev      TEXT NOT NULL,
    created_by    TEXT,
    created_at    TEXT NOT NULL,
    PRIMARY KEY (draft_id, version)
);

-- A draft launch names a draft in `playbook`, so the registry foreign key cannot stand. The
-- integrity it carried moves into the adopt transaction, which re-reads the row it launches under
-- `FOR SHARE` exactly as the registered path already does.
ALTER TABLE playbook_launches DROP CONSTRAINT playbook_launches_playbook_fkey;
-- NULL = a registered-pack launch; set = the draft version this run froze.
ALTER TABLE playbook_launches ADD COLUMN draft_version BIGINT;
ALTER TABLE playbook_launches DROP CONSTRAINT playbook_launches_origin_check;
ALTER TABLE playbook_launches
    ADD CONSTRAINT playbook_launches_origin_check
    CHECK (origin IN ('manual', 'deferred', 'schedule', 'draft'));
