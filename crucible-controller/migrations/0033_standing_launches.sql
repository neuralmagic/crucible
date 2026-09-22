-- 0033_standing_launches.sql — the standing authorization to launch a playbook, split from the
-- trigger that fires it. A schedule (a cron window), a one-shot (a deferred instant), and a watch
-- (a tracker query hit) are 1:1 sidecars on one `playbook_standing_launches` row, keyed by the
-- same id, so the adopted pack snapshot, params, ceilings, owner snapshot and its refresh state,
-- dispatch target, agent pin, enabled flag, and failure count exist once and every trigger fires
-- through one path. Existing schedules and one-shots move onto the core in place; their ids do
-- not change.
CREATE TABLE playbook_standing_launches (
    id                     TEXT PRIMARY KEY,
    trigger                TEXT NOT NULL CHECK (trigger IN ('schedule', 'deferred', 'watch')),
    playbook               TEXT NOT NULL,
    target_kind            TEXT NOT NULL DEFAULT 'adopted'
                               CHECK (target_kind IN ('adopted', 'draft_head')),
    adopted_repo           TEXT,
    adopted_path           TEXT,
    adopted_rev            TEXT,
    adopted_tar_gz         BYTEA,
    adopted_tar_digest     TEXT,
    adopted_tar_bytes      BIGINT,
    adopted_params_schema  JSONB,
    eligible_draft_version BIGINT,
    params                 JSONB NOT NULL,
    schema_digest          TEXT NOT NULL,
    max_cost               DOUBLE PRECISION NOT NULL,
    max_time               TEXT NOT NULL,
    advance_dedupe         BOOLEAN NOT NULL DEFAULT TRUE,
    enabled                BOOLEAN NOT NULL DEFAULT TRUE,
    consecutive_failures   BIGINT NOT NULL DEFAULT 0,
    created_by             TEXT,
    owner_principal        TEXT,
    owner_groups           JSONB,
    owner_groups_at        TEXT,
    owner_signin_required  BOOLEAN NOT NULL DEFAULT false,
    owner_refresh_error    TEXT,
    owner_refresh_at       TEXT,
    dispatch_target        TEXT,
    agent_provider         TEXT,
    agent_model            TEXT,
    created_at             TEXT NOT NULL,
    updated_at             TEXT NOT NULL,
    CONSTRAINT playbook_standing_launches_target_shape CHECK (
        (target_kind = 'adopted' AND adopted_repo IS NOT NULL AND adopted_path IS NOT NULL
         AND adopted_rev IS NOT NULL AND adopted_tar_gz IS NOT NULL
         AND adopted_tar_digest IS NOT NULL AND adopted_tar_bytes IS NOT NULL
         AND adopted_params_schema IS NOT NULL)
        OR target_kind = 'draft_head'
    )
);

CREATE INDEX playbook_standing_launches_playbook ON playbook_standing_launches (playbook);
CREATE INDEX playbook_standing_launches_owner ON playbook_standing_launches (owner_principal);

INSERT INTO playbook_standing_launches (
    id, trigger, playbook, target_kind, adopted_repo, adopted_path, adopted_rev, adopted_tar_gz,
    adopted_tar_digest, adopted_tar_bytes, adopted_params_schema, eligible_draft_version, params,
    schema_digest, max_cost, max_time, advance_dedupe, enabled, consecutive_failures, created_by,
    owner_principal, owner_groups, owner_groups_at, owner_signin_required, owner_refresh_error,
    owner_refresh_at, dispatch_target, agent_provider, agent_model, created_at, updated_at)
SELECT id, 'schedule', playbook, target_kind, adopted_repo, adopted_path, adopted_rev,
       adopted_tar_gz, adopted_tar_digest, adopted_tar_bytes, adopted_params_schema,
       eligible_draft_version, params, schema_digest, max_cost, max_time, advance_dedupe, enabled,
       consecutive_failures, created_by, owner_principal, owner_groups, owner_groups_at,
       owner_signin_required, owner_refresh_error, owner_refresh_at, dispatch_target,
       agent_provider, agent_model, created_at, updated_at
FROM playbook_schedules;

