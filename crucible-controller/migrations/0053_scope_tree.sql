-- 0053_scope_tree.sql — the stored pack tree a scope froze (RFC-0002:C-PACK-BASE). Build planning
-- and dispatch read this tree, not whatever the issue's pack_tarballs row holds later. NULL for a
-- scope frozen before the column existed, which reads the issue's pack_tarballs row as before.
ALTER TABLE scopes ADD COLUMN tree_digest TEXT REFERENCES pack_trees(digest);
