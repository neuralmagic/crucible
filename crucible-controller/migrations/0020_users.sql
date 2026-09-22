-- 0020_users.sql — the identity the controller mints for itself once it is the OIDC relying party.
--
-- `sub` is the issuer's stable subject and the primary key; `login` is the name every role list,
-- ownership check, and `user:` principal is spelled in. A cluster token resolves to an OpenShift
-- username that need not be an SSO login, so ownership of a secret or a schedule needs a row here
-- to land on.
--
-- Same conventions as the baseline: TEXT RFC3339 UTC timestamps.
CREATE TABLE users (
    sub        TEXT PRIMARY KEY,
    -- Normalized (trimmed, lowercased) exactly like the role lists and `Principal::User`.
    login      TEXT NOT NULL,
    email      TEXT,
    -- The last successful login, so an operator can tell a dormant account from a live one.
    last_login TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- One login per subject: two subjects claiming one login would make `user:<login>` ambiguous, and
-- the ownership model compares logins.
CREATE UNIQUE INDEX users_login ON users (login);
