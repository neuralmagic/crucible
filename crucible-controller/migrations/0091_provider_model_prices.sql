-- 0091_provider_model_prices.sql — what each of a provider's models costs, in USD per million tokens.
--
-- A JSON object keyed by model id:
-- {"<model>": {"input": N, "output": N, "cache_read": N, "cache_write": N}}.
-- A model with no entry is charged the provider's price, which is what every model was before.
ALTER TABLE model_providers ADD COLUMN model_prices JSONB NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE model_providers ADD CONSTRAINT model_providers_model_prices_object CHECK (
    jsonb_typeof(model_prices) = 'object'
);
