-- 0029_model_providers.sql — which inference provider and model a dispatch runs against.
--
-- Two tables, for the reason 0025 splits the pinned target from the policy that picks one:
-- `model_providers` is the registry an administrator curates (what exists, what it may run, which
-- registry secret pays for it), and `dispatch_defaults` is the policy layer naming which of those a
-- dispatch takes when nobody chose. A default is a pointer, so repointing it moves every future
-- dispatch that never chose, and leaves the ones that did where they were put.
--
-- Both tables start empty and stay behaviour-neutral while they are: with no rows, resolution
-- yields nothing and every dispatch renders exactly as it did before providers existed — the pack
-- manifest's `[agent]` table still decides the harness and the model.
CREATE TABLE model_providers (
    -- A slug, and the stable handle the API and the pinned columns below refer to. The display
    -- name is free-form and two entries may share one; the id and the kind disambiguate.
    id            TEXT PRIMARY KEY,
    display_name  TEXT NOT NULL,
    -- 'anthropic' | 'vertex' | 'openai'. The kind decides the harness the pod renders with and the
    -- environment variable the key projects as.
    kind          TEXT NOT NULL,
    -- The curated list the pickers offer. Free text is still accepted at launch, so this bounds
    -- what is suggested, not what may run.
    models        TEXT[] NOT NULL DEFAULT '{}',
    default_model TEXT NOT NULL,
    -- A secrets-registry name; NULL means the deploy profile's ambient credentials, which is what
    -- Vertex ADC uses. Keys never live here: Postgres holds the reference, Vault holds the bytes.
    secret_name   TEXT,
    -- The principal whose keyspace holds `secret_name`. A registry name is unique per owner and
    -- not globally, so the owner is the other half of the reference: on the name alone, any user
    -- who registers the same name in their own keyspace makes it ambiguous and refuses every
    -- dispatch that spends it. NULL exactly when `secret_name` is.
    secret_owner  TEXT,
    enabled       BOOLEAN NOT NULL DEFAULT TRUE,
    created_by    TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    CONSTRAINT model_providers_secret_ref CHECK ((secret_name IS NULL) = (secret_owner IS NULL))
);

-- Defaults are per workload class because the classes want different models: an autoresearch loop
-- wants the heavier one, a playbook run does not.
CREATE TABLE dispatch_defaults (
    -- 'platform' | 'domain'.
    scope_kind     TEXT NOT NULL,
    -- The domain name, or '' for the platform-wide row. Empty rather than NULL so the primary key
    -- can hold one platform row per class.
    scope_ref      TEXT NOT NULL DEFAULT '',
    -- 'playbook' | 'autoresearch'.
    workload_class TEXT NOT NULL,
    provider_id    TEXT NOT NULL REFERENCES model_providers(id) ON DELETE CASCADE,
    -- NULL takes the provider's own default_model, so retiring a model is one edit on the provider.
    model          TEXT,
    PRIMARY KEY (scope_kind, scope_ref, workload_class)
);

-- The launch override, pinned on the row exactly as `dispatch_target` is (migration 0025) and read
-- at each dispatch. NULL takes whatever the defaults resolve to at that moment; a value outranks
-- every default. `agent_model` without `agent_provider` is refused at the API, never stored.
ALTER TABLE issues ADD COLUMN agent_provider TEXT;
ALTER TABLE issues ADD COLUMN agent_model    TEXT;

-- A schedule fires long after its author is gone, so it carries the choice they made at save time
-- rather than resolving one at fire time against nobody. NULL is the same "resolve it then".
ALTER TABLE playbook_schedules ADD COLUMN agent_provider TEXT;
ALTER TABLE playbook_schedules ADD COLUMN agent_model    TEXT;
