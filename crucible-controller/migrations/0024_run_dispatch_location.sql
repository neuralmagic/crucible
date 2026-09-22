-- 0024_run_dispatch_location.sql — where a run's engine actually ran.
--
-- `runs.dispatch` (0014) says which substrate started the run; it does not say which cluster. The
-- work pod carries that (`work_pods.cluster`), but the live relay and the run APIs read `runs`, so
-- they had to assume the hub. A spoke run's progress page asked the hub for a pod that was never
-- there.
--
-- `cluster` is the reserved name `hub` or a target resolvable at dispatch; `namespace` is what the
-- cluster resolved the loop pod into (a spoke's comes from its kubeconfig context, so it is not
-- derivable from controller config). NULL namespace is "not recorded" — a local-mode run has no
-- namespace, and a row written before this column existed has none until the backfill runs.
ALTER TABLE runs ADD COLUMN cluster TEXT NOT NULL DEFAULT 'hub';
ALTER TABLE runs ADD COLUMN namespace TEXT;

-- The relay's lookup is by run_id (the primary key); this index serves the operator question
-- "what is still running on the spoke I am about to drain".
CREATE INDEX runs_cluster_status ON runs (cluster, status);
