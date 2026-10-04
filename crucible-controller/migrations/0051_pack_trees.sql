-- 0051_pack_trees.sql — packs stored once by tree1: digest (RFC-0002:C-PACK-BASE, ADR-0059).
-- Expand only: every table that holds pack bytes gains a digest column, and the legacy bytes stay
-- until a later migration drops them after startup conversion has filled every digest.

CREATE TABLE pack_trees (
    digest          TEXT PRIMARY KEY CHECK (digest ~ '^tree1:[0-9a-f]{64}$'),
    file_count      INT    NOT NULL,
    total_bytes     BIGINT NOT NULL,
    delivered_bytes BIGINT NOT NULL,
    -- sha256 of the tree's deterministic delivery tarball, in content_digest form.
    tarball_digest  TEXT   NOT NULL,
    created_at      TEXT   NOT NULL
);

CREATE TABLE pack_tree_files (
    digest  TEXT  NOT NULL REFERENCES pack_trees(digest) ON DELETE CASCADE,
    path    TEXT  NOT NULL,
    sha256  TEXT  NOT NULL,
    content BYTEA NOT NULL,
    PRIMARY KEY (digest, path)
);

-- A pre-tree digest and the tree it became, or why it could not become one. No foreign key on
-- tree_digest: the alias outlives its tree so a stale pin is still answered as superseded.
CREATE TABLE pack_digest_aliases (
    old_digest           TEXT PRIMARY KEY,
    tree_digest          TEXT,
    unconvertible_reason TEXT,
    recorded_at          TEXT NOT NULL,
    CHECK ((tree_digest IS NULL) <> (unconvertible_reason IS NULL))
);

-- Every tree a registered pack has held, for base availability (current or prior revision).
CREATE TABLE playbook_revisions (
    playbook_id   TEXT NOT NULL REFERENCES playbooks(id) ON DELETE CASCADE,
    tree_digest   TEXT NOT NULL REFERENCES pack_trees(digest),
    first_seen_at TEXT NOT NULL,
    PRIMARY KEY (playbook_id, tree_digest)
);

ALTER TABLE playbooks                  ADD COLUMN tree_digest TEXT REFERENCES pack_trees(digest);
ALTER TABLE pack_imports               ADD COLUMN tree_digest TEXT REFERENCES pack_trees(digest);
ALTER TABLE playbook_draft_versions    ADD COLUMN tree_digest TEXT REFERENCES pack_trees(digest);
ALTER TABLE playbook_standing_launches ADD COLUMN adopted_tree_digest TEXT REFERENCES pack_trees(digest);
ALTER TABLE pack_tarballs              ADD COLUMN tree_digest TEXT REFERENCES pack_trees(digest);
-- The tree a scope froze. Build planning and dispatch read it, not whatever the issue's
-- pack_tarballs row holds later. NULL for a scope frozen before trees, which reads that row.
ALTER TABLE scopes                     ADD COLUMN tree_digest TEXT REFERENCES pack_trees(digest);
-- The tree a draft was seeded from and the tree a secret binding was reviewed against. Comparison
-- pins only, so no foreign key; NULL compares by the rev beside it.
ALTER TABLE playbook_drafts            ADD COLUMN origin_digest TEXT;
ALTER TABLE secret_bindings            ADD COLUMN pack_digest TEXT;

-- A controller that predates tree storage rewrites legacy bytes without knowing the tree column.
-- When the bytes change, keep the tree if the new bytes are its tarball or an encoding of it that
-- conversion recorded in pack_digest_aliases; otherwise clear it so startup conversion picks the
-- row up again.
CREATE FUNCTION pack_legacy_bytes_changed() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    bytes_digest TEXT;
    tree         TEXT;
BEGIN
    EXECUTE format('SELECT ''sha256:'' || encode(sha256(($1).%I), ''hex''), ($1).%I',
                   TG_ARGV[0], TG_ARGV[1])
        INTO bytes_digest, tree USING NEW;
    IF tree IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pack_trees WHERE digest = tree AND tarball_digest = bytes_digest)
       AND NOT EXISTS (SELECT 1 FROM pack_digest_aliases
                       WHERE old_digest = bytes_digest AND tree_digest = tree) THEN
        NEW := jsonb_populate_record(NEW, jsonb_build_object(TG_ARGV[1], NULL));
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER playbooks_legacy_bytes BEFORE UPDATE OF tar_gz ON playbooks
    FOR EACH ROW WHEN (NEW.tar_gz IS DISTINCT FROM OLD.tar_gz)
    EXECUTE FUNCTION pack_legacy_bytes_changed('tar_gz', 'tree_digest');
CREATE TRIGGER pack_imports_legacy_bytes BEFORE UPDATE OF tar_gz ON pack_imports
    FOR EACH ROW WHEN (NEW.tar_gz IS DISTINCT FROM OLD.tar_gz)
    EXECUTE FUNCTION pack_legacy_bytes_changed('tar_gz', 'tree_digest');
CREATE TRIGGER playbook_draft_versions_legacy_bytes BEFORE UPDATE OF tar_gz ON playbook_draft_versions
    FOR EACH ROW WHEN (NEW.tar_gz IS DISTINCT FROM OLD.tar_gz)
    EXECUTE FUNCTION pack_legacy_bytes_changed('tar_gz', 'tree_digest');
CREATE TRIGGER playbook_standing_launches_legacy_bytes
    BEFORE UPDATE OF adopted_tar_gz ON playbook_standing_launches
    FOR EACH ROW WHEN (NEW.adopted_tar_gz IS DISTINCT FROM OLD.adopted_tar_gz)
    EXECUTE FUNCTION pack_legacy_bytes_changed('adopted_tar_gz', 'adopted_tree_digest');
CREATE TRIGGER pack_tarballs_legacy_bytes BEFORE UPDATE OF tar_gz ON pack_tarballs
    FOR EACH ROW WHEN (NEW.tar_gz IS DISTINCT FROM OLD.tar_gz)
    EXECUTE FUNCTION pack_legacy_bytes_changed('tar_gz', 'tree_digest');
