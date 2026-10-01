-- 0049_task_links.sql — the external results a task reported, as the contract's ExternalLink
-- array (contract 1.12.0): the url plus the provider, kind and label the engine read off it.
-- Empty for a task that declared no `link`/`links` field and for rows written before it existed.
ALTER TABLE run_task_results ADD COLUMN links JSONB NOT NULL DEFAULT '[]'::jsonb;
