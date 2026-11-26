CREATE TABLE github_links (
    owner        TEXT PRIMARY KEY,
    github_id    BIGINT NOT NULL UNIQUE,
    github_login TEXT NOT NULL,
    linked_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
