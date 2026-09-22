-- Recurring targets carry their own immutable adopted bytes. Registry repins therefore affect new
-- schedules only. The nullable draft_version distinguishes the exceptional draft-head target.
ALTER TABLE playbook_schedules DROP CONSTRAINT playbook_schedules_playbook_fkey;
ALTER TABLE playbook_schedules ADD COLUMN target_kind TEXT NOT NULL DEFAULT 'adopted'
    CHECK (target_kind IN ('adopted', 'draft_head'));
ALTER TABLE playbook_schedules ADD COLUMN adopted_repo TEXT;
ALTER TABLE playbook_schedules ADD COLUMN adopted_path TEXT;
ALTER TABLE playbook_schedules ADD COLUMN adopted_rev TEXT;
ALTER TABLE playbook_schedules ADD COLUMN adopted_tar_gz BYTEA;
ALTER TABLE playbook_schedules ADD COLUMN adopted_tar_digest TEXT;
ALTER TABLE playbook_schedules ADD COLUMN adopted_tar_bytes BIGINT;
ALTER TABLE playbook_schedules ADD COLUMN adopted_params_schema JSONB;
ALTER TABLE playbook_schedules ADD COLUMN eligible_draft_version BIGINT;

UPDATE playbook_schedules s SET
    adopted_repo = p.repo,
    adopted_path = p.path,
    adopted_rev = p.rev,
    adopted_tar_gz = p.tar_gz,
    adopted_tar_digest = p.tar_digest,
    adopted_tar_bytes = p.tar_bytes,
    adopted_params_schema = p.params_schema
FROM playbooks p WHERE p.id = s.playbook;

ALTER TABLE playbook_schedules ADD CONSTRAINT playbook_schedules_target_shape CHECK (
    (target_kind = 'adopted' AND adopted_repo IS NOT NULL AND adopted_path IS NOT NULL
     AND adopted_rev IS NOT NULL AND adopted_tar_gz IS NOT NULL
     AND adopted_tar_digest IS NOT NULL AND adopted_tar_bytes IS NOT NULL
     AND adopted_params_schema IS NOT NULL)
    OR target_kind = 'draft_head'
);
