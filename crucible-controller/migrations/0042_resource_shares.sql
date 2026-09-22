-- 0042_resource_shares.sql — per-resource shares (RFC-0003 C-SHARING): a grantee principal holds a
-- share role on one governed resource, optionally until a not-after time. Stored with the resource,
-- outside the digested policy set; keyed by the resource, so a transfer leaves them in place.
CREATE TABLE resource_shares (
    resource_type TEXT NOT NULL,
    resource_id   TEXT NOT NULL,
    -- `user:<login>` or `team:<slug>`.
    grantee       TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('viewer', 'launcher', 'editor')),
    not_after     TEXT,
    created_by    TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    PRIMARY KEY (resource_type, resource_id, grantee)
);
CREATE INDEX resource_shares_grantee ON resource_shares (grantee, resource_type);
