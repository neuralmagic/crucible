-- 0015_user_prefs.sql — preferences that outlive a session. `sessions` holds scratch UI state
-- that lapses with the inactivity window; anything a user expects back in a fresh browser needs a
-- row keyed by their identity instead. TEXT RFC3339 UTC timestamps, same as everything else.
CREATE TABLE user_prefs (
    -- The identity the caller's session is bound to (crate::session::SessionIdentity::user).
    user_id    TEXT NOT NULL,
    -- Which document: one row per (user, kind), so the editor's settings and later documents do
    -- not contend on one blob.
    kind       TEXT NOT NULL,
    doc        JSONB NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (user_id, kind)
);
