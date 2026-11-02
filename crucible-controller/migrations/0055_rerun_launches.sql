-- 0055_rerun_launches.sql — a launch that reruns a finished run (RFC-0002:C-PLAYBOOK-RERUN): the run
-- it reruns and the tasks it dispatches again. NULL and empty on every other launch.
ALTER TABLE playbook_launches ADD COLUMN rerun_of TEXT;
ALTER TABLE playbook_launches ADD COLUMN rerun_from JSONB NOT NULL DEFAULT '[]'::jsonb;
