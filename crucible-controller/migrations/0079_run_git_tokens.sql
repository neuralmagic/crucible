-- 0079_run_git_tokens.sql — the minted GitHub App token a run pod was delivered, so the leader
-- can re-mint it into that pod's Secret before it lapses (ADR-0051). Written at dispatch beside
-- the Secret; the refresh reads only this, never anything a cluster object claims about itself.
CREATE TABLE run_git_tokens (
    pod_name   TEXT PRIMARY KEY REFERENCES work_pods(pod_name) ON DELETE CASCADE,
    cluster    TEXT NOT NULL,
    namespace  TEXT NOT NULL,
    secret_id  TEXT NOT NULL,
    secret_key TEXT NOT NULL,
    created_at TEXT NOT NULL
);
