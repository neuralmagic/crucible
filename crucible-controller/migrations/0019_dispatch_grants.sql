-- 0019_dispatch_grants.sql — the projection half of the secrets registry: what one dispatch
-- resolved, which pod may redeem it, and until when.
--
-- Same conventions as the baseline: TEXT RFC3339 UTC timestamps, BIGINT integers, and a DEFAULT on
-- every NOT NULL column that has a sensible one.

-- The UID the API server returned when it created the pod. The controller observed it itself, so
-- it is the only pod identity a redemption may be checked against; a pod name is deterministic per
-- run and the loop service account can create pods in the loop namespace.
ALTER TABLE work_pods ADD COLUMN pod_uid TEXT;

CREATE TABLE secret_grants (
    id         TEXT PRIMARY KEY,
    run_id     TEXT NOT NULL,
    issue_key  TEXT,
    pod_name   TEXT NOT NULL,
    -- NULL between minting the grant (its id has to be in the pod spec) and the create response
    -- coming back. A grant with no UID is unredeemable.
    pod_uid    TEXT,
    cluster    TEXT NOT NULL,
    -- `issued`: minted, no UID yet. `bound`: the create response's UID is recorded and the pod may
    -- redeem. `voided`: the pod it was minted for does not exist (an AlreadyExists on create, a
    -- failed launch). `redeemed`: the bundle was handed over at least once.
    state      TEXT NOT NULL DEFAULT 'issued'
               CHECK (state IN ('issued', 'bound', 'voided', 'redeemed')),
    redeem_by  TEXT NOT NULL,
    -- Redemption attempts that failed. Reconcile parks the run and deletes the pod past a
    -- threshold, so a pod that cannot redeem does not hold a GPU in CrashLoopBackOff.
    failures   BIGINT NOT NULL DEFAULT 0,
    error      TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX secret_grants_run ON secret_grants (run_id);
CREATE INDEX secret_grants_pod ON secret_grants (pod_name);

-- What one grant resolves to. The secret id, never its bytes: the hub reads Vault at redemption.
CREATE TABLE secret_grant_items (
    grant_id        TEXT NOT NULL REFERENCES secret_grants(id) ON DELETE CASCADE,
    secret_id       TEXT NOT NULL REFERENCES secrets(id),
    declared_name   TEXT NOT NULL,
    projection_kind TEXT NOT NULL CHECK (projection_kind IN ('env', 'file')),
    projection      TEXT NOT NULL,
    PRIMARY KEY (grant_id, declared_name)
);

-- A scheduled launch fires with no session, so the authorization it launches under is whatever the
-- last save recorded: the saver's principal and the groups their claims carried, plus when the
-- snapshot was taken. Existing rows have none and park until re-saved.
ALTER TABLE playbook_schedules
    ADD COLUMN owner_principal TEXT,
    ADD COLUMN owner_groups    JSONB,
    ADD COLUMN owner_groups_at TEXT;

-- The groups the launcher held when the launch was authorized. `created_by` is only half an
-- identity: a group-owned secret needs the memberships too, and a dispatch happens long after the
-- session that authorized it is gone.
ALTER TABLE playbook_launches ADD COLUMN launcher_groups JSONB;
