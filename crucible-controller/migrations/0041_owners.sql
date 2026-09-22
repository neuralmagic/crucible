-- 0041_owners.sql — an owner principal on every governed root resource (RFC-0003 C-OWNERSHIP),
-- backfilled by the precedence C-COMPATIBILITY gives: the recorded creator when it spells a login,
-- else the platform administrators team. Standing launches keep `owner_principal`, which the
-- fire-time refresh already reads.
ALTER TABLE playbooks       ADD COLUMN owner TEXT NOT NULL DEFAULT 'team:platform-administrators';
ALTER TABLE playbook_drafts ADD COLUMN owner TEXT NOT NULL DEFAULT 'team:platform-administrators';
ALTER TABLE pack_imports    ADD COLUMN owner TEXT NOT NULL DEFAULT 'team:platform-administrators';
ALTER TABLE model_providers ADD COLUMN owner TEXT NOT NULL DEFAULT 'team:platform-administrators';
ALTER TABLE policy_sets     ADD COLUMN owner TEXT NOT NULL DEFAULT 'team:platform-administrators';

UPDATE playbooks SET owner = 'user:' || lower(trim(created_by))
 WHERE created_by IS NOT NULL AND trim(created_by) ~ '^[A-Za-z0-9._@-]+$';
UPDATE playbook_drafts SET owner = 'user:' || lower(trim(created_by))
 WHERE created_by IS NOT NULL AND trim(created_by) ~ '^[A-Za-z0-9._@-]+$';
UPDATE pack_imports SET owner = 'user:' || lower(trim(proposed_by))
 WHERE proposed_by IS NOT NULL AND trim(proposed_by) ~ '^[A-Za-z0-9._@-]+$';
UPDATE model_providers SET owner = 'user:' || lower(trim(created_by))
 WHERE trim(created_by) ~ '^[A-Za-z0-9._@-]+$';

CREATE INDEX playbooks_owner       ON playbooks (owner);
CREATE INDEX playbook_drafts_owner ON playbook_drafts (owner);
CREATE INDEX pack_imports_owner    ON pack_imports (owner);
CREATE INDEX model_providers_owner ON model_providers (owner);

-- The owner principal of the launch at the time of the spend (RFC-0003 C-HIERARCHY): a later
-- transfer changes what the run inherits, never this.
ALTER TABLE runs ADD COLUMN attributed_to TEXT;
