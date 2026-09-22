-- 0030_inference_api_key_secret_kind.sql — a kind for the API keys the provider registry points at.
-- An inference key is an opaque string like any other, but it is spent rather than presented: the
-- kind is what lets a provider registration refuse a secret that was registered for something else,
-- and what keeps the key broker-only no matter what visibility a registration asks for.
ALTER TABLE secrets DROP CONSTRAINT secrets_kind_check;
ALTER TABLE secrets ADD CONSTRAINT secrets_kind_check
    CHECK (kind IN ('opaque', 'file', 'registry_authfile', 'kubeconfig', 'inference_api_key'));
