-- 0077_drop_mlflow_exports_and_secret_grants.sql — tables nothing reads or writes. The MLflow
-- exporter that kept `mlflow_exports` is gone, and run secrets reach a pod as a mounted Secret
-- (ADR-0036), so no dispatch writes the per-dispatch grants 0019 created.
DROP TABLE mlflow_exports;
DROP TABLE secret_grant_items;
DROP TABLE secret_grants;
