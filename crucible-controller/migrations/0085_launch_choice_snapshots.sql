-- 0085_launch_choice_snapshots.sql — the producer snapshot each choices_from param was offered
-- from (ADR-0074), `{param: {run_id, digest}}`, so a launch shows which list it was validated
-- against after the producer runs again.
ALTER TABLE playbook_launches ADD COLUMN choice_snapshots JSONB NOT NULL DEFAULT '{}'::jsonb;
