-- 0053_task_repairs.sql — the repair turns an agent task's attempt took (contract 1.14.0), as the
-- contract's TaskRepair array: label, round, of, cost, and the masked notes each turn was given.
-- Empty for a task that took none and for rows written before the engine reported them.
ALTER TABLE run_task_results ADD COLUMN repairs JSONB NOT NULL DEFAULT '[]'::jsonb;
