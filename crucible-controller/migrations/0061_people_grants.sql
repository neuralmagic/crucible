-- 0061_people_grants.sql — people's OAuth authorization codes and refresh tokens, shared by every
-- controller replica. Each secret is stored only as the SHA-256 of its text.
CREATE TABLE people_codes (
    key        BYTEA PRIMARY KEY,
    sub        TEXT NOT NULL REFERENCES users(sub) ON DELETE CASCADE,
    grant_json JSONB NOT NULL,
    expires_at BIGINT NOT NULL
);

-- One row per approval: the refresh token family a code starts.
CREATE TABLE people_grants (
    id           TEXT PRIMARY KEY,
    sub          TEXT NOT NULL REFERENCES users(sub) ON DELETE CASCADE,
    resource     TEXT NOT NULL,
    cluster      TEXT NOT NULL,
    scopes       JSONB NOT NULL,
    created_at   BIGINT NOT NULL,
    last_used_at BIGINT NOT NULL,
    revoked_at   BIGINT
);
CREATE INDEX people_grants_sub ON people_grants (sub);

-- Every refresh token a family issued. A used token stays until its family goes, so presenting it
-- again is recognised as reuse.
CREATE TABLE people_refresh_tokens (
    key       BYTEA PRIMARY KEY,
    grant_id  TEXT NOT NULL REFERENCES people_grants(id) ON DELETE CASCADE,
    issued_at BIGINT NOT NULL,
    used_at   BIGINT
);
CREATE INDEX people_refresh_tokens_grant ON people_refresh_tokens (grant_id);
