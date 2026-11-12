-- 0086_run_git_token_minted_at.sql — when the token in a run's Secret was minted (ADR-0051).
ALTER TABLE run_git_tokens ADD COLUMN minted_at TEXT;
