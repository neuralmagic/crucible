CREATE TABLE forge_links (
    owner      TEXT NOT NULL,
    forge      TEXT NOT NULL CHECK (forge IN ('github', 'gitlab')),
    host       TEXT NOT NULL,
    account_id BIGINT NOT NULL,
    login      TEXT NOT NULL,
    linked_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (owner, forge, host),
    UNIQUE (forge, host, account_id)
);
