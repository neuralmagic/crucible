-- 0044_playbook_draft_source.sql — a playbook registered straight from a draft.
--
-- `playbook_draft:publish` stores a draft's newest compiling version in the registry without a git
-- round trip, so a row now has exactly one source: a git pack (repo + path, optionally git_ref) or
-- a draft version (source_draft + source_draft_version). `rev` stays NOT NULL; a draft-sourced row
-- pins the tarball digest there. One draft publishes into at most one playbook at a time.
ALTER TABLE playbooks
    ALTER COLUMN repo DROP NOT NULL,
    ALTER COLUMN path DROP NOT NULL,
    ADD COLUMN source_draft         TEXT,
    ADD COLUMN source_draft_version BIGINT,
    ADD CONSTRAINT playbooks_one_source CHECK (
        (repo IS NOT NULL AND path IS NOT NULL
         AND source_draft IS NULL AND source_draft_version IS NULL)
        OR (repo IS NULL AND path IS NULL AND git_ref IS NULL
            AND source_draft IS NOT NULL AND source_draft_version IS NOT NULL)
    );

CREATE UNIQUE INDEX playbooks_source_draft ON playbooks (source_draft)
    WHERE source_draft IS NOT NULL;

-- An adopted snapshot of a draft-sourced playbook has no repo or path to copy.
ALTER TABLE playbook_standing_launches
    DROP CONSTRAINT playbook_standing_launches_target_shape,
    ADD CONSTRAINT playbook_standing_launches_target_shape CHECK (
        (target_kind = 'adopted'
         AND adopted_rev IS NOT NULL AND adopted_tar_gz IS NOT NULL
         AND adopted_tar_digest IS NOT NULL AND adopted_tar_bytes IS NOT NULL
         AND adopted_params_schema IS NOT NULL)
        OR target_kind = 'draft_head'
    );
