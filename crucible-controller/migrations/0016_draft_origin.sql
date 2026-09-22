-- 0016_draft_origin.sql — what a draft is based on. `template_playbook` only ever recorded the
-- registered pack a skeleton-or-template create copied; a draft opened from an import recorded
-- nothing, and neither carried the rev, so the studio could not tell a draft from a moved pin or
-- pre-fill graduation with the place its pack came from. The origin replaces it: a registered pack
-- at the rev it was seeded from, or the import row whose frozen tarball seeded it.
ALTER TABLE playbook_drafts ADD COLUMN origin_playbook TEXT;
ALTER TABLE playbook_drafts ADD COLUMN origin_rev TEXT;
ALTER TABLE playbook_drafts ADD COLUMN origin_import TEXT;

UPDATE playbook_drafts d
   SET origin_playbook = d.template_playbook,
       origin_rev = (SELECT p.rev FROM playbooks p WHERE p.id = d.template_playbook)
 WHERE d.template_playbook IS NOT NULL;
UPDATE playbook_drafts d
   SET origin_import = i.id
  FROM pack_imports i
 WHERE i.draft_id = d.id AND d.template_playbook IS NULL;

ALTER TABLE playbook_drafts DROP COLUMN template_playbook;
ALTER TABLE playbook_drafts
    ADD CONSTRAINT playbook_drafts_origin_check
    CHECK (origin_playbook IS NULL OR origin_import IS NULL);
