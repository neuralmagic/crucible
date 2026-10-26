-- 0069_task_output.sql — what a task attempt emitted, as the session's task_result `output`: a
-- passing attempt's product, or the record a failing one left (an evaluate's verdict). Null for an
-- attempt that emitted nothing and for rows written before it was kept.
ALTER TABLE run_task_results ADD COLUMN output JSONB;
