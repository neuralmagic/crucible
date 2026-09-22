-- 0034_pack_exposure.sql — the declared exposure (RFC-0001:C-EXPOSURE) of the exact pack revision
-- an approval covers and a launch runs. NULL is absent-legacy, not "discloses nothing".
ALTER TABLE playbooks ADD COLUMN exposure JSONB;
ALTER TABLE playbooks ADD COLUMN exposure_digest TEXT;

ALTER TABLE pack_imports ADD COLUMN exposure JSONB;
ALTER TABLE pack_imports ADD COLUMN exposure_digest TEXT;

ALTER TABLE scopes ADD COLUMN exposure JSONB;
ALTER TABLE scopes ADD COLUMN exposure_digest TEXT;
-- An approval binds to one exposure, so the row records which one it signed for.
ALTER TABLE scopes ADD COLUMN approved_exposure_digest TEXT;

-- A draft's content changes on every save, so a launch of one carries its own recomputed exposure
-- rather than reading it back off the draft version.
ALTER TABLE playbook_launches ADD COLUMN exposure JSONB;
ALTER TABLE playbook_launches ADD COLUMN exposure_digest TEXT;
