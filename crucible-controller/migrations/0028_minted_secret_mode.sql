-- 0028_minted_secret_mode.sql — a third storage mode for credentials that are issued per dispatch
-- rather than stored. A minted secret has no bytes at rest: `vault_path` holds the minter's URI
-- (`mint://github-app`), `current_version` stays NULL, and the value exists only in the per-run
-- Secret the hub writes at dispatch and the cluster collects with the pod.
ALTER TABLE secrets DROP CONSTRAINT secrets_mode_check;
ALTER TABLE secrets ADD CONSTRAINT secrets_mode_check
    CHECK (mode IN ('managed', 'reference', 'minted'));
