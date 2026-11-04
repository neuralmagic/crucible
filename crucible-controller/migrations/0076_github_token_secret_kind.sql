-- 0076_github_token_secret_kind.sql — a kind for the GitHub token the hub pushes and opens pull
-- requests with on its owner's behalf. The kind is what lets graduation refuse a secret registered
-- for anything else, and what keeps the token out of every scope binding.
ALTER TABLE secrets DROP CONSTRAINT secrets_kind_check;
ALTER TABLE secrets ADD CONSTRAINT secrets_kind_check
    CHECK (kind IN ('opaque', 'file', 'registry_authfile', 'kubeconfig', 'inference_api_key',
                    'github_token'));
