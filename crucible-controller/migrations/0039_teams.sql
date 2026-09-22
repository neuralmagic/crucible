-- 0039_teams.sql — controller-managed teams (RFC-0003 C-TEAMS) and the authorization audit trail
-- (C-AUDIT). Same conventions as the baseline: TEXT RFC3339 UTC timestamps, a DEFAULT on every
-- NOT NULL column that has a sensible one.
CREATE TABLE teams (
    -- [a-z0-9][a-z0-9-]{1,62}, unique per controller; the `team:<slug>` principal.
    slug         TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    -- The principal that created the team; NULL for a team seeded from configuration.
    created_by   TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

-- One row per member. A member is a user login, a group path, another team's slug, or a
-- membership rule, each at one team role.
CREATE TABLE team_members (
    team        TEXT NOT NULL REFERENCES teams(slug) ON DELETE CASCADE,
    member_kind TEXT NOT NULL CHECK (member_kind IN ('user', 'group', 'team', 'rule')),
    -- The login, the full group path, the nested team's slug, or the rule spelled
    -- `email-domain:<domain>` / `group-prefix:<path>`; normalized like the matching principal.
    member_ref  TEXT NOT NULL,
    role        TEXT NOT NULL CHECK (role IN ('member', 'maintainer', 'owner')),
    since       TEXT NOT NULL,
    added_by    TEXT,
    PRIMARY KEY (team, member_kind, member_ref)
);

-- Membership resolution starts from the subject's own principals and walks up.
CREATE INDEX team_members_ref ON team_members (member_kind, member_ref);

-- Every authorization change and every denied mutation. No foreign keys: the trail outlives the
-- rows it describes.
CREATE TABLE authz_audit (
    id            BIGSERIAL PRIMARY KEY,
    at            TEXT NOT NULL,
    -- The principal that caused the request, or `configuration` for a startup seed.
    actor         TEXT NOT NULL,
    -- The subject principal set's own principal, when it differs from the actor.
    subject       TEXT,
    -- The credential path that proved the request (`session`, `api_key`, ...), or `startup`.
    auth_path     TEXT NOT NULL,
    -- `<resource>:<verb>` from the action vocabulary.
    action        TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    resource_id   TEXT NOT NULL,
    decision      TEXT NOT NULL CHECK (decision IN ('allow', 'deny')),
    -- The rule that decided, or `no-rule`.
    rule          TEXT NOT NULL,
    prior         JSONB,
    result        JSONB
);

CREATE INDEX authz_audit_resource ON authz_audit (resource_type, resource_id, id DESC);
