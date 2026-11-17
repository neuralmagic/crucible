-- 0087_pending_overrides.sql — human overrides (park, cancel, unpark, bump, scope, redispatch)
-- accepted by any replica and not yet applied by the leader. At most one per key: a later
-- submit replaces it. The leader deletes the row when it applies it.
CREATE TABLE pending_overrides (
    key           TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    reason        TEXT,
    priority      BIGINT,
    actor         TEXT,
    justification TEXT,
    max_cost      DOUBLE PRECISION,
    submitted_at  TEXT NOT NULL
);
