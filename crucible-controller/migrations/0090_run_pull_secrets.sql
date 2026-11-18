-- 0089_run_pull_secrets.sql — the pull Secrets the controller wrote into a cluster's workload
-- namespaces for one run, from the pack's `[kueue] pull_secrets`. A run's tokens name them from
-- here, and the controller deletes them once the run's pod is no longer running.
CREATE TABLE run_pull_secrets (
    pod_name   TEXT NOT NULL,
    cluster    TEXT NOT NULL,
    namespace  TEXT NOT NULL,
    name       TEXT NOT NULL,
    registry   TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (pod_name, namespace, name)
);
