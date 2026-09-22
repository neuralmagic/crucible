-- 0007_playbook_launches.sql — the values and ceilings one launch of a registered playbook was
-- authorized with. A dequeued key carries no payload, so the dispatch re-reads this row by key.
-- Same conventions as the baseline: TEXT RFC3339 UTC timestamps, DOUBLE PRECISION money.
CREATE TABLE playbook_launches (
    key           TEXT PRIMARY KEY REFERENCES issues(key) ON DELETE CASCADE,
    -- The registry row the pack came from. The launch's own copy of the tarball lives in
    -- `pack_tarballs` under the sanitized key, so a later re-pin never changes what this runs.
    playbook      TEXT NOT NULL REFERENCES playbooks(id),
    -- The validated param values, `{name: value}`. JSONB because a schedule cursor rewrites
    -- individual fields between firings.
    params        JSONB NOT NULL,
    -- The digest of the schema the values were validated against: the audit trail for a pin bump
    -- that moved the form after this launch was authorized.
    schema_digest TEXT NOT NULL,
    -- The launcher's ceilings, bounded by admin config at the endpoint. A pack may not declare
    -- either. `max_time` is a duration string in the engine's grammar (`90s`, `30m`, `2h`).
    max_cost      DOUBLE PRECISION NOT NULL,
    max_time      TEXT NOT NULL,
    created_by    TEXT,
    created_at    TEXT NOT NULL
);
CREATE INDEX playbook_launches_playbook ON playbook_launches (playbook);
