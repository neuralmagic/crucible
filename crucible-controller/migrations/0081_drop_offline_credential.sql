-- 0081_drop_offline_credential.sql — standing launches fire on the owner snapshot their last save
-- took, and every credential reads groups from the owner's last sign-in. The per-user offline
-- refresh token, the snapshot TTL, and the fire-time refresh state are gone.
DROP TABLE user_credentials;

ALTER TABLE playbook_standing_launches
    DROP COLUMN owner_groups_at,
    DROP COLUMN owner_signin_required,
    DROP COLUMN owner_refresh_error,
    DROP COLUMN owner_refresh_at;

-- Firings the TTL parked can never resume; a fresh firing replaces each.
UPDATE issues
   SET status = 'done', parked_reason = NULL, parked_by = NULL, updated_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
 WHERE status = 'parked' AND parked_reason LIKE '% owner snapshot is stale: %';
