-- 0086_pack_tree_deps.sql — the dependency trees a stored pack's crucible.lock pins. A row keeps
-- its dependency tree from collection for as long as the pack tree that names it is stored.

CREATE TABLE pack_tree_deps (
    tree_digest TEXT NOT NULL REFERENCES pack_trees(digest) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    dep_digest  TEXT NOT NULL REFERENCES pack_trees(digest),
    PRIMARY KEY (tree_digest, name)
);
CREATE INDEX pack_tree_deps_dep_digest ON pack_tree_deps (dep_digest);
