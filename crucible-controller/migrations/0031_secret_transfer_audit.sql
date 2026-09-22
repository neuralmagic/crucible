-- 0031_secret_transfer_audit.sql — an audit action for handing a secret to another principal.
-- The row's owner and vault_path change together; the trail records where it came from.
ALTER TABLE secret_audit DROP CONSTRAINT secret_audit_action_check;
ALTER TABLE secret_audit ADD CONSTRAINT secret_audit_action_check
    CHECK (action IN ('register', 'rotate', 'bind', 'unbind', 'delete',
                      'grant', 'hub_read', 'redeem', 'transfer'));
