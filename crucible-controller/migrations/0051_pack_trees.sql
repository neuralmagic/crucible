-- 0051_pack_trees.sql — packs stored once by tree1: digest (RFC-0002:C-PACK-BASE, ADR-0059).
-- Expand only: every table that holds pack bytes gains a digest column, and the legacy bytes stay
-- until a later migration drops them after startup conversion has filled every digest.

CREATE TABLE pack_trees (
    digest          TEXT PRIMARY KEY CHECK (digest ~ '^tree1:[0-9a-f]{64}$'),
    file_count      INT    NOT NULL,
    total_bytes     BIGINT NOT NULL,
    delivered_bytes BIGINT NOT NULL,
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

-- A controller that predates tree storage rewrites legacy bytes without knowing the tree column.
-- Clear the tree column when the bytes change alone, so startup conversion picks the row up again.
CREATE FUNCTION pack_legacy_bytes_changed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF to_jsonb(NEW) -> TG_ARGV[0] IS DISTINCT FROM to_jsonb(OLD) -> TG_ARGV[0]
       AND to_jsonb(NEW) -> TG_ARGV[1] IS NOT DISTINCT FROM to_jsonb(OLD) -> TG_ARGV[1] THEN
        NEW := jsonb_populate_record(NEW, jsonb_build_object(TG_ARGV[1], NULL));
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER playbooks_legacy_bytes BEFORE UPDATE ON playbooks
    FOR EACH ROW EXECUTE FUNCTION pack_legacy_bytes_changed('tar_gz', 'tree_digest');
CREATE TRIGGER pack_imports_legacy_bytes BEFORE UPDATE ON pack_imports
    FOR EACH ROW EXECUTE FUNCTION pack_legacy_bytes_changed('tar_gz', 'tree_digest');
CREATE TRIGGER playbook_draft_versions_legacy_bytes BEFORE UPDATE ON playbook_draft_versions
    FOR EACH ROW EXECUTE FUNCTION pack_legacy_bytes_changed('tar_gz', 'tree_digest');
CREATE TRIGGER playbook_standing_launches_legacy_bytes BEFORE UPDATE ON playbook_standing_launches
    FOR EACH ROW EXECUTE FUNCTION pack_legacy_bytes_changed('adopted_tar_gz', 'adopted_tree_digest');
CREATE TRIGGER pack_tarballs_legacy_bytes BEFORE UPDATE ON pack_tarballs
    FOR EACH ROW EXECUTE FUNCTION pack_legacy_bytes_changed('tar_gz', 'tree_digest');
