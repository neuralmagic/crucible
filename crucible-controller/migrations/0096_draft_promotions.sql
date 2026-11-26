CREATE TABLE draft_promotions (
    draft_id     TEXT PRIMARY KEY REFERENCES playbook_drafts(id) ON DELETE CASCADE,
    forge        TEXT NOT NULL CHECK (forge IN ('github', 'gitlab')),
    host         TEXT NOT NULL,
    project      TEXT NOT NULL,
    path         TEXT NOT NULL,
    base_branch  TEXT NOT NULL,
    branch       TEXT NOT NULL,
    number       BIGINT NOT NULL,
    url          TEXT NOT NULL,
    version      BIGINT NOT NULL,
    digest       TEXT NOT NULL,
    author       TEXT NOT NULL,
    promoted_by  TEXT NOT NULL,
    state        TEXT NOT NULL CHECK (state IN ('open', 'merged', 'closed', 'imported', 'import_failed')),
    note         TEXT NOT NULL DEFAULT '',
    etag         TEXT,
    next_poll_at TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE INDEX draft_promotions_followed ON draft_promotions (next_poll_at)
    WHERE state IN ('open', 'merged');
