-- 0047_webhooks.sql — the webhook trigger: a 1:1 sidecar on a standing launch whose firings a
-- sender outside the controller requests by POSTing to its delivery address. The listener verifies
-- and records each delivery here; the trigger sweep settles every recorded delivery, in order, to
-- exactly one outcome. Deleting the standing launch deletes the webhook, its deliveries, and its
-- consumed deduplication keys.
ALTER TABLE playbook_standing_launches DROP CONSTRAINT playbook_standing_launches_trigger_check;
ALTER TABLE playbook_standing_launches
    ADD CONSTRAINT playbook_standing_launches_trigger_check
    CHECK (trigger IN ('schedule', 'deferred', 'watch', 'webhook'));

ALTER TABLE playbook_launches DROP CONSTRAINT playbook_launches_origin_check;
ALTER TABLE playbook_launches
    ADD CONSTRAINT playbook_launches_origin_check
    CHECK (origin IN ('manual', 'deferred', 'schedule', 'draft', 'watch', 'webhook'));

-- `token_digest` holds the SHA-256 of a path or header token; `secret_sealed` holds an HMAC
-- secret sealed under the credential key named by `secret_key_id`. Exactly one is set, by
-- verifier. `derive` maps a playbook param name to the CEL expression that derives it.
-- `withheld_headers` names every header a past verifier of this webhook read, which a recorded
-- delivery never keeps.
CREATE TABLE playbook_webhooks (
    id                    TEXT PRIMARY KEY
                              REFERENCES playbook_standing_launches(id) ON DELETE CASCADE,
    verifier              TEXT NOT NULL
                              CHECK (verifier IN ('path_token', 'header_token', 'hmac_sha256',
                                                  'standard_webhooks')),
    header                TEXT,
    token_digest          TEXT,
    secret_sealed         TEXT,
    secret_key_id         TEXT,
    filter                TEXT NOT NULL,
    dedupe                TEXT NOT NULL,
    derive                JSONB NOT NULL,
    max_launches_per_hour INTEGER NOT NULL CHECK (max_launches_per_hour > 0),
    retention_days        INTEGER NOT NULL CHECK (retention_days > 0),
    withheld_headers      JSONB NOT NULL DEFAULT '[]',
    last_delivery_at      TEXT,
    CONSTRAINT playbook_webhooks_secret_shape CHECK (
        (verifier IN ('path_token', 'header_token')
         AND token_digest IS NOT NULL AND secret_sealed IS NULL AND secret_key_id IS NULL)
        OR (verifier IN ('hmac_sha256', 'standard_webhooks')
            AND token_digest IS NULL AND secret_sealed IS NOT NULL AND secret_key_id IS NOT NULL)
    ),
    CONSTRAINT playbook_webhooks_header_shape CHECK (
        (verifier IN ('header_token', 'hmac_sha256')) = (header IS NOT NULL)
    )
);

-- One row per recorded delivery. `id` is a UUIDv7, so id order is arrival order. `outcome` is
-- NULL until the sweep settles the delivery.
CREATE TABLE playbook_webhook_deliveries (
    id          TEXT PRIMARY KEY,
    webhook_id  TEXT NOT NULL REFERENCES playbook_webhooks(id) ON DELETE CASCADE,
    received_at TEXT NOT NULL,
    headers     JSONB NOT NULL,
    body        BYTEA NOT NULL,
    outcome     TEXT CHECK (outcome IN ('launched', 'filtered', 'duplicate', 'throttled',
                                        'failed')),
    reason      TEXT,
    dedupe_key  TEXT,
    launch_key  TEXT,
    settled_at  TEXT,
    CONSTRAINT playbook_webhook_deliveries_settled_shape CHECK (
        (outcome IS NULL) = (settled_at IS NULL)
    )
);

CREATE INDEX playbook_webhook_deliveries_pending
    ON playbook_webhook_deliveries (webhook_id, id) WHERE outcome IS NULL;
CREATE INDEX playbook_webhook_deliveries_settled
    ON playbook_webhook_deliveries (webhook_id, settled_at) WHERE outcome IS NOT NULL;

-- A key a launched delivery consumed. It outlives the delivery record: retention never releases a
-- key, so a sender retry or a replay of a pruned delivery still settles duplicate.
CREATE TABLE playbook_webhook_keys (
    webhook_id  TEXT NOT NULL REFERENCES playbook_webhooks(id) ON DELETE CASCADE,
    dedupe_key  TEXT NOT NULL,
    delivery_id TEXT NOT NULL,
    launch_key  TEXT NOT NULL,
    consumed_at TEXT NOT NULL,
    PRIMARY KEY (webhook_id, dedupe_key)
);

CREATE INDEX playbook_webhook_keys_consumed ON playbook_webhook_keys (webhook_id, consumed_at);
