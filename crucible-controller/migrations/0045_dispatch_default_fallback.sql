-- 0045_dispatch_default_fallback.sql — a second provider a default hands its work to when the
-- first cannot take it.
--
-- A default names one provider per scope and workload class. When that provider is disabled or its
-- credential cannot be resolved, every dispatch inheriting the default stops. The fallback is what
-- those dispatches take instead, and the in-process ranking call also tries it when a call to the
-- primary fails. A launch that pinned a provider never falls back: it asked for that service.
--
-- Deregistering the fallback provider clears the fallback rather than the default row, since the
-- primary still stands. A fallback_model left behind without its provider reads as no fallback.
ALTER TABLE dispatch_defaults
    ADD COLUMN fallback_provider_id TEXT REFERENCES model_providers(id) ON DELETE SET NULL;
ALTER TABLE dispatch_defaults ADD COLUMN fallback_model TEXT;
