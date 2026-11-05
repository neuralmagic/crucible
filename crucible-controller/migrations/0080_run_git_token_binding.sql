-- 0080_run_git_token_binding.sql — the binding a run's GitHub token was delivered through, so the
-- refresh can stop the moment that binding is gone instead of minting for the rest of the run.
-- Rows written before this column existed have none; the refresh drops them.
ALTER TABLE run_git_tokens ADD COLUMN binding_id TEXT;