DROP INDEX playbook_schedules_due;
DROP INDEX playbook_schedules_playbook;
ALTER TABLE playbook_schedules
    DROP CONSTRAINT playbook_schedules_target_shape,
    DROP COLUMN playbook,
    DROP COLUMN target_kind,
    DROP COLUMN adopted_repo,
    DROP COLUMN adopted_path,
    DROP COLUMN adopted_rev,
    DROP COLUMN adopted_tar_gz,
    DROP COLUMN adopted_tar_digest,
    DROP COLUMN adopted_tar_bytes,
    DROP COLUMN adopted_params_schema,
    DROP COLUMN eligible_draft_version,
    DROP COLUMN params,
    DROP COLUMN schema_digest,
    DROP COLUMN max_cost,
    DROP COLUMN max_time,
    DROP COLUMN advance_dedupe,
    DROP COLUMN enabled,
    DROP COLUMN consecutive_failures,
    DROP COLUMN created_by,
    DROP COLUMN owner_principal,
    DROP COLUMN owner_groups,
    DROP COLUMN owner_groups_at,
    DROP COLUMN owner_signin_required,
    DROP COLUMN owner_refresh_error,
    DROP COLUMN owner_refresh_at,
    DROP COLUMN dispatch_target,
    DROP COLUMN agent_provider,
    DROP COLUMN agent_model,
    DROP COLUMN created_at,
    DROP COLUMN updated_at,
    ADD CONSTRAINT playbook_schedules_standing_fkey
        FOREIGN KEY (id) REFERENCES playbook_standing_launches(id) ON DELETE CASCADE;
CREATE INDEX playbook_schedules_due ON playbook_schedules (next_due_at)
    WHERE next_due_at IS NOT NULL;

-- One-shots never snapshotted the pack; a pending one adopts the registry row as it stands at
-- migration, which is what its firing would have copied anyway. `enabled` mirrors `pending`.
INSERT INTO playbook_standing_launches (
    id, trigger, playbook, target_kind, adopted_repo, adopted_path, adopted_rev, adopted_tar_gz,
    adopted_tar_digest, adopted_tar_bytes, adopted_params_schema, params, schema_digest, max_cost,
    max_time, advance_dedupe, enabled, created_by, created_at, updated_at)
SELECT o.id, 'deferred', o.playbook, 'adopted', p.repo, p.path, p.rev, p.tar_gz, p.tar_digest,
       p.tar_bytes, p.params_schema, o.params, o.schema_digest, o.max_cost, o.max_time,
       o.advance_dedupe, o.status = 'pending', o.created_by, o.created_at, o.created_at
FROM playbook_one_shots o JOIN playbooks p ON p.id = o.playbook;

DROP INDEX playbook_one_shots_due;
DROP INDEX playbook_one_shots_playbook;
ALTER TABLE playbook_one_shots
    DROP COLUMN playbook,
    DROP COLUMN params,
    DROP COLUMN schema_digest,
    DROP COLUMN max_cost,
    DROP COLUMN max_time,
    DROP COLUMN advance_dedupe,
    DROP COLUMN created_by,
    DROP COLUMN created_at,
    ADD CONSTRAINT playbook_one_shots_standing_fkey
        FOREIGN KEY (id) REFERENCES playbook_standing_launches(id) ON DELETE CASCADE;
CREATE INDEX playbook_one_shots_due ON playbook_one_shots (fire_at) WHERE status = 'pending';

-- The watch trigger: a tracker query swept on the discovery cadence. `watermark` is the
-- tracker-native update time the next sweep searches from; it starts at the watch's creation
-- (or an explicit earlier bound) and only ever moves forward.
CREATE TABLE playbook_watches (
    id               TEXT PRIMARY KEY
                         REFERENCES playbook_standing_launches(id) ON DELETE CASCADE,
    tracker          TEXT NOT NULL,
    query            TEXT NOT NULL,
    key_param        TEXT NOT NULL,
    watermark        TEXT NOT NULL,
    last_swept_at    TEXT,
    last_launched_at TEXT
);

-- One automatic launch per watch and item, ever. The row commits with the launch it records.
CREATE TABLE playbook_watch_hits (
    watch_id        TEXT NOT NULL REFERENCES playbook_watches(id) ON DELETE CASCADE,
    item_id         TEXT NOT NULL,
    item_updated_at TEXT NOT NULL,
    launch_key      TEXT NOT NULL,
    launched_at     TEXT NOT NULL,
    PRIMARY KEY (watch_id, item_id)
);

ALTER TABLE playbook_launches DROP CONSTRAINT playbook_launches_origin_check;
ALTER TABLE playbook_launches
    ADD CONSTRAINT playbook_launches_origin_check
    CHECK (origin IN ('manual', 'deferred', 'schedule', 'draft', 'watch'));
