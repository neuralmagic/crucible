-- 0052_pack_alias_bytes_keep_tree.sql — pack bytes aliased to a tree keep it (RFC-0002:C-PACK-BASE).

-- When the bytes change, keep the tree if the new bytes are its tarball or an encoding of it that
-- conversion recorded in pack_digest_aliases; otherwise clear it.
CREATE OR REPLACE FUNCTION pack_legacy_bytes_changed() RETURNS trigger LANGUAGE plpgsql AS $$
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
