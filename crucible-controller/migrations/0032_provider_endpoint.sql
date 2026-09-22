-- 0032_provider_endpoint.sql — a provider that is reached at an address the registry names.
--
-- A `custom` provider is an OpenAI- or Anthropic-speaking service somewhere the operator runs it
-- (an on-prem vLLM, a proxy). Its kind cannot say which harness runs or which key it takes, so the
-- protocol does: `messages` is the Anthropic Messages API and runs Claude Code, `chat_completions`
-- and `responses` are the OpenAI APIs and run Codex. The endpoint and the protocol are the
-- non-secret half of the provider, the same split OpenShell keeps between a provider's `config`
-- map and its `credentials` map; the key stays in the secrets registry.
ALTER TABLE model_providers ADD COLUMN endpoint TEXT;
-- 'messages' | 'chat_completions' | 'responses'.
ALTER TABLE model_providers ADD COLUMN protocol TEXT;
ALTER TABLE model_providers ADD CONSTRAINT model_providers_endpoint
    CHECK ((kind = 'custom') = (endpoint IS NOT NULL AND protocol IS NOT NULL));
