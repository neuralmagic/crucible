-- 0087_tool_blobs.sql — the tool downloads stored packs lock, by the sha256 the lock pins.
-- `pack_tree_tools` keeps a download for as long as a stored pack tree's lock names it, and
-- `run_tools` lists the downloads a dispatched run's token may fetch.

CREATE TABLE tool_blobs (
    sha256     TEXT PRIMARY KEY,
    bytes      BYTEA NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE pack_tree_tools (
    tree_digest TEXT NOT NULL REFERENCES pack_trees(digest) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    platform    TEXT NOT NULL,
    sha256      TEXT NOT NULL REFERENCES tool_blobs(sha256),
    PRIMARY KEY (tree_digest, name, platform)
);
CREATE INDEX pack_tree_tools_sha256 ON pack_tree_tools (sha256);

CREATE TABLE run_tools (
    run_id TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    PRIMARY KEY (run_id, sha256)
);
