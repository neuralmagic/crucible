-- 0059_run_sandbox_names_not_global.sql — a sandbox name is unique only where its Sandbox resource
-- lives: two clusters may each run a sandbox with the same name, and a finished run's binding keeps
-- the name it had. Bind checks the name against live bindings on the same cluster instead.
DROP INDEX run_sandboxes_name;
