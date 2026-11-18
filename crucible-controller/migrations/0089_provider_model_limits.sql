-- 0089_provider_model_limits.sql — what each of a provider's models can take and give, in tokens.
--
-- A JSON object keyed by model id: {"<model>": {"context_window": N, "max_output_tokens": M}}.
-- A model with no entry leaves its limits to the harness, which is what every model did before.
ALTER TABLE model_providers ADD COLUMN model_limits JSONB NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE model_providers ADD CONSTRAINT model_providers_model_limits_object CHECK (
    jsonb_typeof(model_limits) = 'object'
);
