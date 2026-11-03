-- 0075_refuse_hermes_harness.sql — the hermes harness is gone from the engine, so a provider can
-- no longer run its models under it. A row that named it falls back to the default for its kind,
-- which for the one kind that offered hermes (vertex) is claude: the same models on the same
-- credential, a different CLI. From here the column admits only the harnesses the engine has.
UPDATE model_providers SET harness = NULL WHERE harness = 'hermes';
ALTER TABLE model_providers ADD CONSTRAINT model_providers_harness_known
    CHECK (harness IS NULL OR harness IN ('claude', 'codex', 'opencode', 'pi'));
