-- 0026_api_keys.sql — opaque per-user API keys, and the groups a key answers with.
--
-- A key reads `crk_<id>_<secret>`. The `id` half is this row's primary key, so verifying one is a
-- single indexed lookup rather than a scan that hashes against every stored key. Only the SHA-256
-- of the secret half is kept: 32 bytes of CSPRNG output is not a password, so a KDF would buy no
-- resistance a brute force could ever spend, and would cost a hash on every request.
--
-- Groups arrive on the ID token at login, so they belong on `users`, where one sign-in updates
-- every key its owner holds. A key is therefore never more powerful than its owner's last login,
-- and a deployment whose claim carries no groups gives its keys exactly what it gives its browser
-- sessions.
ALTER TABLE users ADD COLUMN groups    JSONB NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE users ADD COLUMN groups_at TEXT;

-- Same conventions as the baseline: TEXT RFC3339 UTC timestamps.
CREATE TABLE api_keys (
    id           TEXT PRIMARY KEY,
    sub          TEXT NOT NULL REFERENCES users(sub) ON DELETE CASCADE,
    -- What the owner called it, so a settings page can name the key it is about to revoke.
    name         TEXT NOT NULL,
    -- SHA-256 of the secret half, base64url. The secret is shown once, at mint, and never stored.
    secret_hash  TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    -- NULL never expires. The mint form defaults to 90 days; a key meant to outlive that has to
    -- say so deliberately.
    expires_at   TEXT,
    -- Advanced on use, so an owner can tell a key their agent still holds from one they forgot.
    last_used_at TEXT,
    -- Set rather than deleted: a revoked key stays listed so its owner can see it is dead, and a
    -- request presenting it is refused as revoked rather than as unknown.
    revoked_at   TEXT
);

-- The settings page's question ("which keys do I hold"), and the cascade's own lookup.
CREATE INDEX api_keys_sub ON api_keys (sub);
