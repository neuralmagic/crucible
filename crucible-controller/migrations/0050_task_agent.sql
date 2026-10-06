-- 0050_task_agent.sql — what an agent task's attempts resolved to (contract 1.13.0): the harness,
-- the model and the reasoning effort, after the task's own knobs were applied over the run's.
-- NULL for a task that runs no agent and for rows written before the engine reported it.
ALTER TABLE run_task_results ADD COLUMN agent JSONB;
