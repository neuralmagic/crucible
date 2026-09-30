-- 0048_run_names.sql — a run's display name. A name is reserved before the run's pod is named,
-- ahead of the run row, so two launches can never be handed the same one.
CREATE TABLE run_names (
    name TEXT PRIMARY KEY
);

ALTER TABLE runs ADD COLUMN name TEXT UNIQUE REFERENCES run_names(name);
