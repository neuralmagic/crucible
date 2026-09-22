-- 0018_secrets_registry.sql — the authorization half of the secrets registry. Postgres holds who
-- owns a secret, where its bytes live, and what it is bound to; the bytes themselves live in Vault
-- and never touch this database.
--
-- Same conventions as the baseline: TEXT RFC3339 UTC timestamps, BIGINT integers, and a DEFAULT on
-- every NOT NULL column that has a sensible one.
CREATE TABLE secrets (
    id              TEXT PRIMARY KEY,
    -- The name a pack manifest declares and a binding maps. Unique per owner, not globally: two
    -- teams may both call their push credential `registry`.
    name            TEXT NOT NULL,
    -- `user:<login>` or `group:<rover path>`, normalized (trimmed, lowercased) and compared as an
    -- exact full path.
    owner           TEXT NOT NULL,
    kind            TEXT NOT NULL
                    CHECK (kind IN ('opaque', 'file', 'registry_authfile', 'kubeconfig')),
    visibility      TEXT NOT NULL CHECK (visibility IN ('broker_only', 'agent_visible')),
    -- `run` is projected into pods; `hub` is read by the hub for its own clients.
    consumer        TEXT NOT NULL CHECK (consumer IN ('run', 'hub')),
    -- `managed`: the hub wrote the bytes at `vault_path` under its own mount. `reference`: the
    -- registrant pointed at a path someone else owns, and `vault_path` is the `vault://` URL.
    mode            TEXT NOT NULL CHECK (mode IN ('managed', 'reference')),
    vault_path      TEXT NOT NULL,
    -- The KV version the last write landed at. NULL for a reference, whose versions belong to the
    -- path's owner.
    current_version BIGINT,
    created_by      TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE (owner, name)
);

-- The ownership lookup the launch check runs: "which secrets does this scope's bindings' owner set
-- contain" starts from the binding, but every list and every authorization check filters by owner.
CREATE INDEX secrets_owner ON secrets (owner);

CREATE TABLE secret_bindings (
    id              TEXT PRIMARY KEY,
    secret_id       TEXT NOT NULL REFERENCES secrets(id),
    scope_kind      TEXT NOT NULL CHECK (scope_kind IN ('repo', 'playbook', 'domain')),
    scope_id        TEXT NOT NULL,
    -- How the value reaches the run: an environment variable name, or a path in the pod's tmpfs.
    projection_kind TEXT NOT NULL CHECK (projection_kind IN ('env', 'file')),
    projection      TEXT NOT NULL,
    -- The declared name in the pack manifest this binding satisfies.
    declared_name   TEXT NOT NULL,
    -- What the binding was made against. A pin bump that moves either parks launches on the scope
    -- until an owner member re-binds.
    pack_rev        TEXT,
    schema_digest   TEXT,
    created_by      TEXT,
    created_at      TEXT NOT NULL,
    -- One declared name resolves to one secret per scope, and one projection is written once.
    UNIQUE (scope_kind, scope_id, declared_name),
    UNIQUE (scope_kind, scope_id, projection_kind, projection)
);

-- The launch check's lookup: every binding on the scope a run is about to launch.
CREATE INDEX secret_bindings_scope ON secret_bindings (scope_kind, scope_id);
-- The delete check's lookup: does this secret still have bindings.
CREATE INDEX secret_bindings_secret ON secret_bindings (secret_id);

-- Every mutation of the registry, plus (once grants ship) every grant, hub read, and redemption.
-- No foreign key: the trail outlives the row it describes, which is the point of a delete audit.
CREATE TABLE secret_audit (
    id          BIGSERIAL PRIMARY KEY,
    secret_id   TEXT,
    secret_name TEXT NOT NULL,
    owner       TEXT NOT NULL,
    action      TEXT NOT NULL
                CHECK (action IN ('register', 'rotate', 'bind', 'unbind', 'delete',
                                  'grant', 'hub_read', 'redeem')),
    -- The acting principal (`user:<login>`, `group:<rover path>`). NULL only where there is none:
    -- a hub read on its own behalf.
    actor       TEXT,
    detail      TEXT,
    at          TEXT NOT NULL
);

CREATE INDEX secret_audit_secret ON secret_audit (secret_id, id DESC);
