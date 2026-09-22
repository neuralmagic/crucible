-- 0035_task_blocked.sql — why a task was never dispatched, as the contract's TaskBlocked object
-- (contract 1.3.0) beside the note that renders it. NULL for every other status and for rows
-- written before the field existed.
ALTER TABLE run_task_results ADD COLUMN blocked JSONB;
