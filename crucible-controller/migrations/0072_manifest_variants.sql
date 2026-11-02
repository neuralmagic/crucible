-- 0072_manifest_variants.sql — the manifest variant (`[variant.<name>]`) a playbook runs. NULL is
-- the base manifest.
ALTER TABLE playbooks ADD COLUMN variant TEXT;
-- The registry row's variant as of the launch, copied with the pack.
ALTER TABLE playbook_launches ADD COLUMN variant TEXT;
-- The variant an import previews and registers.
ALTER TABLE pack_imports ADD COLUMN variant TEXT;
-- The variant a standing launch adopted with its pack.
ALTER TABLE playbook_standing_launches ADD COLUMN adopted_variant TEXT;
