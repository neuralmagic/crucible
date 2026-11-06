-- 0082_drop_orphan_run_agent_tools.sql — delete run_agent_tools rows whose run is gone, older
-- than a day so a dispatch still writing its row is never raced. New orphans cannot appear:
-- delete_unstarted_run drops the run's rows in the same transaction.
DELETE FROM run_agent_tools
 WHERE run_id NOT IN (SELECT run_id FROM runs)
   AND granted_at < to_char(now() - interval '1 day', 'YYYY-MM-DD"T"HH24:MI:SS"Z"');
