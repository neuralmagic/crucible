-- 0066_run_agent_tools.sql — the MCP servers each run's agent was granted at dispatch: the server,
-- the cluster it runs on, its URL (the audience of the run's tokens for it), the username it acts
-- as there, the scopes the run's tokens ask for, and its limits (a push repository, a namespace).
-- Written before the run row exists, so run_id carries no foreign key.
CREATE TABLE run_agent_tools (
    run_id     TEXT NOT NULL,
    server     TEXT NOT NULL,
    cluster    TEXT NOT NULL,
    url        TEXT NOT NULL,
    kube_user  TEXT,
    scopes     JSONB NOT NULL DEFAULT '[]',
    limits     JSONB NOT NULL DEFAULT '{}',
    granted_at TEXT NOT NULL,
    PRIMARY KEY (run_id, server)
);
