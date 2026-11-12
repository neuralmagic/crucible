-- 0086_run_git_token_minted_at.sql — when the token a run's Secret holds was minted, so the leader
-- re-mints by the token's age and a controller restart cannot skip one (ADR-0051). Rows written
-- before this column existed have none; the refresh re-mints them on its next sweep.
ALTER TABLE run_git_tokens ADD COLUMN minted_at TEXT;
