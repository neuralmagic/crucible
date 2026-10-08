-- 0055_run_sandboxes.sql — the OpenShell sandboxes each run pod created, keyed by the gateway's
-- sandbox id (the last segment of the sandbox supervisor's SPIFFE ID). The agent token endpoint
-- resolves a presented JWT-SVID to its run through this table. A run pod can hold several
-- sandboxes at once (fanned-out agent tasks).
CREATE TABLE run_sandboxes (
    sandbox_id TEXT PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    pod_name   TEXT NOT NULL REFERENCES work_pods(pod_name) ON DELETE CASCADE,
    bound_at   TEXT NOT NULL
);
CREATE INDEX run_sandboxes_pod ON run_sandboxes (pod_name);
