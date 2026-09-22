-- 0021_user_credentials.sql — the offline credential a scheduled launch re-checks group
-- membership with, and the per-schedule refresh state the schedules view reads.
--
-- The token is an OFFLINE refresh token: RP-initiated logout revokes the session-bound kind, and a
-- schedule owner who closes their tab must keep firing. It is not session state, so it lives here
-- keyed by the issuer's subject, one row per user, latest login wins.
--
-- Same conventions as the baseline: TEXT RFC3339 UTC timestamps, BIGINT integers, and a DEFAULT on
-- every NOT NULL column that has a sensible one.
CREATE TABLE user_credentials (
    sub          TEXT PRIMARY KEY REFERENCES users(sub) ON DELETE CASCADE,
    -- base64(nonce || AES-256-GCM ciphertext || tag) under the chart-mounted key named by
    -- `key_id`, with the subject as additional data so a row cannot be replayed under another.
    token_cipher TEXT NOT NULL,
    -- Which mounted key sealed it. A row sealed under a key the deploy no longer mounts cannot be
    -- opened, and its owner signs in again.
    key_id       TEXT NOT NULL,
    -- When the credential last produced a token. NULL until the first refresh.
    refreshed_at TEXT,
    -- The last refresh refusal, kept so the schedules view can say why an owner needs to sign in.
    last_error   TEXT,
    -- Definitive refusals in a row. Reset by a successful refresh and by a fresh login.
    failures     BIGINT NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

-- The fire-time refresh's own state, per schedule, because that is the row an owner reads and
-- re-saves. `owner_signin_required` is what a definitive refresh failure past the snapshot TTL and
-- an explicit revoke both set; a successful refresh clears it along with the error.
ALTER TABLE playbook_schedules
    ADD COLUMN owner_signin_required BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN owner_refresh_error   TEXT,
    ADD COLUMN owner_refresh_at      TEXT;
