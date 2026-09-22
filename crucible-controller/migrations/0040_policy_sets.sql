-- 0040_policy_sets.sql — stored policy sets (RFC-0003 C-POLICY). One row per distinct set,
-- keyed by its content digest; exactly one row is active.
CREATE TABLE policy_sets (
    -- SHA-256 of the canonical policy text.
    digest         TEXT PRIMARY KEY,
    text           TEXT NOT NULL,
    -- The version of the rendered Cedar schema the set was validated against.
    schema_version INTEGER NOT NULL,
    -- The principal that stored it; NULL for the shipped default.
    created_by     TEXT,
    created_at     TEXT NOT NULL,
    activated_by   TEXT,
    activated_at   TEXT,
    active         BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE UNIQUE INDEX policy_sets_active ON policy_sets (active) WHERE active;
