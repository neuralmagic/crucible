-- 0058_provider_price.sql — what a provider's agent tokens cost, in USD per million tokens.
--
-- NULL on every column prices the provider's runs by the engine's vendor table, which bills a
-- model no vendor sells at nothing. Set, the four rates price every agent turn of a run on this
-- provider and replace the cost the harness reports.
ALTER TABLE model_providers ADD COLUMN price_input DOUBLE PRECISION;
ALTER TABLE model_providers ADD COLUMN price_output DOUBLE PRECISION;
ALTER TABLE model_providers ADD COLUMN price_cache_read DOUBLE PRECISION;
ALTER TABLE model_providers ADD COLUMN price_cache_write DOUBLE PRECISION;
ALTER TABLE model_providers ADD CONSTRAINT model_providers_price_whole CHECK (
    num_nulls(price_input, price_output, price_cache_read, price_cache_write) IN (0, 4)
);
ALTER TABLE model_providers ADD CONSTRAINT model_providers_price_rates CHECK (
    price_input >= 0 AND price_input < 'Infinity'
    AND price_output >= 0 AND price_output < 'Infinity'
    AND price_cache_read >= 0 AND price_cache_read < 'Infinity'
    AND price_cache_write >= 0 AND price_cache_write < 'Infinity'
);
